// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Host test crate root for the ARM SMMU driver port.
//!
//! This crate is used during Checkpoints 1–3 of the migration (`SMMUv2_rust_port_plan.md` §3) to
//! compile the `no_std` driver modules as a host library and expose the FFI entrypoints consumed by
//! the `C++ -> Rust` and `Rust -> C++` cross-test suites.

#![no_std]

// `host_lib` compiles the full `arm_smmu` driver crate into a standalone host `staticlib` for
// cross-testing; kernel-only entrypoints and unused internal helpers are expected in `arm_smmu`.
#[allow(dead_code)]
#[path = "../mod.rs"]
pub mod arm_smmu;
pub mod cross_test_ffi;

use zx_status::Status;

use crate::arm_smmu::common::{
    ArmSmmuMode, DeviceAspace, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, PAGE_SIZE, PageCache,
    PinnedVmoSource, TlbInvalidator, get_smmu_mode, validate_smmu_mode_string,
};
use crate::arm_smmu::v2::{
    AddrMode, Bitmask128, BtiMode, CbarType, ConsoleArg, ContextBank, IrqDef, S2crType, Smmu,
    SmmuBti, SmmuMmio, SmmuParams, SmmuPmt, SmmuPmtState, SmrValue, StreamMatchRegGroup,
    ZbiDcfgArmSmmuDriver, ZbiDcfgArmSmmuIrq, ZxIommuDescArmSmmu, gr0, gr1, s1cbr, smr_intersects,
};

struct SmokePinnedVmo {
    offset: u64,
    size: u64,
    base_pa: u64,
}

impl PinnedVmoSource for SmokePinnedVmo {
    fn offset(&self) -> u64 {
        self.offset
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        if size != PAGE_SIZE || offset < self.offset || (offset - self.offset) >= self.size {
            return Err(Status::INVALID_ARGS);
        }
        Ok(self.base_pa + (offset - self.offset))
    }
}

struct SmokeInvalidator {
    count: usize,
    last_base: u64,
    last_size: u64,
}

impl TlbInvalidator for SmokeInvalidator {
    fn invalidate(&mut self, base: u64, size: u64) {
        self.count += 1;
        self.last_base = base;
        self.last_size = size;
    }
}

