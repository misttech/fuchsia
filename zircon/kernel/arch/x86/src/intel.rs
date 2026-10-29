// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use super::cpuid::{CpuId, Features};
use super::feature::{
    X86_FEATURE_HYPERVISOR, X86MicroarchList, get_microarch_config, x86_feature_test,
    x86_get_microarch_config,
};
use super::idle_states::X86IdleState;
use super::platform_access::MsrAccess;
use super::registers::{
    X86_ARCH_CAPABILITIES_RSBA, X86_MSR_IA32_ARCH_CAPABILITIES, X86_MSR_IA32_BIOS_SIGN_ID,
    X86_MSR_IA32_BIOS_UPDT_TRIG, X86_MSR_IA32_PLATFORM_ID, X86_MSR_POWER_CTL,
};
use super::x86::{read_msr, write_msr};

ksync::declare_singleton_spinlock!(MicrocodeLock);

#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, FromBytes, Immutable, KnownLayout)]
struct X86IntelMicrocodeUpdateHeader {
    header_version: u32,
    update_revision: u32,
    date: u32,
    processor_signature: u32,
    checksum: u32,
    loader_revision: u32,
    processor_flags: u32,
    data_size: u32,
    total_size: u32,
    reserved: [u32; 3],
}

zr::static_assert!(core::mem::size_of::<X86IntelMicrocodeUpdateHeader>() == 48);
zr::static_assert!(core::mem::align_of::<X86IntelMicrocodeUpdateHeader>() == 4);

fn microcode_checksum(patch: &[u32]) -> u32 {
    let mut sum: u32 = 0;
    for &dword in patch {
        sum = sum.wrapping_add(dword);
    }
    sum
}

/// Returns whether entering `state` may empty the Return Stack Buffer (RSB).
#[inline]
pub(super) fn x86_intel_idle_state_may_empty_rsb(state: &X86IdleState) -> bool {
    let microarch = x86_get_microarch_config().expect("x86_microarch_config not initialized");
    match microarch.x86_microarch {
        // C-states deeper than C6 may empty the return stack buffer on certain CPUs.
        // Sequences of code that are sensitive to empty RSBs may wish to refill it when it is
        // emptied; return true if a selected idle state may drain this structure.
        X86MicroarchList::X86_MICROARCH_INTEL_SKYLAKE => state.mwait_hint() >= 0x20,
        _ => false,
    }
}

/// Checks whether `patch` is a valid microcode update for the processor described by `cpuid` and
/// `msr`.
fn x86_intel_check_microcode_patch(
    cpuid: &impl CpuId,
    msr: &impl MsrAccess,
    patch: &[u32],
) -> bool {
    // See Intel SDM Volume 3 9.11 "Microcode Update Facilities"
    let processor_signature = cpuid.read_processor_id().signature();
    let platform_id = msr.read_msr(X86_MSR_IA32_PLATFORM_ID);

    let Ok((hdr, _)) = X86IntelMicrocodeUpdateHeader::ref_from_prefix(patch.as_bytes()) else {
        return false;
    };
    // All Intel microcode patches released so far have a header version of 0x1.
    if hdr.header_version != 0x1 {
        return false;
    }
    // Check if this patch is for this processor.
    if hdr.processor_signature != processor_signature {
        return false;
    }
    let platform_id_bits = (platform_id >> 50) & 0x7;
    let flags_match = (hdr.processor_flags & (1u32 << platform_id_bits)) != 0;
    if !flags_match {
        return false;
    }

    let checksum = microcode_checksum(patch);
    checksum == 0
}

/// Attempt to load a compatible microcode patch. Invoked on every logical processor.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an Intel processor and that
/// early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR`. If `msr` writes to
/// hardware `X86_MSR_IA32_BIOS_UPDT_TRIG`, `patch` must contain a valid Intel microcode update
/// whose payload immediately following the 48-byte header is 16-byte aligned and at least
/// `hdr.data_size` bytes (`2000` bytes if `hdr.data_size == 0`).
unsafe fn x86_intel_load_microcode_patch(
    _cpuid: &impl CpuId,
    msr: &mut impl MsrAccess,
    patch: &[u32],
) {
    ksync::lock!(MicrocodeLock::lock_policy::<ksync::IrqSavePolicy>());

    let Ok((hdr, payload)) = X86IntelMicrocodeUpdateHeader::ref_from_prefix(patch.as_bytes())
    else {
        return;
    };
    // SAFETY: Upheld by caller.
    let current_patch_level = unsafe { x86_intel_get_patch_level() };
    // Skip patch if we already have a newer version loaded. This is not required
    // but does save many cycles, especially on hyperthreaded CPUs.
    if hdr.update_revision <= current_patch_level {
        return;
    }

    let data = payload.as_ptr().addr() as u64;
    // Write back & invalidate caches before loading microcode; this is not necessary
    // per the SDM, but Intel posts to LKML indicate it may be required.
    // SAFETY: `wbinvd` writes back and invalidates caches in ring 0.
    unsafe {
        core::arch::asm!("wbinvd", options(nostack, preserves_flags));
    }
    msr.write_msr(X86_MSR_IA32_BIOS_UPDT_TRIG, data);
}

