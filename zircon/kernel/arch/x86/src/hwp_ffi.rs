// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::cpuid::RealCpuId;
use super::hwp::{IntelHwpPolicy, intel_hwp_init, intel_hwp_supported};
use super::platform_access::RealMsrAccess;

/// Initializes Intel HWP on the current CPU using `policy`.
///
/// If HWP is not supported on the current CPU, no action is taken.
///
/// # Safety
///
/// Must be called in ring 0 (CPL 0) with a valid [`IntelHwpPolicy`] discriminant (`0..=4`).
// SAFETY: Global symbol name is unique and signature matches `arch/x86/hwp_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_intel_hwp_init(policy: IntelHwpPolicy) {
    let cpuid = RealCpuId;
    let mut msr = RealMsrAccess;
    intel_hwp_init(&cpuid, &mut msr, policy);
}

/// Returns whether Intel HWP is supported on the current CPU.
///
/// # Safety
///
/// Must be called on x86_64 hardware supporting `cpuid` (ring 0 in kernel context).
// SAFETY: Global symbol name is unique and signature matches `arch/x86/hwp_ffi.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_intel_hwp_supported() -> bool {
    let cpuid = RealCpuId;
    intel_hwp_supported(&cpuid)
}
