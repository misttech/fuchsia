// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::amd;
use super::feature::X86ModelInfo;

/// FFI wrapper for `x86_amd_get_patch_level`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an AMD processor and that
/// early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_amd_get_patch_level() -> u32 {
    // SAFETY: Upheld by caller.
    unsafe { amd::x86_amd_get_patch_level() }
}

/// FFI wrapper for `x86_amd_zen2_retbleed_mitigation`.
///
/// # Safety
///
/// `model` must be a valid, aligned shared reference to an initialized `X86ModelInfo` for the
/// duration of the call. The caller must ensure that the current privilege level is CPL 0 on an
/// AMD processor matching `model` and that early-boot CPUID feature detection has initialized
/// `X86_FEATURE_HYPERVISOR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_amd_zen2_retbleed_mitigation(model: &X86ModelInfo) {
    // SAFETY: Upheld by caller.
    unsafe { amd::x86_amd_zen2_retbleed_mitigation(model) };
}

/// FFI wrapper for `x86_amd_has_retbleed`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_x86_amd_has_retbleed() -> bool {
    amd::x86_amd_has_retbleed()
}

/// FFI wrapper for `x86_amd_init_percpu`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an AMD processor.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_amd_init_percpu() {
    // SAFETY: Upheld by caller.
    unsafe { amd::x86_amd_init_percpu() };
}
