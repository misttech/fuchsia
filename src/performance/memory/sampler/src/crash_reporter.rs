// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! This module provides an asynchronous service that can be used to
//! regularly file crash reports via the `CrashReporter` FIDL API.

use std::pin::Pin;

use anyhow::{Context, Error, format_err};
use fidl_fuchsia_feedback::{
    Attachment, CrashReport, CrashReporterProxy, CrashReportingProductRegisterMarker,
    MAX_NUM_ATTACHMENTS_PER_CRASH_REPORT,
};
use fidl_fuchsia_mem::Buffer;
use fuchsia_async::{Task, Timer};
use fuchsia_component::client::connect_to_protocol;
use futures::channel::mpsc;
use futures::stream::Peekable;
use futures::{FutureExt, StreamExt, TryFutureExt};
use itertools::Itertools;
use zx::Vmo;

const CRASH_PRODUCT_NAME: &str = "FuchsiaHeapProfile";
const CRASH_PROGRAM_NAME: &str = "memory_sampler";
const CRASH_SIGNATURE: &str = "fuchsia-memory-profile";
/// The extension given to attached profiles.
///
/// Profiles are `gzip`-compressed protocol buffers; see
/// `profile_builder::profile_to_vmo`.
const PROFILE_ATTACHMENT_EXTENSION: &str = ".pb.gz";
/// The number of profiles held in the queue awaiting filing.
///
/// A report can carry at most `MAX_NUM_ATTACHMENTS_PER_CRASH_REPORT`
/// profiles, so holding that many lets a single report be filled
/// completely; below it, capacity is wasted.
///
/// This is deliberately a separate constant from the number of
/// attachments per report, even though the two are currently equal:
/// this one bounds memory, whereas the other is a property of the
/// crash reporting API. It is sound to hold more profiles than a
/// single report can carry, since the surplus is filed by the
/// following report; what constrains it is the memory held by the
/// queued profiles, not the capacity of a report.
///
/// Note: `futures::mpsc::channel` grants every sender a slot in
/// addition to this shared buffer, so the effective bound is this
/// value plus the number of connected processes.
const MAX_CONCURRENT_PROFILES: usize = MAX_NUM_ATTACHMENTS_PER_CRASH_REPORT as usize;
/// Upper bound on the total VMO footprint of profiles held in memory
/// for the next crash report (2 MiB).
///
/// Once the pending report reaches this budget, the crash reporter stops
/// draining the channel until the current reporting interval elapses and
/// the pending report is filed. This bounds `memory_sampler`'s memory
/// footprint and keeps the size of the resulting crash report reasonable.
const MAX_PENDING_REPORT_BYTES: u64 = 2 * 1024 * 1024;
const MIN_DURATION_BETWEEN_SNAPSHOTS_HOURS: zx::MonotonicDuration =
    zx::MonotonicDuration::from_hours(1);

#[derive(PartialEq, Debug)]
pub enum ProfileReport {
    Partial { provider_id: u64, process_name: String, profile: Vmo, size: u64, iteration: usize },
    Final { provider_id: u64, process_name: String, profile: Vmo, size: u64 },
}

impl ProfileReport {
    pub fn provider_id(&self) -> u64 {
        match self {
            ProfileReport::Partial { provider_id, .. } => *provider_id,
            ProfileReport::Final { provider_id, .. } => *provider_id,
        }
    }

    pub fn process_name(&self) -> &str {
        match self {
            ProfileReport::Partial { process_name, .. } => process_name,
            ProfileReport::Final { process_name, .. } => process_name,
        }
    }

    pub fn size(&self) -> u64 {
        match self {
            ProfileReport::Partial { size, .. } => *size,
            ProfileReport::Final { size, .. } => *size,
        }
    }

    /// Returns the page-rounded physical memory footprint of the backing VMO
    /// (at least one page per VMO).
    pub fn vmo_footprint(&self) -> u64 {
        let page_size = zx::system_get_page_size() as u64;
        self.size().max(1).next_multiple_of(page_size)
    }
}

/// Accumulates and coalesces profiles awaiting the next crash report filing.
#[derive(Default)]
struct PendingReport {
    profiles: Vec<ProfileReport>,
    total_bytes: u64,
}

impl PendingReport {
    fn is_empty(&self) -> bool {
        self.profiles.is_empty()
    }

    fn is_budget_exhausted(&self) -> bool {
        self.total_bytes >= MAX_PENDING_REPORT_BYTES
    }

