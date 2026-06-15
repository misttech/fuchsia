// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_mem as fmem;
use fuchsia_async as fasync;
use fuchsia_async::TimeoutExt;
use fuchsia_runtime as fruntime;
use starnix_c_file_buffer::CFileBuffer;
use starnix_logging::{log_debug, log_error, log_warn};
use std::collections::{BTreeSet, HashMap};
use uuid::Uuid;
use zx::{self, Task};

// We use `inspector_print_debug_info` directly instead of
// `backtrace_request_thread` because `backtrace_request_thread` relies on the
// exception mechanism (crashsvc). If we use exceptions, the exception is
// attributed to the main Starnix kernel process (which detects the lockup),
// not the process containing the locked-up thread. This would prevent
// crashsvc from accessing the correct thread state and stack. By holding the
// thread handle directly, we can inspect it regardless of its process.
// SAFETY: This declares external C symbols from the Zircon inspector library.
unsafe extern "C" {
    fn inspector_print_debug_info(
        out: *mut std::ffi::c_void,
        process: zx::sys::zx_handle_t,
        thread: zx::sys::zx_handle_t,
    );
}

async fn dump_thread_backtrace<'a>(
    thread: &zx::Thread,
    file_buffer: &'a mut CFileBuffer,
    timeout: zx::MonotonicDuration,
) -> Result<&'a str, anyhow::Error> {
    let _suspend_token = thread.suspend()?;

    // Wait for suspended signal asynchronously.
    fasync::OnSignals::new(thread, zx::Signals::THREAD_SUSPENDED)
        .on_timeout(timeout, || Err(zx::Status::TIMED_OUT))
        .await?;

    // Reset the buffer to overwrite previous contents.
    file_buffer.reset().map_err(|e| anyhow::anyhow!("Failed to reset CFileBuffer: {}", e))?;

    // SAFETY: Calling FFI is safe when passing valid handles.
    unsafe {
        let process_self = fruntime::process_self().raw_handle();
        let file_ptr = file_buffer.file();
        inspector_print_debug_info(
            file_ptr.as_raw() as *mut std::ffi::c_void,
            process_self,
            thread.raw_handle(),
        );
    }

    let data = file_buffer.data();
    let backtrace_str = str::from_utf8(data)?;
    Ok(backtrace_str)
}

const LOCKUP_DETECTOR_INTERVAL_MINUTES: i64 = 2;
const RCU_STALL_THRESHOLD_SAMPLES: u32 = 4;

struct ActiveRcuRead {
    consecutive_polls_active: u32,
    first_seen: zx::MonotonicInstant,
    last_counter_index: u8,
}

struct RcuStall {
    thread: zx::Thread,
    koid: zx::Koid,
    first_seen: zx::MonotonicInstant,
}

#[derive(Default)]
struct LockupDetectorContext {
    event_id: Option<String>,
    reported_lockup_koids: BTreeSet<zx::Koid>,
    reported_rcu_koids: BTreeSet<zx::Koid>,
    active_rcu_reads: HashMap<zx::Koid, ActiveRcuRead>,
}

