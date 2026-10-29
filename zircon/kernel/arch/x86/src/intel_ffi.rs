// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::cpuid::RealCpuId;
use super::idle_states::X86IdleState;
use super::intel;
use super::platform_access::RealMsrAccess;

/// FFI wrapper for `x86_intel_idle_state_may_empty_rsb`.
///
/// # Safety
///
/// `state` must be a valid, aligned shared reference to an initialized `X86IdleState` for the
/// duration of the call, and early-boot microarchitecture detection must have initialized
/// `x86_microarch_config`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_intel_idle_state_may_empty_rsb(state: &X86IdleState) -> bool {
    intel::x86_intel_idle_state_may_empty_rsb(state)
}

/// FFI wrapper for `x86_intel_get_patch_level`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an Intel processor and that
/// early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_intel_get_patch_level() -> u32 {
    // SAFETY: Upheld by caller.
    unsafe { intel::x86_intel_get_patch_level() }
}

/// FFI wrapper for `x86_intel_cpu_has_rsb_fallback` using real hardware `CPUID` and `MSR` access.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_intel_cpu_has_rsb_fallback() -> bool {
    // TODO(61093): Replace with newer lib/arch and hwreg counterparts.
    let cpuid = RealCpuId;
    let msr = RealMsrAccess;
    intel::x86_intel_cpu_has_rsb_fallback(&cpuid, &msr)
}

/// FFI wrapper for `x86_intel_init_percpu`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an Intel processor and that
/// early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR` and
/// `x86_microarch_config`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_intel_init_percpu() {
    // SAFETY: Upheld by caller.
    unsafe { intel::x86_intel_init_percpu() };
}
