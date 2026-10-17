// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_char;

unsafe extern "C" {
    /// Parses `str` as an `IntelHwpPolicy`, writing the discriminant to `out_policy` on success.
    ///
    /// # Safety
    ///
    /// `str` must be either null or point to a valid, NUL-terminated C string, and `out_policy`
    /// must point to a valid, mutable `u8`.
    pub(super) fn cpp_x86_intel_hwp_parse_policy(str: *const c_char, out_policy: *mut u8) -> bool;

    /// Initializes Intel HWP on the current CPU using `policy`.
    ///
    /// # Safety
    ///
    /// `policy` must be a valid `IntelHwpPolicy` discriminant obtained from
    /// `cpp_x86_intel_hwp_parse_policy`.
    pub(super) fn cpp_x86_intel_hwp_init(policy: u8);

    /// Constructs a 64-bit `IA32_HWP_REQUEST` MSR value.
    ///
    /// # Safety
    ///
    /// Pure bitfield packing function with no memory safety preconditions.
    pub(super) fn cpp_x86_make_hwp_request(
        min_perf: u8,
        max_perf: u8,
        desired_perf: u8,
        epp: u8,
    ) -> u64;

    /// Returns whether Intel HWP is supported on the current CPU.
    ///
    /// # Safety
    ///
    /// Reads CPUID feature leaves on the current CPU with no caller preconditions.
    pub(super) fn cpp_x86_intel_hwp_supported() -> bool;
}
