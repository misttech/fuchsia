// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::cpuid::{CpuId, Features, RealCpuId};
use super::feature::{X86_FEATURE_HYPERVISOR, X86ModelInfo, x86_feature_test};
use super::platform_access::{MsrAccess, RealMsrAccess};
use super::registers::{
    X86_MSR_AMD_F10_DE_CFG, X86_MSR_AMD_F10_DE_CFG_LFENCE_SERIALIZE, X86_MSR_IA32_BIOS_SIGN_ID,
};
use super::x86::read_msr;

/// Returns the current microcode patch level on AMD processors.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an AMD processor and that
/// early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR`.
#[inline]
pub(super) unsafe fn x86_amd_get_patch_level() -> u32 {
    let mut patch_level = 0;
    if !x86_feature_test(X86_FEATURE_HYPERVISOR) {
        // SAFETY: `X86_MSR_IA32_BIOS_SIGN_ID` is readable in ring 0 when not on a hypervisor.
        patch_level = unsafe { read_msr(X86_MSR_IA32_BIOS_SIGN_ID) } as u32;
    }
    patch_level
}

/// Applies the Zen 2 BTC-NOBR Retbleed mitigation if applicable to `model`.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an AMD processor matching
/// `model` and that early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR`.
#[inline]
pub(super) unsafe fn x86_amd_zen2_retbleed_mitigation(model: &X86ModelInfo) {
    const DE_CFG2: u32 = 0xC00110E3;
    const SUPPRESS_BP_ON_NON_BR: u64 = 1 << 1;
    let mut msr = RealMsrAccess;

    // Zen 2 processors have a configuration bit that mitigates the BTC-NOBR case
    // of the Branch Type Confusion / retbleed vulnerability.
    if x86_feature_test(X86_FEATURE_HYPERVISOR) {
        return;
    }
    if (model.display_family == 0x17)
        && ((model.display_model >= 0x30 && model.display_model <= 0x4F)
            || (model.display_model >= 0x60 && model.display_model <= 0x7F))
    {
        let de_cfg2 = msr.read_msr(DE_CFG2);
        if (de_cfg2 & SUPPRESS_BP_ON_NON_BR) == 0 {
            msr.write_msr(DE_CFG2, de_cfg2 | SUPPRESS_BP_ON_NON_BR);
        }
    }
}

/// Configures `LFENCE` to be dispatch-serializing on AMD Family 10h+ processors.
#[inline]
fn x86_amd_set_lfence_serializing(cpuid: &impl CpuId, msr: &mut impl MsrAccess) {
    // "Software Techniques for Managing Speculation on AMD Processors"
    // Mitigation G-2: Set MSR so that LFENCE is a dispatch-serializing instruction.
    //
    // To mitigate certain speculative execution infoleaks (Spectre) efficiently, configure the
    // CPU to treat LFENCE as a dispatch serializing instruction. This allows code to use LFENCE
    // in contexts to restrict speculative execution.
    if cpuid.read_processor_id().family() >= 0x10
        && !cpuid.read_features().has_feature(Features::LFENCE_RDTSC)
    {
        let de_cfg = msr.read_msr(X86_MSR_AMD_F10_DE_CFG);
        if (de_cfg & X86_MSR_AMD_F10_DE_CFG_LFENCE_SERIALIZE) == 0 {
            msr.write_msr(X86_MSR_AMD_F10_DE_CFG, de_cfg | X86_MSR_AMD_F10_DE_CFG_LFENCE_SERIALIZE);
        }
    }
}

/// All Bulldozer and Zen 1 / Zen 2 CPUs are affected by Retbleed.
#[inline]
pub(super) fn x86_amd_has_retbleed() -> bool {
    let cpuid = RealCpuId;

    // TODO: Check BTC_NO bit.
    let cpu = cpuid.read_processor_id();
    cpu.family() == 0x15 || cpu.family() == 0x17
}

/// Vendor-specific per-CPU initialization for AMD processors.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an AMD processor.
#[inline]
pub(super) unsafe fn x86_amd_init_percpu() {
    let cpuid = RealCpuId;
    let mut msr = RealMsrAccess;
    x86_amd_set_lfence_serializing(&cpuid, &mut msr);
}

/// x86 AMD platform tests.
#[cfg(ktest)]
#[unittest::suite(name = "x86_amd")]
mod tests {
    use super::super::cpuid::tests::CPU_ID_THREAD_RIPPER_2970WX;
    use super::super::fake_msr_access::{FakeMsr, FakeMsrAccess};
    use unittest::expect_eq;

    /// test amd_platform_init
    #[test]
    fn test_amd_platform_init() {
        let mut fake_msrs = FakeMsrAccess::default();

        // Test that set_lfence_serializing sets the LFENCE bit when it's not already set.
        fake_msrs.msrs[0] = FakeMsr { index: X86_MSR_AMD_F10_DE_CFG, value: 0 };
        x86_amd_set_lfence_serializing(&CPU_ID_THREAD_RIPPER_2970WX, &mut fake_msrs);
        expect_eq!(fake_msrs.msrs[0].value, 0x2u64);

        // Test that set_lfence_serializing doesn't change the LFENCE bit when it's set.
        fake_msrs.msrs[0] = FakeMsr { index: X86_MSR_AMD_F10_DE_CFG, value: 0x2 };
        fake_msrs.no_writes = true;
        x86_amd_set_lfence_serializing(&CPU_ID_THREAD_RIPPER_2970WX, &mut fake_msrs);
        expect_eq!(fake_msrs.msrs[0].value, 0x2u64);
    }
}