/// Executes a host smoke test exercising
/// `common/{smmu_mode,page_cache,translation_table,device_aspace}` and
/// `v2/{utils,registers,stream_match_reg,context_bank}`, verifying that all `SmmuEnv` FFI hooks
/// link and run against `smmu_env_ffi.cc` and `PmmMock`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_arm_smmu_common_smoke_test() -> bool {
    if get_smmu_mode("disabled, 0x1234 = passthru, 0x5678 = enforced", Some(0x1234))
        != Some(ArmSmmuMode::Passthru)
    {
        return false;
    }
    if !validate_smmu_mode_string("disabled, 0x1234 = passthru")
        || validate_smmu_mode_string("disabled,")
    {
        return false;
    }

    // Exercise Bitmask128 and SmrValue (including boundary cases `n == 0` and `n == 128`).
    let mut mask = Bitmask128::new();
    mask.set_lowest_n_bits(0);
    if mask.num_set_bits() != 0 || mask.find_first_set_bit().is_some() {
        return false;
    }
    mask.set_lowest_n_bits(128);
    if mask.num_set_bits() != 128
        || mask.find_first_clr_bit().is_some()
        || !mask.test_bit(0)
        || !mask.test_bit(127)
    {
        return false;
    }
    mask.clear_all();
    mask.set_lowest_n_bits(4);
    mask.clear_bit(1);
    mask.set_bit(70);
    if !mask.test_bit(0)
        || mask.test_bit(1)
        || !mask.test_bit(2)
        || !mask.test_bit(70)
        || mask.num_set_bits() != 4
        || mask.find_first_set_bit() != Some(0)
    {
        return false;
    }

    // `smr_a` sets bit 0 in both `id` (`0x0013`) and `mask` (`0x0005`, wildcarding bits 0 and 2)
    // to exercise `SmrValueIter` clearing wildcarded ID bits and ripple-carrying across fixed bits.
    let smr_a = SmrValue::new(0x0013, 0x0005);
    let smr_b = SmrValue::new(0x0012, 0x0000);
    let smr_c = SmrValue::new(0x0022, 0x0001);
    if !smr_a.intersects(smr_b)
        || smr_a.intersects(smr_c)
        || !smr_intersects(smr_a.value(), smr_b.value())
        || smr_a.num_streams() != 4
    {
        return false;
    }
    let mut smr_iter = smr_a.iter();
    if smr_iter.next() != Some(0x0012)
        || smr_iter.next() != Some(0x0013)
        || smr_iter.next() != Some(0x0016)
        || smr_iter.next() != Some(0x0017)
        || smr_iter.next().is_some()
    {
        return false;
    }

    // Exercise StreamMatchRegGroup and ContextBank over a heap-backed MMIO buffer.
    let Ok(mut fake_regs) = fbl::Array::<u64>::try_new(512 * 6) else {
        return false;
    };
    fake_regs.fill(0);
    let base_vaddr = fake_regs.as_mut_ptr().expose_provenance();
    // SAFETY: `fake_regs` is a valid, 8-byte-aligned heap buffer of `6 * 4096` bytes with exposed
    // provenance that outlives `gr0_base`, `gr1_base`, `cb0_base`, and `smmu`.
    let (gr0_base, gr1_base, cb0_base) = unsafe {
        (
            SmmuMmio::with_size(base_vaddr, 4096),
            SmmuMmio::with_size(base_vaddr + 4096, 4096),
            SmmuMmio::with_size(base_vaddr + 8192, 4 * 4096),
        )
    };
    let mut avail_smrgs = Bitmask128::new();
    avail_smrgs.set_lowest_n_bits(4);
    let mut avail_cbs = Bitmask128::new();
    avail_cbs.set_lowest_n_bits(4);
    let smmu = SmmuParams {
        name: "SMMU@test",
        op_mode: ArmSmmuMode::Enforced,
        num_smrgs: 4,
        num_cbs: 4,
        reg_page_size: 4096,
        gr0_base,
        gr1_base,
        cb0_base,
        available_smrgs: &avail_smrgs,
        available_cbs: &avail_cbs,
    };

    // Exercise `StreamMatchRegGroup::create_and_adopt` failure paths (`S2crType::Invalid` and
    // out-of-range `cbndx`) to verify that `Drop`'s debug assertions do not trip when adoption
    // fails.
    gr0::S2cr::from_value(0).set_s2cr_type(S2crType::Invalid).write_at(&gr0_base, 0);
    if StreamMatchRegGroup::create_and_adopt(&smmu, 0, smr_a).err() != Some(Status::BAD_STATE) {
        return false;
    }
    gr0::S2crType0::from_value(0)
        .set_s2cr_type(S2crType::Translation)
        .set_cbndx(99)
        .write_at(&gr0_base, 0);
    if StreamMatchRegGroup::create_and_adopt(&smmu, 0, smr_a).err() != Some(Status::OUT_OF_RANGE) {
        return false;
    }

    let Ok(mut smrg) = StreamMatchRegGroup::create_and_lockdown(&smmu, 0, smr_a) else {
        return false;
    };
    let Ok(mut cb) = ContextBank::create_and_lockdown(&smmu, 0) else {
        smrg.disable();
        return false;
    };
    smrg.enable_for_context_bank(cb.cb_ndx());
    if smrg.mode() != S2crType::Translation
        || gr0::Smr::read_at(&gr0_base, 0).valid() != 1
        || gr0::Smr::read_at(&gr0_base, 0).mask() != u32::from(smr_a.mask())
        || gr0::Smr::read_at(&gr0_base, 0).id() != u32::from(smr_a.id())
        || gr0::S2cr::read_at(&gr0_base, 0).s2cr_type() != S2crType::Translation
    {
        smrg.disable();
        cb.set_mode(smmu.name, BtiMode::Shutdown);
        return false;
    }

    cb.set_mode(smmu.name, BtiMode::Bypass);
    if cb.mode() != BtiMode::Bypass
        || cb.addr_mode() != AddrMode::Mode64Bit
        || gr1::Cbar::read_at(&gr1_base, 0).cbar_type() != CbarType::S1TS2Bypass
        || s1cbr::Sctlr::read_from(&cb0_base).m() != 0
    {
        smrg.disable();
        cb.set_mode(smmu.name, BtiMode::Shutdown);
        return false;
    }

    // Exercise `BtiMode::Fault` transition and its debug-assertion register checks.
    cb.set_mode(smmu.name, BtiMode::Fault);
    if cb.mode() != BtiMode::Fault
        || gr1::Cbar::read_at(&gr1_base, 0).cbar_type() != CbarType::S1TS2Bypass
        || s1cbr::Sctlr::read_from(&cb0_base).m() != 1
    {
        smrg.disable();
        cb.set_mode(smmu.name, BtiMode::Shutdown);
        return false;
    }

    smrg.disable();
    cb.set_mode(smmu.name, BtiMode::Shutdown);

    // Exercise PageCache LIFO ordering and trimming.
    let mut cache = PageCache::new();
    let Ok(p1) = cache.get_page() else {
        return false;
    };
    let Ok(p2) = cache.get_page() else {
        return false;
    };
    // SAFETY: `p1` and `p2` were just checked out from `cache`, are unlinked, and remain zeroed
    // and cache-cleaned.
    unsafe {
        cache.return_page(p1);
        cache.return_page(p2);
    }
    let Ok(pop1) = cache.get_page() else {
        return false;
    };
    let Ok(pop2) = cache.get_page() else {
        return false;
    };
    if pop1 != p2 || pop2 != p1 {
        return false;
    }
    // SAFETY: `pop1` and `pop2` were just checked out from `cache`, are unlinked, and remain
    // zeroed and cache-cleaned.
    unsafe {
        cache.return_page(pop1);
        cache.return_page(pop2);
    }
    cache.trim(0);

    // Exercise DeviceAspace map/unmap straddling all 4 translation table levels.
    let aspace_start = 1u64 << 20;
    let aspace_end = 1u64 << 48;
    let Ok(mut aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let vmo = SmokePinnedVmo { offset: 0, size: 2 * PAGE_SIZE, base_pa: 0x1000 };
    let mut inval = SmokeInvalidator { count: 0, last_base: 0, last_size: 0 };
    let target_loc = (1u64 << 39) - PAGE_SIZE;

    let Ok(alloc) = aspace.map(
        &vmo,
        IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE,
        &mut inval,
        Some(target_loc),
    ) else {
        return false;
    };
    if alloc.base() != target_loc || alloc.size() != 2 * PAGE_SIZE || inval.count != 1 {
        return false;
    }

    aspace.unmap(alloc, &mut inval);
    if inval.count != 2 || inval.last_base != target_loc || inval.last_size != 2 * PAGE_SIZE {
        return false;
    }

    aspace.free_translation_tables(&mut inval);
    if inval.count != 3 || inval.last_base != 0 || inval.last_size != 0 {
        return false;
    }

    // Exercise IrqDef, ZbiDcfgArmSmmuDriver, ZxIommuDescArmSmmu, SmmuPmtState, and
    // Smmu::console_cmd.
    let _ = core::marker::PhantomData::<(SmmuBti, SmmuPmt, ZbiDcfgArmSmmuDriver)>;
    let irq = IrqDef::from_zbi(ZbiDcfgArmSmmuIrq { num: 32, flags: 1 });
    let desc = ZxIommuDescArmSmmu::default();
    let help_args = [ConsoleArg { str_val: "smmu", u: 0 }, ConsoleArg { str_val: "help", u: 0 }];
    irq.valid()
        && desc.register_base == 0
        && SmmuPmtState::Active != SmmuPmtState::Quarantined
        && Smmu::console_cmd(&help_args, 0) == 0
}

