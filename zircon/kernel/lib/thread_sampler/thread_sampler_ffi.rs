// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! C++ FFI entry points for `sampler::ThreadSampler`.

use core::ffi::c_void;

use zx_status::Status;
use zx_types::{zx_koid_t, zx_status_t};

use super::{THREAD_SAMPLER, sampler_percpu_init, sampler_percpu_shutdown};
use crate::arch_rs::GeneralRegsSource;

/// Joins the active sampling session on the current CPU (`sampler::sampler_percpu_init`).
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled on the current CPU (`ints_disabled()`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_sampler_percpu_init() {
    // SAFETY: The caller guarantees interrupts are disabled on the current CPU.
    unsafe { sampler_percpu_init() };
}

/// Exits the active sampling session on the current CPU (`sampler::sampler_percpu_shutdown`).
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled on the current CPU (`ints_disabled()`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_sampler_percpu_shutdown() {
    // SAFETY: The caller guarantees interrupts are disabled on the current CPU.
    unsafe { sampler_percpu_shutdown() };
}

/// Re-arms the sampling timer on the current CPU if the session is running
/// (`sampler::ThreadSampler::RescheduleMarking`).
///
/// # Safety
///
/// The caller must ensure that:
/// - Interrupts are disabled on the current CPU (`ints_disabled()`).
/// - This function is called at most once per firing of the current CPU's `sampling_timer`
///   callback, representing an active scheduled marking in `MarkingsScheduled`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_sampler_reschedule_marking() {
    // SAFETY: The caller upholds the safety preconditions of `ThreadSampler::reschedule_marking`.
    unsafe { THREAD_SAMPLER.reschedule_marking() };
}

/// Walks the current thread's user stack starting from `gregs` and emits a profiler backtrace
/// record (`sampler::ThreadSampler::SampleThread`).
///
/// # Safety
///
/// The caller must ensure that:
/// - Interrupts are enabled (`!ints_disabled()`).
/// - If `source` is [`GeneralRegsSource::Iframe`] or `GeneralRegsSource::Syscall`, `gregs` is
///   non-null, properly aligned, and points to a valid, initialized register frame of the
///   corresponding architecture type with no concurrent mutable aliases for the duration of this
///   call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_thread_sampler_sample_thread(
    pid: zx_koid_t,
    tid: zx_koid_t,
    source: GeneralRegsSource,
    gregs: *const c_void,
    session_id: u64,
) -> zx_status_t {
    // SAFETY: The caller upholds the safety preconditions of `ThreadSampler::sample_thread`.
    Status::result_into_raw(unsafe {
        THREAD_SAMPLER.sample_thread(pid, tid, source, gregs, session_id)
    })
}