    fn find_coalesce_target(&self, incoming: &ProfileReport) -> Option<usize> {
        self.profiles.iter().rposition(|existing| {
            matches!(existing, ProfileReport::Partial { .. })
                && existing.provider_id() == incoming.provider_id()
        })
    }

    fn can_accept(&self, incoming: &ProfileReport) -> bool {
        if !self.is_empty()
            && self.total_bytes.saturating_add(incoming.vmo_footprint()) > MAX_PENDING_REPORT_BYTES
        {
            return false;
        }
        if self.find_coalesce_target(incoming).is_some() {
            return true;
        }
        self.profiles.len() < MAX_NUM_ATTACHMENTS_PER_CRASH_REPORT as usize
    }

    fn push_or_coalesce(&mut self, incoming: ProfileReport) {
        if let Some(idx) = self.find_coalesce_target(&incoming) {
            let old = self.profiles.swap_remove(idx);
            let old_footprint = old.vmo_footprint();
            let merged = match old.merge(incoming) {
                Ok(merged) => merged,
                Err((fallback, err)) => {
                    log::warn!(
                        "Failed to coalesce profile reports, keeping fallback report: {:?}",
                        err
                    );
                    fallback
                }
            };
            let merged_footprint = merged.vmo_footprint();
            self.total_bytes = self.total_bytes.saturating_sub(old_footprint) + merged_footprint;
            self.profiles.push(merged);
            let last_idx = self.profiles.len() - 1;
            self.profiles.swap(idx, last_idx);
        } else {
            self.total_bytes += incoming.vmo_footprint();
            self.profiles.push(incoming);
        }
    }

    fn drain_ready(&mut self, rx: &mut Pin<&mut Peekable<mpsc::Receiver<ProfileReport>>>) {
        while let Some(Some(candidate)) = rx.as_mut().peek().now_or_never() {
            if !self.can_accept(candidate) {
                break;
            }
            let item = rx.as_mut().next().now_or_never().flatten().unwrap();
            self.push_or_coalesce(item);
        }
    }

    fn take(&mut self) -> Vec<ProfileReport> {
        self.total_bytes = 0;
        std::mem::take(&mut self.profiles)
    }
}

/// Sanitize a process name for use in a `fuchsia.feedback/Attachment` key,
/// which forbids spaces, forward slashes, and non-printable ASCII characters.
fn sanitize_for_attachment_key(name: &str) -> String {
    name.chars().map(|c| if c.is_ascii_graphic() && c != '/' { c } else { '_' }).collect()
}

/// File a crash report with the provided profiles attached to it.
///
/// Warning: Care should be taken to not call this function too often,
/// as to not spam the crash reporting system. As a safe baseline, aim
/// to never file more than a crash report per hour.
///
/// Note: Attached final profiles, are grouped by process name, then
/// numbered, and carry the `PROFILE_ATTACHMENT_EXTENSION` extension.
/// Characters disallowed in `fuchsia.feedback/Attachment` keys (spaces,
/// `'/'`, and non-printable ASCII) are replaced with `'_'`.
/// E.g. if there are two final profiles for "process 1" and one profile
/// for "process 2", then they will be named "process_1_final_0.pb.gz",
/// "process_1_final_1.pb.gz" and "process_2_final_0.pb.gz". Partial
/// profiles are numbered by their index; currently no further effort
/// is done to distinguish partial profiles from different processes
/// that share the same name.
///
/// Warning: panics if called with more than
/// `MAX_NUM_ATTACHMENTS_PER_CRASH_REPORT` profiles.
async fn file_report(
    profiles: Vec<ProfileReport>,
    crash_reporter: &CrashReporterProxy,
) -> Result<(), Error> {
    assert!(profiles.len() <= MAX_NUM_ATTACHMENTS_PER_CRASH_REPORT as usize);
    if profiles.is_empty() {
        return Ok(());
    }
    let attachments: Result<Vec<Attachment>, Error> = profiles
        .into_iter()
        .chunk_by(|p| sanitize_for_attachment_key(p.process_name()))
        .into_iter()
        .flat_map(|(_, profiles)| profiles.enumerate())
        .map(|(i, profile_report)| {
            let (key, profile, size) = match profile_report {
                ProfileReport::Final { process_name, profile, size, .. } => (
                    format!(
                        "{}_final_{}{}",
                        sanitize_for_attachment_key(&process_name),
                        i,
                        PROFILE_ATTACHMENT_EXTENSION
                    ),
                    profile,
                    size,
                ),
                ProfileReport::Partial { process_name, profile, size, iteration, .. } => (
                    format!(
                        "{}_partial_{}{}",
                        sanitize_for_attachment_key(&process_name),
                        iteration,
                        PROFILE_ATTACHMENT_EXTENSION
                    ),
                    profile,
                    size,
                ),
            };
            Ok(Attachment { key, value: Buffer { vmo: profile, size } })
        })
        .collect();
    let crash_report = CrashReport {
        program_name: Some(CRASH_PROGRAM_NAME.to_string()),
        crash_signature: Some(CRASH_SIGNATURE.to_string()),
        attachments: Some(attachments?),
        is_fatal: Some(false),
        ..Default::default()
    };
    crash_reporter.file_report(crash_report).await?.map_err(|e| format_err!("{:?}", e))?;
    Ok(())
}