/// Runs the native Rust `SmmuModeDefaults` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_smmu_mode_defaults() -> bool {
    crate::arm_smmu::unit_tests::test_smmu_mode_defaults()
}

/// Runs the native Rust `SmmuModeAdvanced` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_smmu_mode_advanced() -> bool {
    crate::arm_smmu::unit_tests::test_smmu_mode_advanced()
}

/// Runs the native Rust `SmmuModeCaseInsensitive` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_smmu_mode_case_insensitive() -> bool {
    crate::arm_smmu::unit_tests::test_smmu_mode_case_insensitive()
}

/// Runs the native Rust `SmmuModeWhitespace` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_smmu_mode_whitespace() -> bool {
    crate::arm_smmu::unit_tests::test_smmu_mode_whitespace()
}

/// Runs the native Rust `SmmuModeValidation` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_smmu_mode_validation() -> bool {
    crate::arm_smmu::unit_tests::test_smmu_mode_validation()
}

/// Runs the native Rust `Vmsav8_64Descriptors` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_vmsav8_64_descriptors() -> bool {
    crate::arm_smmu::unit_tests::test_vmsav8_64_descriptors()
}

/// Runs the native Rust `SmrValueAndIter` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_smr_value_and_iter() -> bool {
    crate::arm_smmu::unit_tests::test_smr_value_and_iter()
}

