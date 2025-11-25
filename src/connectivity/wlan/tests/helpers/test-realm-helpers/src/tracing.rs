// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::trace_runner::{TerminationResult, TraceRunner};
use anyhow::format_err;
use log::{info, warn};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

static NEXT_TRACING_ID: AtomicUsize = AtomicUsize::new(0);
static DEFAULT_TRACE_TIMEOUT: Duration = Duration::from_mins(2);
static DEFAULT_TRACE_FILE_MAX_BYTES: usize = 100 * 1024 * 1024;

/// An RAII-style struct that starts tracing in the test realm upon creation via `Tracing::start`
/// and collects and writes the trace when the struct is dropped.
pub struct Tracing {
    output_trace_path: std::ffi::OsString,
    tracer: TraceRunner,
    always_record_trace: bool,
}

impl Tracing {
    pub async fn start(test_ns_prefix: &str) -> Result<Self, anyhow::Error> {
        Self::start_(test_ns_prefix, false, DEFAULT_TRACE_TIMEOUT, DEFAULT_TRACE_FILE_MAX_BYTES)
            .await
    }

    async fn start_<'a>(
        test_ns_prefix: &str,
        always_record_trace: bool,
        trace_timeout: Duration,
        trace_file_max_bytes: usize,
    ) -> Result<Self, anyhow::Error> {
        // The test namespace prefix ensures all generated trace files for a test suite
        // are unique per test realm.  In case multiple trace files are generated in the
        // same process, the tracing_id ensures their names will still be unique even
        // when the same test namespace prefix is used.
        let tracing_id = NEXT_TRACING_ID.fetch_add(1, Ordering::SeqCst);
        let trace_file_prefix = test_ns_prefix.strip_prefix("/").unwrap();
        let output_trace_path = std::path::Path::new(
            format!("/custom_artifacts/{tracing_id:04}-{trace_file_prefix}-trace.fxt").as_str(),
        )
        .to_path_buf();

        let tracer = TraceRunner::start(
            test_ns_prefix.to_string(),
            output_trace_path.clone(),
            trace_timeout,
            trace_file_max_bytes,
        )
        .await?;

        Ok(Tracing { output_trace_path: output_trace_path.into(), tracer, always_record_trace })
    }
}

impl Drop for Tracing {
    fn drop(&mut self) {
        let always_record_trace = self.always_record_trace;
        let output_trace_path = self.output_trace_path.clone().into_string().unwrap();

        match self.tracer.terminate_trace(format!("Tracing::drop")) {
            TerminationResult { termination_signal: None, .. } => {
                warn!("Terminate signal sent before Tracing::drop. Keeping trace.");
                return;
            }
            TerminationResult { termination_signal: Some(Err(e)), .. } => {
                warn!("Failed to signal termination of trace-writer: {e:?}");
                return;
            }
            TerminationResult { trace_writer: Some(Err(e)), .. } => {
                warn!("Failed to terminate trace-writer: {e:?}");
                return;
            }
            _ => (),
        }

        if !always_record_trace {
            info!("Discarding trace because Tracing instance dropped before a panic.");
            std::fs::remove_file(output_trace_path.clone())
                .map_err(|e| format_err!("Failed to remove {output_trace_path}: {e:?}"))
                .unwrap();
        }
    }
}