async fn check_and_report_lockups(
    context: &mut LockupDetectorContext,
) -> Result<(), anyhow::Error> {
    let long_running = starnix_core::task::ThreadLockupDetector::get_long_running_threads(
        zx::MonotonicDuration::from_minutes(LOCKUP_DETECTOR_INTERVAL_MINUTES),
    );

    let current_koids: BTreeSet<zx::Koid> = long_running.iter().map(|r| r.koid).collect();

    // Clean up threads that are no longer locked up.
    context.reported_lockup_koids.retain(|koid| current_koids.contains(koid));

    if long_running.is_empty() {
        return Ok(());
    }

    // Identify newly locked threads.
    let newly_locked: Vec<_> =
        long_running.iter().filter(|r| !context.reported_lockup_koids.contains(&r.koid)).collect();

    if newly_locked.is_empty() {
        return Ok(());
    }

    let mut file_buffer = CFileBuffer::new(1024 * 1024)
        .map_err(|e| anyhow::anyhow!("Failed to create CFileBuffer: {}", e))?;

    let event_id_str = context.event_id.get_or_insert_with(|| Uuid::new_v4().to_string()).clone();

    let koids_str = format!("{:?}", current_koids.iter().map(|k| k.raw_koid()).collect::<Vec<_>>());

    log_error!(
        "Detected threads locked up for more than {} minutes: {:?}",
        LOCKUP_DETECTOR_INTERVAL_MINUTES,
        current_koids
    );

    let mut thread_names = BTreeSet::new();
    for registered in &long_running {
        let name = if let Ok(name) = registered.thread.get_name() {
            format!("{}({})", name, registered.koid.raw_koid())
        } else {
            format!("koid-{}", registered.koid.raw_koid())
        };
        thread_names.insert(name);
    }
    let mut names_str = thread_names.into_iter().collect::<Vec<_>>().join(", ");
    let max_annotation_len = ffeedback::MAX_ANNOTATION_VALUE_LENGTH as usize;
    if names_str.len() > max_annotation_len {
        let mut limit = max_annotation_len - 3;
        while !names_str.is_char_boundary(limit) {
            limit -= 1;
        }
        names_str.truncate(limit);
        names_str.push_str("...");
    }

    let reporter =
        fuchsia_component::client::connect_to_protocol::<ffeedback::CrashReporterMarker>()
            .context("Failed to connect to CrashReporter")?;

    for registered in newly_locked {
        let bt = dump_thread_backtrace(
            &registered.thread,
            &mut file_buffer,
            zx::MonotonicDuration::from_seconds(1),
        )
        .await
        .with_context(|| {
            format!("Failed to dump backtrace for thread {}", registered.koid.raw_koid())
        })?;
        log_error!("Locked thread backtrace:\n{}", bt);

        let size = bt.len() as u64;
        let vmo = zx::Vmo::create(size).context("Failed to create VMO")?;
        vmo.write(bt.as_bytes(), 0).context("Failed to write backtrace to VMO")?;

        let attachments = vec![ffeedback::Attachment {
            key: "backtrace.txt".to_string(),
            value: fmem::Buffer { vmo, size },
        }];

        let report = ffeedback::CrashReport {
            program_name: Some("starnix_kernel".to_string()),
            crash_signature: Some("fuchsia-starnix_kernel-thread-lockup".to_string()),
            is_fatal: Some(false),
            annotations: Some(vec![
                ffeedback::Annotation {
                    key: "starnix.lockup_thread_koids".to_string(),
                    value: koids_str.clone(),
                },
                ffeedback::Annotation {
                    key: "starnix.lockup_thread_names".to_string(),
                    value: names_str.clone(),
                },
                ffeedback::Annotation {
                    key: "starnix.lockup_target_thread_koid".to_string(),
                    value: registered.koid.raw_koid().to_string(),
                },
            ]),
            attachments: Some(attachments),
            event_id: Some(event_id_str.clone()),
            ..Default::default()
        };

        reporter.file_report(report).await.context("Failed to call file_report")?.map_err(|e| {
            anyhow::anyhow!(
                "Failed to file crash report for thread {}: {:?}",
                registered.koid.raw_koid(),
                e
            )
        })?;

        log_debug!("Filed crash report for thread lockup (thread {}).", registered.koid.raw_koid());
        context.reported_lockup_koids.insert(registered.koid);
    }

    Ok(())
}

async fn check_rcu_stalls(context: &mut LockupDetectorContext) -> Vec<RcuStall> {
    let now = zx::MonotonicInstant::get();
    let mut active_koids = std::collections::HashSet::new();
    let mut stalled_threads = vec![];

    starnix_core::task::ThreadLockupDetector::active_rcu_read_locks(
        |thread, koid, counter_index| {
            active_koids.insert(koid);
            let stall_info = context.active_rcu_reads.get(&koid);
            let (count, first_seen) = match stall_info {
                Some(info) => {
                    if info.last_counter_index != counter_index {
                        // Counter index changed, progress was made.
                        (1, now)
                    } else {
                        (info.consecutive_polls_active + 1, info.first_seen)
                    }
                }
                None => (1, now),
            };

            context.active_rcu_reads.insert(
                koid,
                ActiveRcuRead {
                    consecutive_polls_active: count,
                    first_seen,
                    last_counter_index: counter_index,
                },
            );

            if count >= RCU_STALL_THRESHOLD_SAMPLES
                && context.reported_rcu_koids.insert(koid)
                && let Ok(thread_dup) = thread.duplicate_handle(zx::Rights::SAME_RIGHTS)
            {
                stalled_threads.push(RcuStall { thread: thread_dup, koid, first_seen });
            }
        },
    );

    context.active_rcu_reads.retain(|koid, _| active_koids.contains(koid));
    context.reported_rcu_koids.retain(|koid| active_koids.contains(koid));

    stalled_threads
}