/// Returns the current microcode patch level on Intel processors.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an Intel processor and that
/// early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR`.
#[inline]
pub(super) unsafe fn x86_intel_get_patch_level() -> u32 {
    let mut patch_level: u32 = 0;
    if !x86_feature_test(X86_FEATURE_HYPERVISOR) {
        // Invoking CPUID for leaf 1h fills in the microcode patch level into the high half of
        // X86_MSR_IA32_BIOS_SIGN_ID MSR. Operations between CPUID and RDMSR may clear the MSR;
        // write this sequence in assembly to ensure that there are none.
        // SAFETY: Clears `X86_MSR_IA32_BIOS_SIGN_ID`, executes `cpuid` leaf 1, and reads
        // `X86_MSR_IA32_BIOS_SIGN_ID` in ring 0 when not on a hypervisor.
        unsafe {
            core::arch::asm!(
                "xor eax, eax",
                "xor edx, edx",
                "mov ecx, {msr}",
                "wrmsr", // Clear X86_MSR_IA32_BIOS_SIGN_ID before reading the patch level, per SDM.
                "mov rax, 0x1",
                "mov {rbx_tmp}, rbx",
                "cpuid",
                "mov ecx, {msr}",
                "rdmsr",
                "mov rbx, {rbx_tmp}",
                msr = const X86_MSR_IA32_BIOS_SIGN_ID,
                rbx_tmp = out(reg) _,
                out("rax") _,
                out("ecx") _,
                out("edx") patch_level,
                options(nostack),
            );
        }
    }
    patch_level
}

/// Returns whether the CPU may fall back to an alternative predictor on RSB underflow.
#[inline]
pub(super) fn x86_intel_cpu_has_rsb_fallback(cpuid: &impl CpuId, msr: &impl MsrAccess) -> bool {
    if cpuid.read_features().has_feature(Features::ARCH_CAPABILITIES) {
        let arch_capabilities = msr.read_msr(X86_MSR_IA32_ARCH_CAPABILITIES);
        if (arch_capabilities & X86_ARCH_CAPABILITIES_RSBA) != 0 {
            return true;
        }
    }

    let microarch_config = get_microarch_config(cpuid);
    (microarch_config.x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_SKYLAKE)
        || (microarch_config.x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_CANNONLAKE)
        || (microarch_config.x86_microarch == X86MicroarchList::X86_MICROARCH_UNKNOWN)
}

/// Vendor-specific per-CPU initialization for Intel processors.
///
/// # Safety
///
/// The caller must ensure that the current privilege level is CPL 0 on an Intel processor and that
/// early-boot CPUID feature detection has initialized `X86_FEATURE_HYPERVISOR` and
/// `x86_microarch_config`.
#[inline]
pub(super) unsafe fn x86_intel_init_percpu() {
    // Some intel cpus support auto-entering C1E state when all cores are at C1. In
    // C1E state the voltage is reduced on all cores as well as clock gated. There is
    // a latency associated with ramping the voltage on wake. Disable this feature here
    // to save time on the irq path from idle. (5-10us on skylake nuc from kernel irq
    // handler to user space handler).
    if !x86_feature_test(X86_FEATURE_HYPERVISOR)
        && x86_get_microarch_config().expect("x86_microarch_config not initialized").disable_c1e
    {
        // SAFETY: `X86_MSR_POWER_CTL` is valid on Intel CPUs that set `disable_c1e` when not
        // running under a hypervisor.
        unsafe {
            let power_ctl_msr = read_msr(X86_MSR_POWER_CTL);
            write_msr(X86_MSR_POWER_CTL, power_ctl_msr & !0x2);
        }
    }
}

/// x86 Intel platform tests.
#[cfg(ktest)]
#[unittest::suite(name = "x86_intel")]
mod tests {
    use super::super::cpuid::tests::{
        CPU_ID_CELERON_J3455, FakeCpuId, TEST_DATA_CELERON_J3455, TestDataSet,
    };
    use super::super::fake_msr_access::{FakeMsr, FakeMsrAccess};
    use super::super::feature::{X86VendorList, x86_get_vendor};
    use unittest::{expect_eq, expect_false, expect_true};

    fn intel_make_microcode_checksum(patch: &[u32]) -> u32 {
        microcode_checksum(patch).wrapping_neg()
    }

