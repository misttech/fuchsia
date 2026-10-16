// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Helpers for triggering best-effort crash reports.

use anyhow::anyhow;
use fidl_fuchsia_feedback::{CrashReport, CrashReporterMarker, CrashReporterProxy};
use fuchsia_async as fasync;
use log::{error, warn};

const TWENTY_FOUR_HOURS: zx::MonotonicDuration = zx::MonotonicDuration::from_hours(24);

#[derive(Default)]
pub(crate) struct CrashReporter {
    previous_installation_error_report_timestamp: Option<zx::MonotonicInstant>,
}

impl CrashReporter {
    pub(crate) fn new() -> Self {
        Self::default()
    }

    fn connect_proxy() -> Option<CrashReporterProxy> {
        match fuchsia_component::client::connect_to_protocol::<CrashReporterMarker>() {
            Ok(p) => Some(p),
            Err(e) => {
                error!("Failed to connect to fuchsia.feedback/CrashReporter: {:#}", anyhow!(e));
                None
            }
        }
    }

    /// Files an installation error crash report in a background task if one has not already been
    /// filed in the past 24 hours. Logs on error because crash-reporting is best-effort.
    pub(crate) fn installation_error(&mut self) {
        if let Some(proxy) = Self::connect_proxy() {
            self.installation_error_impl(proxy, zx::MonotonicInstant::get());
        }
    }

    fn installation_error_impl(&mut self, proxy: CrashReporterProxy, now: zx::MonotonicInstant) {
        if let Some(prev) = self.previous_installation_error_report_timestamp
            && now < prev + TWENTY_FOUR_HOURS
        {
            warn!(
                "skipping report because we already filed one in the past 24 hours (at {prev:?})"
            );
            return;
        }
        self.previous_installation_error_report_timestamp = Some(now);
        Self::file_report(proxy, "fuchsia-installation-error", now);
    }

    /// Files an excessive update duration crash report in a background task. Logs on error because
    /// crash-reporting is best-effort.
    pub(crate) fn excessive_update_duration(&self) {
        if let Some(proxy) = Self::connect_proxy() {
            Self::file_report(
                proxy,
                "fuchsia-excessive-update-duration",
                zx::MonotonicInstant::get(),
            );
        }
    }

    fn file_report(proxy: CrashReporterProxy, signature: &'static str, now: zx::MonotonicInstant) {
        fasync::Task::spawn(async move {
            match proxy
                .file_report(CrashReport {
                    crash_signature: Some(signature.to_owned()),
                    program_name: Some("system".to_owned()),
                    program_uptime: Some(now.into_nanos()),
                    is_fatal: Some(false),
                    ..Default::default()
                })
                .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => error!("Error filing crash report: {e:?}"),
                Err(e) => error!("FIDL error filing crash report: {:#}", anyhow!(e)),
            }
        })
        .detach();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_feedback::{FileReportResults, FilingError};
    use futures::prelude::*;
    use mock_crash_reporter::{MockCrashReporterService, ThrottleHook};
    use std::assert_matches;
    use std::sync::Arc;
    use test_case::test_case;

    fn assert_signature(report: CrashReport, expected_signature: &str) {
        assert_matches!(
            report,
            CrashReport {
                crash_signature: Some(signature),
                program_name: Some(program),
                program_uptime: Some(_),
                is_fatal: Some(false),
                ..
            } if signature == expected_signature && program == "system"
        )
    }

    #[test_case(Ok(FileReportResults::default()); "success")]
    #[test_case(Err(FilingError::InvalidArgsError); "error_ignored")]
    #[fuchsia::test]
    async fn test_file_crash_report(res: Result<FileReportResults, FilingError>) {
        let (hook, mut recv) = ThrottleHook::new(res);
        let mock = Arc::new(MockCrashReporterService::new(hook));
        let (proxy, _crash_report_server) = mock.spawn_crash_reporter_service();
        let mut crash_reporter = CrashReporter::new();

        crash_reporter.installation_error_impl(proxy.clone(), zx::MonotonicInstant::get());
        assert_signature(recv.next().await.unwrap(), "fuchsia-installation-error");

        CrashReporter::file_report(
            proxy,
            "fuchsia-excessive-update-duration",
            zx::MonotonicInstant::get(),
        );
        assert_signature(recv.next().await.unwrap(), "fuchsia-excessive-update-duration");
    }

    #[fuchsia::test]
    async fn test_installation_error_deduplicated_over_24_hours() {
        let (hook, mut recv) = ThrottleHook::new(Ok(FileReportResults::default()));
        let mock = Arc::new(MockCrashReporterService::new(hook));
        let (proxy, _fidl_server) = mock.spawn_crash_reporter_service();
        let mut crash_reporter = CrashReporter::new();
        let mut now = zx::MonotonicInstant::get();

        // On the first InstallationError, we file a report.
        crash_reporter.installation_error_impl(proxy.clone(), now);
        assert_signature(recv.next().await.unwrap(), "fuchsia-installation-error");

        // Subsequent requests within 24 hours should not file a report.
        crash_reporter.installation_error_impl(proxy.clone(), now);
        assert_matches!(recv.try_recv(), Err(_));
        now += TWENTY_FOUR_HOURS - zx::MonotonicDuration::from_seconds(1);
        crash_reporter.installation_error_impl(proxy.clone(), now);
        assert_matches!(recv.try_recv(), Err(_));

        // When we hit 24 hrs, we'll file a new report.
        now += zx::MonotonicDuration::from_seconds(1);
        crash_reporter.installation_error_impl(proxy.clone(), now);
        assert_signature(recv.next().await.unwrap(), "fuchsia-installation-error");

        // We'll also file a new report when we exceed 24 hours.
        now += TWENTY_FOUR_HOURS + zx::MonotonicDuration::from_seconds(1);
        crash_reporter.installation_error_impl(proxy, now);
        assert_signature(recv.next().await.unwrap(), "fuchsia-installation-error");
    }
}