/// Register an appropriate crash product; it will be inherited by
/// every profiling crash report filed with the same program name.
pub async fn register_crash_product() -> Result<(), Error> {
    let crash_reporting_product_register =
        connect_to_protocol::<CrashReportingProductRegisterMarker>()
            .context("Failed to connect to crash reporter service")?;
    let product_config = fidl_fuchsia_feedback::CrashReportingProduct {
        name: Some(CRASH_PRODUCT_NAME.to_string()),
        ..Default::default()
    };
    crash_reporting_product_register.upsert_with_ack(CRASH_PROGRAM_NAME, &product_config).await?;
    Ok(())
}

/// Returns a channel `Sender`, and a task that, when run, will file
/// crash reports pushed to this channel at most once every
/// `MIN_DURATION_BETWEEN_SNAPSHOTS_HOURS` hour.
///
/// While waiting for the next filing window, incoming `Partial` profiles
/// for the same process are coalesced in place (and collapsed into `Final`
/// if the process terminates), bounded by `MAX_PENDING_REPORT_BYTES`.
///
/// Note: the returned `Sender` is bounded: trying to queue a message
/// when it is full will cause an error.
pub fn setup_crash_reporter_task(
    crash_reporter: CrashReporterProxy,
) -> (mpsc::Sender<ProfileReport>, Task<Result<(), Error>>) {
    let (tx, rx) = mpsc::channel::<ProfileReport>(MAX_CONCURRENT_PROFILES);
    let crash_reporter_task = Task::spawn(async move {
        let mut rx = std::pin::pin!(rx.peekable());
        let mut pending = PendingReport::default();

        loop {
            // Time to file a report; drain remaining profiles then proceed.
            if pending.is_empty() {
                if rx.as_mut().peek().await.is_none() {
                    return Ok(());
                }
                pending.drain_ready(&mut rx);
                if pending.is_empty() {
                    return Ok(());
                }
            }

            file_report(pending.take(), &crash_reporter)
                .inspect_err(|e| log::error!("Filing report: {}", e))
                .await?;

            let mut cooldown = std::pin::pin!(
                Timer::new(fuchsia_async::MonotonicInstant::after(
                    MIN_DURATION_BETWEEN_SNAPSHOTS_HOURS,
                ))
                .fuse()
            );

            loop {
                // Monitor the queue for upcoming profiles until the end of the cooldown period.
                pending.drain_ready(&mut rx);
                let blocked_by_head = rx
                    .as_mut()
                    .peek()
                    .now_or_never()
                    .is_some_and(|opt| opt.is_some_and(|p| !pending.can_accept(p)));
                if pending.is_budget_exhausted() || blocked_by_head {
                    cooldown.as_mut().await;
                    break;
                }

                futures::select! {
                    peeked = rx.as_mut().peek().fuse() => {
                        if peeked.is_none() {
                            if !pending.is_empty() {
                                cooldown.as_mut().await;
                            }
                            break;
                        }
                    }
                    () = cooldown => {
                        pending.drain_ready(&mut rx);
                        break;
                    }
                }
            }
        }
    });
    (tx, crash_reporter_task)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::profile_builder::{ProfileBuilder, vmo_to_profile};
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_feedback::{CrashReporterMarker, CrashReporterRequest};
    use futures::try_join;
    use itertools::assert_equal;
    use std::task::Poll;

    fn is_valid_feedback_attachment_key(key: &str) -> bool {
        !key.is_empty()
            && key != "."
            && key != ".."
            && key.chars().all(|c| c.is_ascii_graphic() && c != '/')
    }

    fn handle_crash_reporter_request(request: CrashReporterRequest) -> CrashReport {
        match request {
            CrashReporterRequest::FileReport { report, responder } => {
                if let Some(attachments) = &report.attachments {
                    for attachment in attachments {
                        assert!(
                            is_valid_feedback_attachment_key(&attachment.key),
                            "Invalid fuchsia.feedback attachment key: {:?}",
                            attachment.key
                        );
                    }
                }
                let _ = responder.send(Ok(&Default::default()));
                report
            }
        }
    }

    /// Converts an `Attachment` back to a `ProfileReport`.
    /// Note: this function assumes the following format for `Attachment::key`,
    /// where `<ext>` is `PROFILE_ATTACHMENT_EXTENSION`:
    ///   - `<process_name>_final_<index><ext>` if it is a complete report.
    ///   - `<process_name>_partial_<iteration><ext>` if it is a partial report.
    fn retrieve_profile_from_attachment(attachment: Attachment) -> ProfileReport {
        let Attachment { key, value, .. } = attachment;
        let profile = value.vmo;
        let size = value.size;
        let stem = key
            .strip_suffix(PROFILE_ATTACHMENT_EXTENSION)
            .unwrap_or_else(|| panic!("Attachment name misses the profile extension: {}", key));
        let (prefix_key, index) = stem.rsplit_once('_').unwrap();
        let (process_name, suffix) = prefix_key.rsplit_once('_').unwrap();
        let is_final = match suffix {
            "final" => true,
            "partial" => false,
            _ => panic!("Unexpected attachment name: {}", key),
        };
        if is_final {
            ProfileReport::Final {
                provider_id: 0,
                process_name: process_name.to_string(),
                profile,
                size,
            }
        } else {
            ProfileReport::Partial {
                provider_id: 0,
                process_name: process_name.to_string(),
                profile,
                size,
                iteration: index.parse().unwrap(),
            }
        }
    }

    #[fuchsia::test]
    async fn test_file_report_no_profile() {
        let (client, _) = create_proxy_and_stream::<CrashReporterMarker>();
        let no_profiles = vec![];
        let file_report_future = file_report(no_profiles, &client);
        file_report_future
            .now_or_never()
            .expect("Future was not instantly resolved.")
            .expect("Unexpected error when trying to file an empty report.")
    }

    /// Create a non-empty VMO filled with arbitrary data; can be used
    /// for integrity tests.
    fn create_vmo_with_some_data(size: usize) -> Vmo {
        let data: &[u8] = &vec![42; size][..];
        let vmo = Vmo::create(size as u64).unwrap();
        vmo.write(&data, 0).unwrap();
        vmo
    }

    #[fuchsia::test]
    async fn test_file_report_complete_reports() {
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();

        // Create some profiles with some arbitrary data.
        let size = 42;
        let profiles_metadata = vec![
            // First profile report for a first process.
            ("test_process_1".to_string(), size),
            // First profile report for a second process.
            ("test_process_2".to_string(), size),
            // Second profile report for the first process.
            ("test_process_1".to_string(), size),
        ];
        let profiles = profiles_metadata
            .clone()
            .into_iter()
            .enumerate()
            .map(|(idx, (process_name, size))| ProfileReport::Final {
                provider_id: idx as u64 + 1,
                process_name,
                size,
                profile: create_vmo_with_some_data(size as usize),
            })
            .collect();
        let file_report_future = file_report(profiles, &client);
        let (_, report) = try_join!(
            file_report_future,
            request_stream
                .next()
                .map(|item| Ok(handle_crash_reporter_request(item.unwrap().unwrap())))
        )
        .unwrap();

        assert_eq!(report.program_name, Some(CRASH_PROGRAM_NAME.to_string()));
        assert_eq!(report.crash_signature, Some(CRASH_SIGNATURE.to_string()));
        // Note: spelled out, rather than derived from
        // `PROFILE_ATTACHMENT_EXTENSION`, because the point is to pin
        // the extension that `pprof` tooling expects.
        assert!(
            report.attachments.as_ref().unwrap().iter().all(|a| a.key.ends_with(".pb.gz")),
            "Attachments should be named after the profile format."
        );
        let attachments: Vec<(String, Vec<u8>, u64)> = report
            .attachments
            .unwrap()
            .into_iter()
            .map(retrieve_profile_from_attachment)
            .map(|p| match p {
                ProfileReport::Final { process_name, profile, size, .. } => {
                    (process_name, profile.read_to_vec(0, size).unwrap(), size)
                }
                _ => panic!("Expected a complete report, but received a partial report instead."),
            })
            .collect();
        assert_equal(
            attachments.into_iter(),
            profiles_metadata.into_iter().map(|(process_name, size)| {
                (
                    process_name,
                    create_vmo_with_some_data(size as usize).read_to_vec(0, size).unwrap(),
                    size,
                )
            }),
        );
    }

    #[fuchsia::test]
    async fn test_file_report_partial_reports() {
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();

        // Create some profiles with some arbitrary data.
        let size = 42;
        let profiles_metadata = vec![
            // First profile report for a first process.
            ("test_process_1".to_string(), size),
            // First profile report for a second process.
            ("test_process_2".to_string(), size),
            // Second profile report for the first process.
            ("test_process_1".to_string(), size),
        ];
        let profiles = profiles_metadata
            .clone()
            .into_iter()
            .enumerate()
            .map(|(iteration, (process_name, size))| ProfileReport::Partial {
                provider_id: iteration as u64 + 1,
                process_name,
                size,
                profile: create_vmo_with_some_data(size as usize),
                iteration,
            })
            .collect();

        let file_report_future = file_report(profiles, &client);
        let (_, report) = try_join!(
            file_report_future,
            request_stream
                .next()
                .map(|item| Ok(handle_crash_reporter_request(item.unwrap().unwrap())))
        )
        .unwrap();

        assert_eq!(report.program_name, Some(CRASH_PROGRAM_NAME.to_string()));
        assert_eq!(report.crash_signature, Some(CRASH_SIGNATURE.to_string()));
        assert!(
            report.attachments.as_ref().unwrap().iter().all(|a| a.key.ends_with(".pb.gz")),
            "Attachments should be named after the profile format."
        );
        let attachments: Vec<(String, Vec<u8>, u64, usize)> = report
            .attachments
            .unwrap()
            .into_iter()
            .map(retrieve_profile_from_attachment)
            .map(|p| match p {
                ProfileReport::Partial { process_name, profile, size, iteration, .. } => {
                    (process_name, profile.read_to_vec(0, size).unwrap(), size, iteration)
                }
                _ => panic!("Expected a partial report, but received a complete report instead."),
            })
            .collect();
        assert_equal(
            attachments.into_iter(),
            profiles_metadata.clone().into_iter().enumerate().map(
                |(iteration, (process_name, size))| {
                    (
                        process_name,
                        create_vmo_with_some_data(size as usize).read_to_vec(0, size).unwrap(),
                        size,
                        iteration,
                    )
                },
            ),
        );
    }

    #[test]
    fn test_crash_reporter_task_rate_limiting() {
        let mut executor = fuchsia_async::TestExecutor::new_with_fake_time();
        // Setup the crash reporter task.
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();
        let (mut sender, mut task) = setup_crash_reporter_task(client);

        let size = 42;
        let profile1 = ProfileReport::Final {
            provider_id: 1,
            process_name: "test_process_1".to_string(),
            size,
            profile: create_vmo_with_some_data(size as usize),
        };
        sender.try_send(profile1).unwrap();

        assert!(executor.run_until_stalled(&mut task).is_pending());
        match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => {
                handle_crash_reporter_request(request);
            }
            _ => panic!("Expected profile to be processed immediately"),
        };
        assert!(executor.run_until_stalled(&mut task).is_pending());

        // Send a second profile and assert it is NOT processed immediately.
        let profile2 = ProfileReport::Final {
            provider_id: 2,
            process_name: "test_process_2".to_string(),
            size,
            profile: create_vmo_with_some_data(size as usize),
        };
        sender.try_send(profile2).unwrap();
        assert!(executor.run_until_stalled(&mut task).is_pending());
        assert!(executor.run_until_stalled(&mut request_stream.next()).is_pending());

        // Advance time by 30 minutes, less than the 1 hour minimum.
        executor.set_fake_time(executor.now() + zx::MonotonicDuration::from_minutes(30));
        assert!(!executor.wake_expired_timers());
        assert!(executor.run_until_stalled(&mut task).is_pending());
        assert!(
            executor.run_until_stalled(&mut request_stream.next()).is_pending(),
            "Expected profile to NOT be processed yet"
        );

        // Advance time by another 35 minutes, exceeding the 1 hour minimum.
        executor.set_fake_time(executor.now() + zx::MonotonicDuration::from_minutes(35));
        assert!(executor.wake_expired_timers());

        assert!(executor.run_until_stalled(&mut task).is_pending());
        match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => {
                handle_crash_reporter_request(request);
            }
            _ => panic!("Expected profile to be processed after 1 hour elapsed"),
        }
        assert!(executor.run_until_stalled(&mut task).is_pending());

        // Check that following a long period of inactivity, rate limiting still strictly
        // limits subsequent reports to 1 per hour.
        executor.set_fake_time(executor.now() + zx::MonotonicDuration::from_hours(5));
        assert!(executor.wake_expired_timers());
        assert!(executor.run_until_stalled(&mut task).is_pending());

        let profile3 = ProfileReport::Final {
            provider_id: 3,
            process_name: "test_process_3".to_string(),
            size,
            profile: create_vmo_with_some_data(size as usize),
        };
        sender.try_send(profile3).unwrap();
        assert!(executor.run_until_stalled(&mut task).is_pending());

        // First profile after 5h should be processed immediately.
        match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => {
                handle_crash_reporter_request(request);
            }
            _ => panic!("Expected profile to be processed immediately after 5h of inactivity"),
        }
        assert!(executor.run_until_stalled(&mut task).is_pending());

        // A second profile immediately after should NOT be processed.
        let profile4 = ProfileReport::Final {
            provider_id: 4,
            process_name: "test_process_4".to_string(),
            size,
            profile: create_vmo_with_some_data(size as usize),
        };
        sender.try_send(profile4).unwrap();
        assert!(executor.run_until_stalled(&mut task).is_pending());
        assert!(
            executor.run_until_stalled(&mut request_stream.next()).is_pending(),
            "Expected profile to NOT be processed yet"
        );

        // Advance time by 1 hour and verify it gets processed.
        executor.set_fake_time(executor.now() + zx::MonotonicDuration::from_hours(1));
        assert!(executor.wake_expired_timers());

        assert!(executor.run_until_stalled(&mut task).is_pending());
        match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => {
                handle_crash_reporter_request(request);
            }
            _ => panic!("Expected profile to be processed after another 1 hour elapsed"),
        }
        assert!(executor.run_until_stalled(&mut task).is_pending());
    }

    #[test]
    fn test_report_can_be_filled_to_capacity() {
        let mut executor = fuchsia_async::TestExecutor::new_with_fake_time();
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();
        let (mut sender, mut task) = setup_crash_reporter_task(client);

        // Note: driven by the number of attachments a report can
        // carry, rather than by the depth of the queue, so that this
        // still holds if the queue is later sized to hold more
        // profiles than a single report can take.
        let attachments_per_report = MAX_NUM_ATTACHMENTS_PER_CRASH_REPORT as usize;
        let size = 42;
        for iteration in 0..attachments_per_report {
            sender
                .try_send(ProfileReport::Partial {
                    provider_id: iteration as u64 + 1,
                    process_name: format!("test_process_{iteration}"),
                    size,
                    profile: create_vmo_with_some_data(size as usize),
                    iteration,
                })
                .expect("Queue should hold a full report worth of profiles.");
        }

        assert!(executor.run_until_stalled(&mut task).is_pending());
        let report = match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => handle_crash_reporter_request(request),
            _ => panic!("Expected the queued profiles to be filed."),
        };

        // Every queued profile has to be filed by this report:
        // whatever is left over waits a further hour.
        assert_eq!(attachments_per_report, report.attachments.unwrap().len());
    }

    #[test]
    fn test_partial_profiles_coalesce_within_window() {
        let mut executor = fuchsia_async::TestExecutor::new_with_fake_time();
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();
        let (mut sender, mut task) = setup_crash_reporter_task(client);

        let mut builder = ProfileBuilder::default();
        builder.set_process_info(Some("coalesced_proc".to_string()), std::iter::empty());
        // Address 0x100: allocated and freed in Partial 0 (dead allocation).
        builder.allocate_with_scale(0x100, vec![10, 20], 100, 1.0);
        builder.deallocate(0x100, vec![30, 40]);
        // Address 0x200: allocated in Partial 0 and still alive at Partial 0.
        builder.allocate_with_scale(0x200, vec![50, 60], 200, 1.0);
        let partial_0 = builder.build_partial_profile(0).unwrap();

        // Address 0x200 is freed during Partial 1's window, and 0x300 remains live.
        builder.deallocate(0x200, vec![70, 80]);
        builder.allocate_with_scale(0x300, vec![90, 100], 300, 1.0);
        let partial_1 = builder.build_partial_profile(1).unwrap();

        // Queue both partial profiles before the task runs so they coalesce in the same report.
        sender.try_send(partial_0).unwrap();
        sender.try_send(partial_1).unwrap();

        assert!(executor.run_until_stalled(&mut task).is_pending());
        let report = match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => handle_crash_reporter_request(request),
            _ => panic!("Expected coalesced report to be filed."),
        };

        let attachments = report.attachments.unwrap();
        assert_eq!(1, attachments.len());
        assert_eq!("coalesced_proc_partial_1.pb.gz", attachments[0].key);

        let merged = vmo_to_profile(&attachments[0].value.vmo, attachments[0].value.size).unwrap();
        // Live sample (0x300), two dead allocation sites (0x100 and 0x200), two dealloc sites.
        let live: Vec<_> = merged.sample.iter().filter(|s| s.value[0] > 0).collect();
        assert_eq!(1, live.len());
        assert_eq!(vec![90, 100], live[0].location_id);
        assert_eq!(vec![1, 300, 1, 300, 0, 0], live[0].value);

        let dead_total_bytes: i64 =
            merged.sample.iter().filter(|s| s.value[0] == 0).map(|s| s.value[3]).sum();
        assert_eq!(100 + 200, dead_total_bytes);
    }

    #[test]
    fn test_partial_and_final_profiles_coalesce_into_final() {
        let mut executor = fuchsia_async::TestExecutor::new_with_fake_time();
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();
        let (mut sender, mut task) = setup_crash_reporter_task(client);

        let mut builder = ProfileBuilder::default();
        builder.set_process_info(Some("exiting_proc".to_string()), std::iter::empty());
        builder.allocate_with_scale(0x1, vec![1, 2], 64, 1.0);
        builder.deallocate(0x1, vec![3, 4]);
        builder.allocate_with_scale(0x2, vec![5, 6], 128, 1.0);
        let partial = builder.build_partial_profile(0).unwrap();
        let final_report = builder.build().unwrap();

        sender.try_send(partial).unwrap();
        sender.try_send(final_report).unwrap();

        assert!(executor.run_until_stalled(&mut task).is_pending());
        let report = match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => handle_crash_reporter_request(request),
            _ => panic!("Expected final report to be filed."),
        };

        let attachments = report.attachments.unwrap();
        assert_eq!(1, attachments.len());
        assert_eq!("exiting_proc_final_0.pb.gz", attachments[0].key);

        let merged = vmo_to_profile(&attachments[0].value.vmo, attachments[0].value.size).unwrap();
        // Both the dead allocation from `partial` (64 B) and the live leak from `final` (128 B)
        // must be present without double-counting the live allocation.
        let total_allocated_bytes: i64 = merged.sample.iter().map(|s| s.value[3]).sum();
        assert_eq!(64 + 128, total_allocated_bytes);
        let total_residual_bytes: i64 = merged.sample.iter().map(|s| s.value[1]).sum();
        assert_eq!(128, total_residual_bytes);
    }

    #[test]
    fn test_respawned_process_does_not_coalesce_across_providers() {
        let mut executor = fuchsia_async::TestExecutor::new_with_fake_time();
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();
        let (mut sender, mut task) = setup_crash_reporter_task(client);

        // First instance of "respawning_proc" emits a partial profile and then dies (final).
        let mut instance_1 = ProfileBuilder::default();
        instance_1.set_process_info(Some("respawning_proc".to_string()), std::iter::empty());
        instance_1.allocate_with_scale(0x10, vec![1, 2], 64, 1.0);
        let inst1_partial = instance_1.build_partial_profile(0).unwrap();
        let inst1_final = instance_1.build().unwrap();

        // Second (respawned) instance of "respawning_proc" with a new provider_id emits two
        // partial profiles, while a third concurrent instance with the same name also emits
        // a partial profile.
        let mut instance_2 = ProfileBuilder::default();
        instance_2.set_process_info(Some("respawning_proc".to_string()), std::iter::empty());
        instance_2.allocate_with_scale(0x20, vec![3, 4], 128, 1.0);
        let inst2_partial_0 = instance_2.build_partial_profile(0).unwrap();
        instance_2.allocate_with_scale(0x30, vec![5, 6], 256, 1.0);
        let inst2_partial_1 = instance_2.build_partial_profile(1).unwrap();

        let mut instance_3 = ProfileBuilder::default();
        instance_3.set_process_info(Some("respawning_proc".to_string()), std::iter::empty());
        instance_3.allocate_with_scale(0x40, vec![7, 8], 512, 1.0);
        let inst3_partial_0 = instance_3.build_partial_profile(0).unwrap();

        sender.try_send(inst1_partial).unwrap();
        sender.try_send(inst1_final).unwrap();
        sender.try_send(inst2_partial_0).unwrap();
        sender.try_send(inst3_partial_0).unwrap();
        sender.try_send(inst2_partial_1).unwrap();

        assert!(executor.run_until_stalled(&mut task).is_pending());
        let report = match executor.run_until_stalled(&mut request_stream.next()) {
            Poll::Ready(Some(Ok(request))) => handle_crash_reporter_request(request),
            _ => panic!("Expected report to be filed."),
        };

        // Instance 1 coalesced (partial + final -> 1 final), Instance 2 coalesced
        // (partial 0 + partial 1 -> 1 partial), and Instance 3 remained separate (1 partial),
        // yielding 3 distinct attachments rather than merging across providers.
        let attachments = report.attachments.unwrap();
        assert_eq!(3, attachments.len());
        assert_eq!("respawning_proc_final_0.pb.gz", attachments[0].key);
        assert_eq!("respawning_proc_partial_1.pb.gz", attachments[1].key);
        assert_eq!("respawning_proc_partial_0.pb.gz", attachments[2].key);
    }

    #[fuchsia::test]
    fn test_pending_report_respects_byte_budget() {
        let page_size = zx::system_get_page_size() as u64;
        let mut pending = PendingReport::default();
        let first_profile = ProfileReport::Final {
            provider_id: 1,
            process_name: "heavy_proc_1".to_string(),
            size: MAX_PENDING_REPORT_BYTES - page_size,
            profile: Vmo::create(64).unwrap(),
        };
        assert!(pending.can_accept(&first_profile));
        pending.push_or_coalesce(first_profile);
        assert!(!pending.is_budget_exhausted());

        // A 100-byte profile rounds up to one page (`page_size`), fitting within
        // the remaining single-page budget.
        let fits_profile = ProfileReport::Final {
            provider_id: 2,
            process_name: "light_proc".to_string(),
            size: 100,
            profile: Vmo::create(64).unwrap(),
        };
        assert!(pending.can_accept(&fits_profile));

        // A profile larger than one page rounds up to `2 * page_size`, exceeding
        // MAX_PENDING_REPORT_BYTES even though the budget is not yet exhausted.
        let overflow_profile = ProfileReport::Final {
            provider_id: 3,
            process_name: "heavy_proc_2".to_string(),
            size: page_size + 1,
            profile: Vmo::create(64).unwrap(),
        };
        assert!(!pending.can_accept(&overflow_profile));
    }

    #[fuchsia::test]
    async fn test_file_report_sanitizes_attachment_keys() {
        let (client, mut request_stream) = create_proxy_and_stream::<CrashReporterMarker>();

        let mut tainted_builder = ProfileBuilder::default();
        tainted_builder.set_process_info(Some("tainted_proc".to_string()), std::iter::empty());
        tainted_builder.set_lossy(true);
        let tainted_final = tainted_builder.build().unwrap();

        let mut slash_space_builder = ProfileBuilder::default();
        slash_space_builder
            .set_process_info(Some("/pkg/bin/my proc".to_string()), std::iter::empty());
        let slash_space_partial = slash_space_builder.build_partial_profile(0).unwrap();

        let (_, report) = try_join!(
            file_report(vec![tainted_final, slash_space_partial], &client),
            request_stream
                .next()
                .map(|item| Ok(handle_crash_reporter_request(item.unwrap().unwrap())))
        )
        .unwrap();

        let attachments = report.attachments.unwrap();
        assert_eq!(2, attachments.len());
        assert_eq!("tainted_proc_[TAINTED:_BUFFER_OVERFLOW]_final_0.pb.gz", attachments[0].key);
        assert_eq!("_pkg_bin_my_proc_partial_0.pb.gz", attachments[1].key);
    }
}