async fn report_rcu_stalls(stalls: Vec<RcuStall>) -> Result<(), anyhow::Error> {
    if stalls.is_empty() {
        return Ok(());
    }
    let mut file_buffer = CFileBuffer::new(1024 * 1024)
        .map_err(|e| anyhow::anyhow!("Failed to create CFileBuffer: {}", e))?;

    for stall in stalls {
        let name = stall
            .thread
            .get_name()
            .map(|n| n.to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        match dump_thread_backtrace(
            &stall.thread,
            &mut file_buffer,
            zx::MonotonicDuration::from_millis(100),
        )
        .await
        {
            Ok(backtrace_str) => {
                log_error!(
                    "RCU Stall detected: Thread {}({}) has held RCU read lock for {}ms. Backtrace:\n{}",
                    name,
                    stall.koid.raw_koid(),
                    (zx::MonotonicInstant::get() - stall.first_seen).into_millis(),
                    backtrace_str
                );
            }
            Err(e) => {
                log_error!(
                    "RCU Stall suspected on Thread {}({}), but failed to suspend for backtrace: {:?}",
                    name,
                    stall.koid.raw_koid(),
                    e
                );
            }
        }
    }
    Ok(())
}

pub fn start_thread_lockup_detector() -> fasync::Task<()> {
    fasync::Task::spawn(async {
        let mut context = LockupDetectorContext::default();
        loop {
            fasync::Timer::new(zx::MonotonicInstant::after(zx::MonotonicDuration::from_minutes(
                LOCKUP_DETECTOR_INTERVAL_MINUTES,
            )))
            .await;

            let stalls = check_rcu_stalls(&mut context).await;
            if let Err(e) = report_rcu_stalls(stalls).await {
                log_warn!("Error reporting RCU stalls: {:?}", e);
            }

            let _waiting_guard = starnix_core::task::ThreadLockupDetector::pause_tracking();
            if let Err(e) = check_and_report_lockups(&mut context).await {
                log_warn!("Error in thread lockup detector: {:?}", e);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Barrier};

    #[fuchsia::test]
    async fn test_rcu_lockup_detector() {
        let barrier = Arc::new(Barrier::new(2));
        let barrier_clone = barrier.clone();

        // Spawn a thread that holds an RCU read lock.
        let thread = std::thread::spawn(move || {
            let _guard = starnix_core::task::ThreadLockupDetector::track();
            let _scope = fuchsia_rcu::RcuReadScope::new();
            barrier_clone.wait(); // Synchronize with the main test thread.
            barrier_clone.wait(); // Block until test finishes.
        });

        barrier.wait(); // Wait for thread to acquire lock.

        let mut context = LockupDetectorContext::default();

        // Run check_rcu_stalls.
        // We need to run it multiple times to trigger the stall.
        // Threshold is RCU_STALL_THRESHOLD_SAMPLES = 4.

        // 1st sample
        let candidates = check_rcu_stalls(&mut context).await;
        assert!(candidates.is_empty());
        assert_eq!(context.active_rcu_reads.len(), 1);
        let koid = *context.active_rcu_reads.keys().next().unwrap();
        assert_eq!(context.active_rcu_reads.get(&koid).unwrap().consecutive_polls_active, 1);

        // 2nd and 3rd samples
        for i in 2..=3 {
            let candidates = check_rcu_stalls(&mut context).await;
            assert!(candidates.is_empty());
            assert_eq!(context.active_rcu_reads.get(&koid).unwrap().consecutive_polls_active, i);
        }

        // 4th sample (should return candidate)
        let candidates = check_rcu_stalls(&mut context).await;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].koid, koid);
        assert_eq!(context.active_rcu_reads.get(&koid).unwrap().consecutive_polls_active, 4);

        barrier.wait(); // Allow thread to exit.
        thread.join().unwrap();

        // Run check again, thread should be gone.
        let candidates = check_rcu_stalls(&mut context).await;
        assert!(candidates.is_empty());
        assert!(context.active_rcu_reads.is_empty());
    }
}
