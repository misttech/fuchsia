// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

unsafe extern "C" {
    /// Returns whether RAPL power limit controls are supported on the current x86
    /// microarchitecture.
    ///
    /// # Safety
    ///
    /// Must be called after early boot CPU microarchitecture initialization.
    pub(super) fn cpp_x86_power_limit_suported() -> bool;

    /// Toggles the recurring RAPL energy status callback.
    ///
    /// # Safety
    ///
    /// Synchronizes recurring callback state internally with no caller preconditions.
    pub(super) fn cpp_x86_status_callback_toggle();

    /// Prints CPU and graphics performance limit reasons from architectural MSRs.
    ///
    /// # Safety
    ///
    /// Checks microarchitecture support internally before reading MSRs; must be called after early
    /// boot CPU microarchitecture initialization.
    pub(super) fn cpp_x86_print_limit_reasons(use_log: bool);

    /// Clears the latched CPU and graphics performance limit reason log MSR bits.
    ///
    /// # Safety
    ///
    /// Checks microarchitecture support internally before writing MSRs; must be called after early
    /// boot CPU microarchitecture initialization.
    pub(super) fn cpp_x86_clear_limit_reason_log();

    /// Prints RAPL package power limits (PL1 and PL2) from MSRs.
    ///
    /// # Safety
    ///
    /// Must only be called when `cpp_x86_power_limit_suported()` is true.
    pub(super) fn cpp_x86_print_limits();
}