    /// test Intel x86 microcode patch loader match and load logic
    #[test]
    fn test_x64_intel_ucode_loader() {
        // x86_intel_check_microcode_patch checks if a microcode patch is suitable for a particular
        // CPU. Test that its match logic works for various CPUs and conditions we commonly use.
        let mut fake_patch = kalloc::Box::<[u32]>::try_new_zeroed_slice(512).unwrap();
        let mut fake_msrs = FakeMsrAccess::default();
        fake_msrs.msrs[0] = FakeMsr {
            index: X86_MSR_IA32_PLATFORM_ID,
            value: 0x1u64 << 50, // Apollo Lake
        };

        // Reject a patch shorter than the 48-byte header.
        expect_false!(x86_intel_check_microcode_patch(
            &CPU_ID_CELERON_J3455,
            &fake_msrs,
            &[0u32; 11]
        ));

        // Reject an all-zero patch.
        expect_false!(x86_intel_check_microcode_patch(
            &CPU_ID_CELERON_J3455,
            &fake_msrs,
            &fake_patch
        ));

        // Reject patch with non-matching processor signature.
        fake_patch[0] = 0x1;
        fake_patch[4] = intel_make_microcode_checksum(&fake_patch);
        expect_false!(x86_intel_check_microcode_patch(
            &CPU_ID_CELERON_J3455,
            &fake_msrs,
            &fake_patch
        ));

        // Expect matching patch to pass
        fake_patch[0] = 0x1;
        fake_patch[3] = TEST_DATA_CELERON_J3455.leaf1.reg[0]; // Signature match
        fake_patch[6] = 0x3; // Processor flags match PLATFORM_ID
        fake_patch[4] = 0;
        fake_patch[4] = intel_make_microcode_checksum(&fake_patch);
        expect_true!(x86_intel_check_microcode_patch(
            &CPU_ID_CELERON_J3455,
            &fake_msrs,
            &fake_patch
        ));
        // Real header from 2019-01-15, rev 38
        fake_patch[0] = 0x1;
        fake_patch[1] = 0x38;
        fake_patch[2] = 0x01152019;
        fake_patch[3] = 0x506c9;
        fake_patch[6] = 0x3; // Processor flags match PLATFORM_ID
        fake_patch[4] = 0;
        fake_patch[4] = intel_make_microcode_checksum(&fake_patch);
        expect_true!(x86_intel_check_microcode_patch(
            &CPU_ID_CELERON_J3455,
            &fake_msrs,
            &fake_patch
        ));
    }

    #[derive(Default)]
    struct FakeWriteMsr {
        written: bool,
        msr_index: u32,
    }

    impl MsrAccess for FakeWriteMsr {
        fn read_msr(&self, _msr_index: u32) -> u64 {
            panic!("unexpected read_msr");
        }

        fn write_msr(&mut self, msr_index: u32, _value: u64) {
            debug_assert!(!self.written);
            self.written = true;
            self.msr_index = msr_index;
        }
    }

    /// test Intel x86 microcode patch loader mechanism
    #[test]
    fn test_x64_intel_ucode_patch_loader() {
        // This test can only run on physical Intel x86-64 hosts; x86_intel_get_patch_level
        // does not use an interface to access patch_level registers and those registers are
        // only present/writable on h/w.
        if x86_get_vendor() == X86VendorList::X86_VENDOR_INTEL
            && !x86_feature_test(X86_FEATURE_HYPERVISOR)
        {
            static DATA: TestDataSet = TestDataSet::EMPTY;
            let cpu = FakeCpuId::new(&DATA);
            let mut msrs = FakeWriteMsr::default();
            let mut fake_patch = kalloc::Box::<[u32]>::try_new_zeroed_slice(512).unwrap();

            // Expect that a patch == current patch is not loaded.
            // SAFETY: Guarded by physical Intel x86-64 host check in ring 0.
            let current_patch_level = unsafe { x86_intel_get_patch_level() };
            fake_patch[1] = current_patch_level;
            // SAFETY: Guarded by physical Intel x86-64 host check in ring 0, and `msrs` is a fake
            // MSR sink that does not trigger a hardware microcode load.
            unsafe { x86_intel_load_microcode_patch(&cpu, &mut msrs, &fake_patch) };
            expect_false!(msrs.written);

            // Expect that a newer patch is loaded.
            if let Some(next_level) = current_patch_level.checked_add(1) {
                fake_patch[1] = next_level;
                // SAFETY: Guarded by physical Intel x86-64 host check in ring 0, and `msrs` is a
                // fake MSR sink that does not trigger a hardware microcode load.
                unsafe { x86_intel_load_microcode_patch(&cpu, &mut msrs, &fake_patch) };
                expect_true!(msrs.written);
                expect_eq!(msrs.msr_index, X86_MSR_IA32_BIOS_UPDT_TRIG);
            }
        }
    }
}
