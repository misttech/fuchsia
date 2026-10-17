// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::Iframe;
use super::arch::{Backtrace, CpuContext};
use crate::kernel::types::cpu_num_t;
use zx_types::{zx_duration_mono_t, zx_status_t};

unsafe extern "C" {
    /// Requests the execution context of `target_cpu`, waiting up to `timeout`.
    ///
    /// # Safety
    ///
    /// Interrupts must be disabled on the calling CPU, `target_cpu` must be a valid CPU number,
    /// and `context` must point to a valid, mutable `CpuContext`.
    pub(super) fn cpp_x86_request_context(
        target_cpu: cpu_num_t,
        timeout: zx_duration_mono_t,
        context: *mut CpuContext,
    ) -> zx_status_t;

    /// Prints `bt` to the kernel console.
    ///
    /// # Safety
    ///
    /// `bt` must point to a valid, initialized `Backtrace`.
    pub(super) fn cpp_x86_backtrace_print(bt: *const Backtrace);

    /// Prints `frame` to the kernel console.
    ///
    /// # Safety
    ///
    /// `frame` must point to a valid `Iframe`.
    pub(super) fn cpp_x86_print_frame(frame: *const Iframe);

    /// Prints x86 CPU feature and vulnerability information to the kernel console.
    ///
    /// # Safety
    ///
    /// Must be called after early boot CPUID initialization.
    pub(super) fn cpp_x86_feature_debug();

    /// Reads `msr_id` on `cpu` synchronously via IPI.
    ///
    /// # Safety
    ///
    /// `msr_id` must be a valid architectural MSR on `cpu`.
    pub(super) fn cpp_x86_read_msr_on_cpu(cpu: cpu_num_t, msr_id: u32) -> u64;

    /// Writes `val` to `msr_id` on `cpu` synchronously via IPI.
    ///
    /// # Safety
    ///
    /// `msr_id` must be a valid architectural MSR on `cpu` and `val` must be a valid value for it.
    pub(super) fn cpp_x86_write_msr_on_cpu(cpu: cpu_num_t, msr_id: u32, val: u64);
}
