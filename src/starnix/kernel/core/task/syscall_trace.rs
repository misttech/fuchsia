// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tracks the `CATEGORY_STARNIX` duration of the syscall running on the current thread, so that
//! code inside the syscall can end that duration early.
//!
//! `TraceMarkerFile::write` uses this to end the syscall's slice before it records an
//! `ftrace/print` (`atrace`) event. Otherwise the syscall slice ends after the `atrace` event on
//! the same thread track, and Perfetto's strict slice nesting truncates the `atrace` slice. Linux
//! `ftrace` traces avoid this the same way, in Perfetto's
//! `SyscallTracker::MaybeTruncateOngoingWriteSlice`.

use fuchsia_trace::{TraceCategoryContext, trace_site_t};
use starnix_logging::CATEGORY_STARNIX;
use std::cell::Cell;

thread_local! {
    /// The name and start time of the syscall duration that is open on the current thread, if any.
    ///
    /// This is a `const`-initialized thread-local without a destructor, so accessing it is cheap.
    /// A `CurrentTask` runs syscalls only on its own thread, so this holds the state of that task's
    /// current syscall.
    static SYSCALL_TRACE: Cell<Option<(&'static str, zx::BootTicks)>> = const { Cell::new(None) };
}

/// Ends the syscall duration started by [`begin_syscall_trace`] when dropped, unless
/// [`complete_syscall_trace`] already ended it.
#[must_use = "the syscall duration ends when the scope is dropped"]
pub struct SyscallTraceScope(());

impl Drop for SyscallTraceScope {
    fn drop(&mut self) {
        complete_syscall_trace();
    }
}

/// Starts a `CATEGORY_STARNIX` duration for the syscall about to run on the current thread.
///
/// Returns `None`, without calling `name`, if the category is disabled. Otherwise the duration
/// ends when the returned scope is dropped, or earlier at a call to [`complete_syscall_trace`].
#[inline]
pub fn begin_syscall_trace(name: impl FnOnce() -> &'static str) -> Option<SyscallTraceScope> {
    static CACHE: trace_site_t = trace_site_t::new(0);
    // The context is dropped right away. `complete_syscall_trace` acquires it again, so the trace
    // buffers can be flushed while the syscall runs, as with `fuchsia_trace::duration!`.
    TraceCategoryContext::acquire_cached(CATEGORY_STARNIX, &CACHE)?;
    SYSCALL_TRACE.set(Some((name(), zx::BootTicks::get())));
    Some(SyscallTraceScope(()))
}

/// Ends the syscall duration that is open on the current thread, if any, and writes it to the
/// trace.
///
/// Called early by `TraceMarkerFile::write`, right before it records an `ftrace/print` event, so
/// that the syscall slice ends at or before the event's timestamp.
#[inline]
pub fn complete_syscall_trace() {
    if let Some((name, start_time)) = SYSCALL_TRACE.take() {
        fuchsia_trace::complete_duration(CATEGORY_STARNIX, name, start_time, &[]);
    }
}

/// Opens a syscall duration on the current thread even if `CATEGORY_STARNIX` is disabled, so
/// tests can check that code ends it. Drop the returned scope to clear it.
pub fn begin_syscall_trace_for_testing(name: &'static str) -> SyscallTraceScope {
    SYSCALL_TRACE.set(Some((name, zx::BootTicks::get())));
    SyscallTraceScope(())
}

/// Returns whether a syscall duration is open on the current thread.
pub fn has_active_syscall_trace_for_testing() -> bool {
    SYSCALL_TRACE.with(|trace| trace.get().is_some())
}