/// Runs the native Rust `Bitmask128` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_bitmask128() -> bool {
    crate::arm_smmu::unit_tests::test_bitmask128()
}

/// Runs the native Rust `PageCacheAddAndGet` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_page_cache_add_and_get() -> bool {
    crate::arm_smmu::unit_tests::test_page_cache_add_and_get()
}

/// Runs the native Rust `PageCacheTrim` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_page_cache_trim() -> bool {
    crate::arm_smmu::unit_tests::test_page_cache_trim()
}

/// Runs the native Rust `PageCacheOom` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_page_cache_oom() -> bool {
    crate::arm_smmu::unit_tests::test_page_cache_oom()
}

/// Runs the native Rust `TranslationTableHelperWalkAndReclaim` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_translation_table_helper_walk_and_reclaim() -> bool {
    crate::arm_smmu::unit_tests::test_translation_table_helper_walk_and_reclaim()
}

/// Runs the native Rust `DeviceAspaceMapAndUnmap` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_device_aspace_map_and_unmap() -> bool {
    crate::arm_smmu::unit_tests::test_device_aspace_map_and_unmap()
}

/// Runs the native Rust `DeviceAspaceMapAtSpecificLocation` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_device_aspace_map_at_specific_location() -> bool {
    crate::arm_smmu::unit_tests::test_device_aspace_map_at_specific_location()
}

/// Runs the native Rust `DeviceAspaceMapStraddlingTableLevels` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_device_aspace_map_straddling_table_levels() -> bool {
    crate::arm_smmu::unit_tests::test_device_aspace_map_straddling_table_levels()
}

/// Runs the native Rust `DeviceAspaceMapFailureInjection` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_device_aspace_map_failure_injection() -> bool {
    crate::arm_smmu::unit_tests::test_device_aspace_map_failure_injection()
}

/// Runs the native Rust `DeviceAspaceSharedTableLevels` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_device_aspace_shared_table_levels() -> bool {
    crate::arm_smmu::unit_tests::test_device_aspace_shared_table_levels()
}

/// Runs the native Rust `DeviceAspaceMapAtLocationEdgeCases` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_device_aspace_map_at_location_edge_cases() -> bool {
    crate::arm_smmu::unit_tests::test_device_aspace_map_at_location_edge_cases()
}

/// Runs the native Rust `StreamMatchRegGroup` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_stream_match_reg_group() -> bool {
    crate::arm_smmu::unit_tests::test_stream_match_reg_group()
}

/// Runs the native Rust `ContextBank` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_context_bank() -> bool {
    crate::arm_smmu::unit_tests::test_context_bank()
}

/// Runs the native Rust `RegistersAndBitfields` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_registers_and_bitfields() -> bool {
    crate::arm_smmu::unit_tests::test_registers_and_bitfields()
}

/// Runs the native Rust `EscapedName` unit test (`Rust -> Rust`).
#[unsafe(no_mangle)]
pub extern "C" fn rust_unit_test_escaped_name() -> bool {
    crate::arm_smmu::unit_tests::test_escaped_name()
}
