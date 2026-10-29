// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::Iframe;
#[cfg(console_enabled)]
use super::arch::{Backtrace, CpuContext};
use crate::kernel::types::cpu_num_t;
#[cfg(console_enabled)]
use zx_types::{zx_duration_mono_t, zx_status_t};

unsafe extern "C" {
    /// Requests the execution context of `target_cpu`, waiting up to `timeout`.
    ///
    /// # Safety
    ///
    /// Interrupts must be disabled on the calling CPU, `target_cpu` must be a valid CPU number,
    /// and `context` must point to a valid, mutable `CpuContext`.
    #[cfg(console_enabled)]
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
    #[cfg(console_enabled)]
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
}
