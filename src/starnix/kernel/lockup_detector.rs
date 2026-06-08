// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context as _;
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_mem as fmem;
use fuchsia_async as fasync;
use fuchsia_runtime as fruntime;
use starnix_c_file_buffer::CFileBuffer;
use starnix_logging::{log_debug, log_error, log_warn};
use std::collections::BTreeSet;
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
) -> Result<&'a str, anyhow::Error> {
    let _suspend_token = thread.suspend()?;

    // Wait for suspended signal asynchronously.
    fasync::OnSignals::new(thread, zx::Signals::THREAD_SUSPENDED).await?;

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
    log_error!("Locked thread backtrace:\n{}", backtrace_str);
    Ok(backtrace_str)
}

const LOCKUP_DETECTOR_INTERVAL_MINUTES: i64 = 2;

#[derive(Default)]
struct LockupDetectorContext {
    event_id: Option<String>,
    reported_koids: BTreeSet<zx::Koid>,
}

async fn check_and_report_lockups(
    context: &mut LockupDetectorContext,
) -> Result<(), anyhow::Error> {
    let long_running = starnix_core::task::ThreadLockupDetector::get_long_running_threads(
        zx::MonotonicDuration::from_minutes(LOCKUP_DETECTOR_INTERVAL_MINUTES),
    );

    let current_koids: BTreeSet<zx::Koid> = long_running.iter().map(|r| r.koid).collect();

    // Clean up threads that are no longer locked up.
    context.reported_koids.retain(|koid| current_koids.contains(koid));

    if long_running.is_empty() {
        return Ok(());
    }

    // Identify newly locked threads.
    let newly_locked: Vec<_> =
        long_running.iter().filter(|r| !context.reported_koids.contains(&r.koid)).collect();

    if newly_locked.is_empty() {
        return Ok(());
    }

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

    let mut file_buffer = CFileBuffer::new(1024 * 1024)
        .map_err(|e| anyhow::anyhow!("Failed to create CFileBuffer: {}", e))?;

    let reporter =
        fuchsia_component::client::connect_to_protocol::<ffeedback::CrashReporterMarker>()
            .context("Failed to connect to CrashReporter")?;

    for registered in newly_locked {
        let bt = dump_thread_backtrace(&registered.thread, &mut file_buffer).await.with_context(
            || format!("Failed to dump backtrace for thread {}", registered.koid.raw_koid()),
        )?;

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
        context.reported_koids.insert(registered.koid);
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
            let _waiting_guard = starnix_core::task::ThreadLockupDetector::pause_tracking();
            if let Err(e) = check_and_report_lockups(&mut context).await {
                log_warn!("Error in thread lockup detector: {:?}", e);
            }
        }
    })
}
