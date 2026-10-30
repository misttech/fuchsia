// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Fine-grained native Rust (`Rust -> Rust`) unit tests for the ARM SMMUv2 driver components.
//!
//! This module is shared between the standalone host test binary (`fx test --host`) and the
//! in-kernel `#[cfg(ktest)]` test suite, so assertions return `bool` via [`check!`] and
//! [`check_eq!`] rather than panicking or depending on `std`.
//!
//! Covers all 10 component areas:
//! 1. `smmu_mode` boot option string validation and parsing.
//! 2. `vmsav8_64` stage-1 page/table descriptor encoding, permissions, and atomic PTE stores.
//! 3. `SmrValue` stream-matching construction, intersection, containment, and iteration.
//! 4. `Bitmask128` 128-bit resource mask operations and set-bit iteration.
//! 5. `PageCache` LIFO allocation, zero-filling, FIFO tail trimming, and OOM handling.
//! 6. `TranslationTableHelper` 4-level cursor walks, PoC cache cleaning, and table reclamation.
//! 7. `DeviceAspace` map/unmap, specific-location placement, straddling levels, OOM rollback,
//!    shared table levels, and recursive 4-level page-table verification.
//! 8. `StreamMatchRegGroup` lockdown, adoption, context-bank binding, invalidation, and teardown.
//! 9. `ContextBank` lockdown, mode transitions, adoption (32-bit, Ext32Bit, 64-bit), fault IRQ
//!    toggling, and ASID/VA TLB invalidation.
//! 10. `define_smmu_reg!` 32-bit/64-bit/indexed register bitfields, `RSVDZ_MASK`, and metadata.
//!
//! Plus the `EscapedName` formatter used when printing object names in diagnostics.

use core::ffi::c_char;
use core::fmt::Write as _;
use core::pin::Pin;
use core::sync::atomic::Ordering;

use debug::dprintf;
use pin_init::PinInit;
use region_alloc::{AllowOverlap, RegionAllocator, RegionSpan, TestRegionSet};
use zx_status::Status;

use super::common::translation_table::{LEVELS, TranslationTableHelper, table_slice};
use super::common::vmsav8_64::{
    ENTRIES_PER_PAGE, PAGE_ENTRY_ACCESS_FLAG_BIT, PAGE_ENTRY_BIT, PERM_LUT, TABLE_ENTRY_BIT,
    VALID_ADDR_MASK, VALID_ENTRY_BIT, get_page_entry_perms, get_table_entry_paddr, is_block_entry,
    is_table_entry, is_valid_addr, is_valid_entry, make_page_entry, make_perms, make_table_entry,
    write_pte,
};
use super::common::{
    ALL_PERM_FLAGS, ArmSmmuMode, DEFAULT_TCR_T0SZ, DeviceAspace, EscapedName,
    IOMMU_FLAG_PERM_EXECUTE, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, PAGE_MASK, PAGE_SHIFT,
    PAGE_SIZE, PAGE_SIZE_USIZE, PageCache, PinnedVmoSource, SmmuEnv, SmmuPage, TlbInvalidator,
    get_smmu_mode, get_smmu_mode_bytes, get_smmu_mode_cstr, validate_smmu_mode_cstr,
    validate_smmu_mode_string,
};
use super::v2::context_bank::{INVALIDATE_ALL_PAGE_COUNT_THRESHOLD, UNUSED_ASID};
use super::v2::registers::{
    Cba2rVa64, CbarType, ClientPd, ExidEnable, FieldDesc, S2crPrivCfg, S2crType, SmmuMmio, Stalld,
    TcrIrg0, TcrOrg0, TcrSh0, TcrTg0, TcrTg1, gr0, gr1, s1cbr, s2cbr,
};
use super::v2::smmu::TagBuf;
use super::v2::{
    AddrMode, Bitmask128, BtiMode, ContextBank, SmmuController, SmmuParams, SmrValue,
    StreamMatchRegGroup, addr_mode_to_string, arm_cbar_type_to_string, arm_s2cr_type_to_string,
    arm_smmu_mode_to_string, bti_mode_to_string, smr_intersects,
};

// Lightweight boolean assertion macros shared by both the host FFI test runner (`cross_test_ffi`)
// and the in-kernel `#[ktest]` harness, where panicking on failure is undesirable.
macro_rules! check {
    ($cond:expr) => {
        if !($cond) {
            dprintf!(INFO, "UNIT TEST FAILURE at {}:{}: {}\n", file!(), line!(), stringify!($cond));
            return false;
        }
    };
}

macro_rules! check_eq {
    ($left:expr, $right:expr) => {
        match (&($left), &($right)) {
            (left_val, right_val) => {
                if !(*left_val == *right_val) {
                    dprintf!(
                        INFO,
                        "UNIT TEST FAILURE at {}:{}: {} != {}\n",
                        file!(),
                        line!(),
                        stringify!($left),
                        stringify!($right)
                    );
                    return false;
                }
            }
        }
    };
}

// ============================================================================
// 1. `smmu_mode` Unit Tests
// ============================================================================

/// Tests default SMMU mode strings (`"disabled"`, `"passthru"`, `"enforced"`) and invalid default
/// tokens (`""`, `"pass thru"`, `"pass-thru"`, `"asdfjkldsf"`).
pub fn test_smmu_mode_defaults() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeDefaults\n");

    let cases: &[(&str, &[u8], ArmSmmuMode)] = &[
        ("disabled", b"disabled\0", ArmSmmuMode::Disabled),
        ("passthru", b"passthru\0", ArmSmmuMode::Passthru),
        ("enforced", b"enforced\0", ArmSmmuMode::Enforced),
    ];

    for &(mode_str, cstr_bytes, expected) in cases {
        check!(validate_smmu_mode_string(mode_str));
        check_eq!(get_smmu_mode(mode_str, None), Some(expected));
        check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(expected));
        check_eq!(get_smmu_mode_bytes(mode_str.as_bytes(), None), Some(expected));
        check_eq!(get_smmu_mode_bytes(mode_str.as_bytes(), Some(0x1234)), Some(expected));

        let c_ptr = cstr_bytes.as_ptr().cast::<c_char>();
        // SAFETY: `cstr_bytes` is a valid NUL-terminated byte slice.
        unsafe {
            check!(validate_smmu_mode_cstr(c_ptr));
            check_eq!(get_smmu_mode_cstr(c_ptr, None), Some(expected));
            check_eq!(get_smmu_mode_cstr(c_ptr, Some(0x1234)), Some(expected));
        }
    }

    // Invalid default mode strings must return `None`.
    for &invalid in &["", "pass thru", "pass-thru", "asdfjkldsf"] {
        check!(!validate_smmu_mode_string(invalid));
        check_eq!(get_smmu_mode(invalid, None), None);
        check_eq!(get_smmu_mode(invalid, Some(0x1234)), None);
    }

    // Null C-string pointers must be rejected cleanly without dereferencing.
    // SAFETY: Testing explicit null pointer handling in `validate_smmu_mode_cstr` and
    // `get_smmu_mode_cstr`.
    unsafe {
        check!(!validate_smmu_mode_cstr(core::ptr::null()));
        check_eq!(get_smmu_mode_cstr(core::ptr::null(), None), None);
    }

    true
}

/// Tests per-base-address overrides, first-match-wins semantics, radix parsing, and malformed
/// segment skipping in `get_smmu_mode`.
pub fn test_smmu_mode_advanced() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeAdvanced\n");

    // Base address matching and fallback to default.
    check_eq!(get_smmu_mode("disabled,0x1234=passthru", Some(0x1234)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode("disabled,0x1234=passthru", Some(0x5678)), Some(ArmSmmuMode::Disabled));

    // Multiple entries.
    let mode_str = "disabled,0x1234=passthru,0x5678=enforced";
    check!(validate_smmu_mode_string(mode_str));
    check_eq!(get_smmu_mode(mode_str, None), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode(mode_str, Some(0x5678)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(ArmSmmuMode::Passthru));

    // First match wins.
    check_eq!(
        get_smmu_mode("disabled,0x1234=passthru,0x1234=enforced", Some(0x1234)),
        Some(ArmSmmuMode::Passthru)
    );

    // Invalid entries: missing `=` is skipped and falls back to default; invalid mode for a
    // matched address returns `None`.
    check_eq!(get_smmu_mode("disabled,0x1234passthru", Some(0x1234)), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode("disabled,0x1234=invalid", Some(0x1234)), None);
    check_eq!(get_smmu_mode("disabled,0x1234=", Some(0x1234)), None);

    // Decimal, octal, and hex radix parsing.
    let radix_str = "disabled, 4660 = passthru, 01234 = enforced, 0xabcd = passthru";
    check!(validate_smmu_mode_string(radix_str));
    check_eq!(get_smmu_mode(radix_str, Some(0x1234)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(radix_str, Some(0o1234)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(radix_str, Some(0xabcd)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(radix_str, Some(0x9999)), Some(ArmSmmuMode::Disabled));

    // The `<base_addr>` grammar matches kernel `strtoul(..., 0)` semantics, quirks included.
    //
    // An uppercase `0X` prefix is not recognized, so the segment is malformed.
    check!(!validate_smmu_mode_string("disabled,0X1234=enforced"));
    check_eq!(get_smmu_mode("disabled,0X1234=enforced", Some(0x1234)), Some(ArmSmmuMode::Disabled));
    // Uppercase hex digits after a lowercase prefix are fine.
    check_eq!(get_smmu_mode("disabled,0xABCD=enforced", Some(0xabcd)), Some(ArmSmmuMode::Enforced));
    // Leading signs: `+` is ignored and `-` negates in two's complement.
    check_eq!(get_smmu_mode("disabled,+0x10=enforced", Some(0x10)), Some(ArmSmmuMode::Enforced));
    check!(validate_smmu_mode_string("disabled,-1=enforced"));
    check_eq!(get_smmu_mode("disabled,-1=enforced", Some(u64::MAX)), Some(ArmSmmuMode::Enforced));
    check_eq!(
        get_smmu_mode("disabled,-0x10=enforced", Some(0x10u64.wrapping_neg())),
        Some(ArmSmmuMode::Enforced)
    );
    // Overflow saturates to `u64::MAX` (and is then not negated).
    for overflow in [
        "disabled,0x1ffffffffffffffff=enforced",
        "disabled,99999999999999999999999=enforced",
        "disabled,-0x1ffffffffffffffff=enforced",
    ] {
        check!(validate_smmu_mode_string(overflow));
        check_eq!(get_smmu_mode(overflow, Some(u64::MAX)), Some(ArmSmmuMode::Enforced));
    }
    check_eq!(
        get_smmu_mode("disabled,0xffffffffffffffff=enforced", Some(u64::MAX)),
        Some(ArmSmmuMode::Enforced)
    );
    // A sign or prefix with no digits parses as zero.
    for zero in ["disabled,0x=enforced", "disabled,+=enforced", "disabled,-=enforced"] {
        check!(validate_smmu_mode_string(zero));
        check_eq!(get_smmu_mode(zero, Some(0)), Some(ArmSmmuMode::Enforced));
    }
    // An empty token, digits invalid for the radix, and trailing garbage are all rejected.
    for bad in [
        "disabled,=enforced",
        "disabled, =enforced",
        "disabled,08=enforced",
        "disabled,0xg=enforced",
        "disabled,0x 12=enforced",
        "disabled,12q=enforced",
        "disabled,--1=enforced",
    ] {
        check!(!validate_smmu_mode_string(bad));
        check_eq!(get_smmu_mode(bad, Some(0)), Some(ArmSmmuMode::Disabled));
    }

    // Skipping bad address segments to find a later match under `TolerateErrors`.
    check_eq!(
        get_smmu_mode("disabled,invalid_segment,0x1234=passthru", Some(0x1234)),
        Some(ArmSmmuMode::Passthru)
    );
    check_eq!(
        get_smmu_mode("disabled,0x1234passthru,0x5678=enforced", Some(0x5678)),
        Some(ArmSmmuMode::Enforced)
    );

    true
}

/// Tests ASCII case-insensitivity of SMMU mode tokens and hexadecimal base addresses.
pub fn test_smmu_mode_case_insensitive() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeCaseInsensitive\n");

    check_eq!(get_smmu_mode("DiSaBlEd", None), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode("PASSTHRU", None), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode("EnFoRcEd", None), Some(ArmSmmuMode::Enforced));

    let mode_str = "EnFoRcEd, 0x1234 = Disabled, 0x5678 = ENFORCED, 0x9ABC = pAsStHrU";
    check!(validate_smmu_mode_string(mode_str));
    check_eq!(get_smmu_mode(mode_str, None), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode(mode_str, Some(0x5678)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x9abc)), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(mode_str, Some(0x0001)), Some(ArmSmmuMode::Enforced));

    true
}

/// Tests whitespace trimming (`' '`, `'\t'`, `'\n'`, `'\r'`) around commas, `=`, and tokens.
pub fn test_smmu_mode_whitespace() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeWhitespace\n");

    check_eq!(get_smmu_mode("  passthru  ", None), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(" disabled ", None), Some(ArmSmmuMode::Disabled));

    let mode_str = " \t\n\r passthru \t\n\r , \t\n\r 0x1234 \t\n\r = \t\n\r disabled \
                    \t\n\r , \t\n\r 0x5678=enforced";
    check!(validate_smmu_mode_string(mode_str));
    check_eq!(get_smmu_mode(mode_str, None), Some(ArmSmmuMode::Passthru));
    check_eq!(get_smmu_mode(mode_str, Some(0x1234)), Some(ArmSmmuMode::Disabled));
    check_eq!(get_smmu_mode(mode_str, Some(0x5678)), Some(ArmSmmuMode::Enforced));
    check_eq!(get_smmu_mode(mode_str, Some(0x0001)), Some(ArmSmmuMode::Passthru));

    // Whitespace-only string is invalid (equivalent to empty).
    check!(!validate_smmu_mode_string("   \t\n\r  "));
    check_eq!(get_smmu_mode("   \t\n\r  ", None), None);

    true
}

/// Tests validation of valid and malformed SMMU mode strings (including radix errors, integer
/// overflow, sign prefixes, and delimiter edge cases).
pub fn test_smmu_mode_validation() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmmuModeValidation\n");

    let valid_strings: &[&str] = &[
        "disabled",
        "passthru",
        "enforced",
        "  disabled  ",
        "disabled,0x1234=passthru",
        "disabled , 0x1234 = passthru",
        "disabled,0x1234=passthru,0x5678=enforced",
        "disabled, 1234 = passthru",
        "disabled, 01234 = passthru",
        "disabled, +1 = passthru",
        "disabled, 0 = passthru",
        "disabled, 0x0 = passthru",
        "disabled, 0xffffffffffffffff = passthru",
        // Quirks of kernel `strtoul(..., 0)` reproduced by the `<base_addr>` grammar: overflow
        // saturates, a leading `-` negates, and a sign or `0x` prefix with no digits parses as
        // zero.
        "disabled, 0x10000000000000000 = passthru",
        "disabled, -1 = passthru",
        "disabled, + = passthru",
        "disabled, 0x = passthru",
    ];
    for &s in valid_strings {
        check!(validate_smmu_mode_string(s));
        check!(get_smmu_mode(s, None).is_some());
    }

    let invalid_strings: &[&str] = &[
        "",
        "pass thru",
        "invalid",
        "disabled,",                              // Trailing comma
        "disabled,,",                             // Double comma
        ",disabled",                              // Leading comma
        "0x1234 = passthru",                      // Missing initial default mode
        "invalid_segment,0x1234=passthru",        // Bad segment first
        "disabled,0x1234passthru",                // Malformed address (missing '=')
        "disabled,0x1234=invalid",                // Invalid mode
        "disabled,0x1234=",                       // Empty mode
        "disabled, 0x1234",                       // Missing '=mode'
        "disabled, = passthru",                   // Empty address
        "disabled, 0x1234 = passthru = enforced", // Extra '='
        "disabled, not_a_number = passthru",      // Malformed address
        "disabled, 0x1234extra = passthru",       // Malformed address with trailing chars
        "disabled, 08 = passthru",
        "disabled, 0X1234 = passthru",
    ];
    for &s in invalid_strings {
        check!(!validate_smmu_mode_string(s));
    }

    true
}

// ============================================================================
// 2. `vmsav8_64` Unit Tests
// ============================================================================

/// Tests VMSAv8-64 stage-1 address validation, table/block/page descriptor encoding, permission
/// lookup table invariants (`AF == 1`, `DBM == 0`), and atomic PTE writes.
pub fn test_vmsav8_64_descriptors() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] Vmsav8_64Descriptors\n");

    check_eq!(ENTRIES_PER_PAGE, 512);
    check_eq!(VALID_ADDR_MASK, 0x0000_FFFF_FFFF_F000);

    // Address alignment and 48-bit range checks.
    check!(is_valid_addr(0));
    check!(is_valid_addr(PAGE_SIZE));
    check!(is_valid_addr(0x0000_FFFF_FFFF_F000));
    check!(!is_valid_addr(1));
    check!(!is_valid_addr(PAGE_MASK));
    check!(!is_valid_addr(PAGE_SIZE + 1));
    check!(!is_valid_addr(1u64 << 48));
    check!(!is_valid_addr(0xFFFF_0000_0000_0000));

    // Invalid, block, and table descriptor classification across levels 0..=3.
    check!(!is_valid_entry(0));
    check!(!is_table_entry(0));
    for level in 0..=3 {
        check!(!is_block_entry(0, level));
        check!(!is_block_entry(TABLE_ENTRY_BIT, level));
    }

    // A descriptor with bit 0 = 1 and bit 1 = 0 is a valid block entry at levels 0..=2, but never
    // at level 3.
    let block_desc = 0x0000_1234_0000_0000 | VALID_ENTRY_BIT;
    check!(is_valid_entry(block_desc));
    check!(!is_table_entry(block_desc));
    check!(is_block_entry(block_desc, 0));
    check!(is_block_entry(block_desc, 1));
    check!(is_block_entry(block_desc, 2));
    check!(!is_block_entry(block_desc, 3));

    // Table descriptor round-trip.
    let table_pa: u64 = 0x0000_ABCD_EF01_2000;
    let table_desc = make_table_entry(table_pa);
    check!(is_valid_entry(table_desc));
    check!(is_table_entry(table_desc));
    for level in 0..=3 {
        check!(!is_block_entry(table_desc, level));
    }
    check_eq!(get_table_entry_paddr(table_desc), table_pa);
    check_eq!(table_desc & 0x3, VALID_ENTRY_BIT | TABLE_ENTRY_BIT);

    // Page descriptor encoding and `PERM_LUT` invariants across all `0..=7` RWX combinations.
    const DBM_BIT: u64 = 1u64 << 51;
    const PERM_BITS_MASK: u64 = (1u64 << 54) | (1u64 << 53) | (0x3u64 << 6);
    let page_pa: u64 = 0x0000_fedc_ba98_7000;

    for perms in 0..=ALL_PERM_FLAGS {
        let entry = make_page_entry(page_pa, perms);
        check!(is_valid_entry(entry));
        check_eq!(entry & (VALID_ENTRY_BIT | PAGE_ENTRY_BIT), VALID_ENTRY_BIT | PAGE_ENTRY_BIT);
        check_eq!(get_table_entry_paddr(entry), page_pa);
        // Access Flag (bit 10) must always be 1, and Dirty Bit Modifier (bit 51) must be 0.
        check!((entry & PAGE_ENTRY_ACCESS_FLAG_BIT) != 0);
        check_eq!(entry & DBM_BIT, 0);
        check_eq!(entry & PERM_BITS_MASK, get_page_entry_perms(perms));
        check_eq!(get_page_entry_perms(perms), PERM_LUT[perms as usize]);

        // Verify UXN/PXN and AP[2:1] semantics.
        let has_write = (perms & IOMMU_FLAG_PERM_WRITE) != 0;
        let has_read = (perms & IOMMU_FLAG_PERM_READ) != 0;
        let has_exec = (perms & IOMMU_FLAG_PERM_EXECUTE) != 0;
        let expected_xn = if has_exec { 0 } else { 1 };
        let expected_ap21 = if has_write {
            1 // Read/Write at EL0+
        } else if has_read {
            3 // Read-only at EL0+
        } else {
            2 // No access at EL0
        };
        check_eq!(get_page_entry_perms(perms), make_perms(expected_xn, expected_xn, expected_ap21));
    }

    // Exercise `write_pte` on an allocated physical page.
    let Ok(page) = SmmuEnv::alloc_page(0) else {
        return false;
    };
    let vaddr = SmmuEnv::paddr_to_physmap(page.paddr());
    // SAFETY: `vaddr` is the physmap virtual address of `page`, which is exclusively owned here.
    unsafe {
        core::ptr::write_bytes(
            core::ptr::with_exposed_provenance_mut::<u8>(vaddr),
            0,
            PAGE_SIZE_USIZE,
        );
    }
    // SAFETY: `vaddr` is the physmap virtual address of `page`, which is exclusively owned here.
    let table = unsafe { table_slice(vaddr) };
    write_pte(&table[0], table_desc, 0);
    check_eq!(table[0].load(Ordering::Acquire), table_desc);
    let leaf_desc = make_page_entry(page_pa, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);
    write_pte(&table[511], leaf_desc, 3);
    check_eq!(table[511].load(Ordering::Acquire), leaf_desc);
    write_pte(&table[0], 0, 0);
    write_pte(&table[511], 0, 3);
    // SAFETY: `vaddr` is the physmap address of `page`, which is owned by this test.
    unsafe {
        SmmuEnv::clean_dcache_range(vaddr, PAGE_SIZE_USIZE);
    }
    // SAFETY: `page` was allocated above via `SmmuEnv::alloc_page`, zeroed and cleaned to PoC, and
    // is unlinked.
    unsafe {
        SmmuEnv::free_page(page);
    }

    true
}

// ============================================================================
// 3. `SmrValue` and `SmrValueIter` Unit Tests
// ============================================================================

/// Tests `SmrValue` construction, raw value preservation, boundary Stream IDs (`0x7fff` vs
/// `0x8000`), intersection, containment, stream counting, and `SmrValueIter` iteration.
pub fn test_smr_value_and_iter() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] SmrValueAndIter\n");

    // Default invalid sentinel (`0x7fff_ffff`): extracting mask/id.
    let def = SmrValue::default_invalid();
    check_eq!(def, SmrValue::default());
    check_eq!(def.value(), 0x7fff_ffff);
    check_eq!(def.id(), 0x7fff);
    check_eq!(def.mask(), 0x7fff);

    // The raw value is preserved: bits in `id` that are masked by `mask` are kept as given (so
    // `value()` round-trips a `bti_id` or `SMR` register), and are only cleared when computing the
    // first matched Stream ID / iterating.
    let raw = SmrValue::new(0x00ff, 0x000f);
    check_eq!(raw.id(), 0x00ff);
    check_eq!(raw.mask(), 0x000f);
    check_eq!(raw.num_streams(), 16);
    check_eq!(raw.value(), (0x000fu32 << 16) | 0x00ffu32);
    check_eq!(raw.first_stream_id(), 0x00f0);
    check_eq!(
        SmrValue::from_bti_id(0x000f_00ff, SmrValue::FULL_VALID_MASK).map(|v| v.value()),
        Ok(0x000f_00ff)
    );
    // Values which differ only in masked `id` bits match the same streams.
    check!(raw.intersects(SmrValue::new(0x00f0, 0x000f)));
    check!(raw.contains(0x00f0) && raw.contains(0x00ff) && !raw.contains(0x0100));
    let mut raw_count = 0u16;
    for sid in raw {
        check_eq!(sid, 0x00f0 + raw_count);
        raw_count += 1;
    }
    check_eq!(raw_count, 16);
    // The default sentinel wildcards every bit, and so matches every 15-bit Stream ID.
    check_eq!(def.first_stream_id(), 0);
    check_eq!(def.iter().count(), 0x8000);
    check_eq!(def.iter().last(), Some(0x7fff));

    // `from_bti_id` boundary checks against `FULL_VALID_MASK` (`0x7fff_7fff`) and narrower masks.
    check!(SmrValue::from_bti_id(0x7fff, SmrValue::FULL_VALID_MASK).is_ok());
    check_eq!(SmrValue::from_bti_id(0x8000, SmrValue::FULL_VALID_MASK), Err(Status::INVALID_ARGS));
    check_eq!(
        SmrValue::from_bti_id(0x8000_0000, SmrValue::FULL_VALID_MASK),
        Err(Status::INVALID_ARGS)
    );
    check_eq!(
        SmrValue::from_bti_id(1u64 << 32, SmrValue::FULL_VALID_MASK),
        Err(Status::INVALID_ARGS)
    );
    // Narrower hardware mask (e.g. 10-bit stream IDs `0x03ff_03ff`):
    let mask_10bit: u32 = 0x03ff_03ff;
    check!(SmrValue::from_bti_id(0x03ff, mask_10bit).is_ok());
    check_eq!(SmrValue::from_bti_id(0x0400, mask_10bit), Err(Status::INVALID_ARGS));

    // `from_smr_reg` construction (the `VALID` bit is dropped, masked `id` bits are preserved).
    let mut smr_reg = gr0::Smr::from_value(0);
    smr_reg.set_valid(1).set_id(0x0123).set_mask(0x0003);
    let from_reg = SmrValue::from_smr_reg(smr_reg, SmrValue::FULL_VALID_MASK);
    check_eq!(from_reg.id(), 0x0123);
    check_eq!(from_reg.first_stream_id(), 0x0120);
    check_eq!(from_reg.mask(), 0x0003);
    check_eq!(from_reg.num_streams(), 4);

    // Intersection and containment.
    let exact_10 = SmrValue::new(0x0010, 0x0000);
    let exact_11 = SmrValue::new(0x0011, 0x0000);
    let masked_10_13 = SmrValue::new(0x0010, 0x0003); // {0x10, 0x11, 0x12, 0x13}
    let masked_10_12 = SmrValue::new(0x0010, 0x0002); // {0x10, 0x12}
    let masked_10_11 = SmrValue::new(0x0010, 0x0001); // {0x10, 0x11}
    let masked_20_23 = SmrValue::new(0x0020, 0x0003); // {0x20, 0x21, 0x22, 0x23}

    check!(!exact_10.intersects(exact_11));
    check!(!smr_intersects(exact_10.value(), exact_11.value()));
    check!(exact_10.contains(0x0010));
    check!(!exact_10.contains(0x0011));

    check!(masked_10_13.intersects(exact_11));
    check!(exact_11.intersects(masked_10_13));
    check!(smr_intersects(masked_10_13.value(), exact_11.value()));
    check!(masked_10_13.contains(0x0010));
    check!(masked_10_13.contains(0x0011));
    check!(masked_10_13.contains(0x0012));
    check!(masked_10_13.contains(0x0013));
    check!(!masked_10_13.contains(0x0014));

    // Overlapping masks with non-subset relationship (`{0x10, 0x11}` vs `{0x10, 0x12}` intersect
    // at `0x10`).
    check!(masked_10_11.intersects(masked_10_12));
    check!(!masked_10_13.intersects(masked_20_23));

    // `SmrValueIter` over an exact Stream ID (`mask == 0`).
    let mut iter_exact = exact_10.iter();
    check_eq!(iter_exact.next(), Some(0x0010));
    check_eq!(iter_exact.next(), None);

    // `SmrValueIter` over a non-contiguous mask (`id = 0x0200, mask = 0x0105`, wildcard bits 0, 2,
    // and 8 -> 8 stream IDs in strictly ascending order).
    let non_contig = SmrValue::new(0x0205, 0x0105);
    check_eq!(non_contig.id(), 0x0205);
    check_eq!(non_contig.first_stream_id(), 0x0200);
    check_eq!(non_contig.num_streams(), 8);
    let expected_sids: [u16; 8] = [0x0200, 0x0201, 0x0204, 0x0205, 0x0300, 0x0301, 0x0304, 0x0305];
    let mut count = 0usize;
    for sid in non_contig {
        check!(count < expected_sids.len());
        check_eq!(sid, expected_sids[count]);
        check!(non_contig.contains(sid));
        count += 1;
    }
    check_eq!(count, expected_sids.len());

    // `SmrValueIter` at the upper 15-bit boundary (`0x7ffe..=0x7fff`) must terminate cleanly
    // without wrapping around.
    let top_pair = SmrValue::new(0x7ffe, 0x0001);
    let mut top_iter = top_pair.iter();
    check_eq!(top_iter.next(), Some(0x7ffe));
    check_eq!(top_iter.next(), Some(0x7fff));
    check_eq!(top_iter.next(), None);

    let max_single = SmrValue::new(0x7fff, 0x0000);
    let mut max_iter = max_single.iter();
    check_eq!(max_iter.next(), Some(0x7fff));
    check_eq!(max_iter.next(), None);

    true
}

// ============================================================================
// 4. `Bitmask128` Unit Tests
// ============================================================================

/// Tests `Bitmask128` bit manipulation, boundary indices (`0`, `63`, `64`, `127`),
/// `set_lowest_n_bits`, `find_first_set_bit`, `find_first_clr_bit`, and `Bitmask128Iter`.
pub fn test_bitmask128() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] Bitmask128\n");

    let mut mask = Bitmask128::new();
    check_eq!(mask, Bitmask128::default());
    check_eq!(mask.raw(), 0);
    check_eq!(mask.num_set_bits(), 0);
    check_eq!(mask.find_first_set_bit(), None);
    check_eq!(mask.find_first_clr_bit(), Some(0));
    check_eq!(mask.iter_set_bits().next(), None);

    // Boundary bits: 0, 63, 64, 127.
    let boundary_bits: [u32; 4] = [0, 63, 64, 127];
    for &b in &boundary_bits {
        mask.set_bit(b);
        check!(mask.test_bit(b));
    }
    check_eq!(mask.num_set_bits(), 4);
    check_eq!(mask.find_first_set_bit(), Some(0));
    check_eq!(mask.find_first_clr_bit(), Some(1));

    // Verify `Bitmask128Iter` yields set bits in ascending order across low and high 64-bit halves.
    let mut iter_idx = 0usize;
    for b in &mask {
        check!(iter_idx < boundary_bits.len());
        check_eq!(b, boundary_bits[iter_idx]);
        iter_idx += 1;
    }
    check_eq!(iter_idx, boundary_bits.len());

    // `clear_bit` and `assign_bit`.
    mask.clear_bit(0);
    check!(!mask.test_bit(0));
    check_eq!(mask.find_first_set_bit(), Some(63));
    mask.clear_bit(63);
    check_eq!(mask.find_first_set_bit(), Some(64));
    mask.assign_bit(64, false);
    check_eq!(mask.find_first_set_bit(), Some(127));
    mask.assign_bit(10, true);
    check_eq!(mask.find_first_set_bit(), Some(10));

    // `clear_all` and `set_lowest_n_bits` across word boundaries (`0`, `1`, `63`, `64`, `65`,
    // `127`, `128`).
    for &n in &[0u32, 1, 63, 64, 65, 127, 128] {
        mask.clear_all();
        mask.set_lowest_n_bits(n);
        check_eq!(mask.num_set_bits(), n);
        if n == 0 {
            check_eq!(mask.find_first_set_bit(), None);
            check_eq!(mask.find_first_clr_bit(), Some(0));
        } else if n == 128 {
            check_eq!(mask.raw(), u128::MAX);
            check_eq!(mask.find_first_set_bit(), Some(0));
            check_eq!(mask.find_first_clr_bit(), None);
        } else {
            check_eq!(mask.find_first_set_bit(), Some(0));
            check_eq!(mask.find_first_clr_bit(), Some(n));
            check!(mask.test_bit(n - 1));
            check!(!mask.test_bit(n));
        }
    }

    // `set_all` and `from_raw`.
    mask.clear_all();
    mask.set_all();
    check_eq!(mask, Bitmask128::from_raw(u128::MAX));
    check_eq!(mask.num_set_bits(), 128);
    check_eq!(mask.find_first_clr_bit(), None);
    mask.clear_bit(99);
    check_eq!(mask.find_first_clr_bit(), Some(99));

    true
}

// ============================================================================
// 5. `PageCache` Unit Tests
// ============================================================================

/// Helper returning a mutable byte slice over a 4 KiB page in the kernel/mock physmap.
///
/// # Safety
///
/// `paddr` must be the physical address of an allocated 4 KiB page exclusively accessed by the
/// caller for lifetime `'a`.
unsafe fn page_bytes_mut<'a>(paddr: u64) -> &'a mut [u8; PAGE_SIZE_USIZE] {
    let vaddr = SmmuEnv::paddr_to_physmap(paddr);
    // SAFETY: Caller guarantees `paddr` is an allocated page with exclusive mutable access.
    unsafe { &mut *core::ptr::with_exposed_provenance_mut::<[u8; PAGE_SIZE_USIZE]>(vaddr) }
}

/// Recursively frees all valid translation table pages reachable from `table_page` back into
/// `cache` during early-return test cleanup.
fn free_test_table_tree(table_page: SmmuPage, level: usize, cache: &mut PageCache) {
    let vaddr = SmmuEnv::paddr_to_physmap(table_page.paddr());
    // SAFETY: `table_page` is a valid translation table page allocated by the test.
    let entries = unsafe { table_slice(vaddr) };
    for entry_ref in entries.iter() {
        let entry = entry_ref.load(Ordering::Relaxed);
        if entry != 0 {
            if level + 1 < LEVELS
                && is_valid_entry(entry)
                && is_table_entry(entry)
                && let Some(child) = SmmuEnv::paddr_to_page(get_table_entry_paddr(entry))
            {
                free_test_table_tree(child, level + 1, cache);
            }
            entry_ref.store(0, Ordering::Relaxed);
        }
    }
    // SAFETY: `table_page` was checked out from `cache`, is unlinked, and all entries were zeroed
    // above.
    unsafe {
        cache.return_page(table_page);
    }
}

/// Test wrapper around [`PageCache`] that returns any in-flight pages and trims the cache to 0 on
/// early test failure return before [`PageCache::drop`] runs its teardown `debug_assert!`s.
struct TestPageCache {
    inner: PageCache,
    in_flight: [Option<SmmuPage>; 8],
    root_page: Option<SmmuPage>,
}

impl TestPageCache {
    const fn new() -> Self {
        Self { inner: PageCache::new(), in_flight: [None; 8], root_page: None }
    }

    fn get_page(&mut self) -> Result<SmmuPage, Status> {
        let page = self.inner.get_page()?;
        if let Some(slot) = self.in_flight.iter_mut().find(|s| s.is_none()) {
            *slot = Some(page);
        }
        Ok(page)
    }

    fn get_root_page(&mut self) -> Result<SmmuPage, Status> {
        let page = self.inner.get_page()?;
        self.root_page = Some(page);
        Ok(page)
    }

    fn return_page(&mut self, page: SmmuPage) {
        if self.root_page == Some(page) {
            self.root_page = None;
        }
        if let Some(slot) = self.in_flight.iter_mut().find(|s| **s == Some(page)) {
            *slot = None;
        }
        // SAFETY: Test callers only return unlinked pages checked out from `self.inner`.
        unsafe {
            self.inner.return_page(page);
        }
    }
}

impl core::ops::Deref for TestPageCache {
    type Target = PageCache;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}

impl core::ops::DerefMut for TestPageCache {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.inner
    }
}

impl Drop for TestPageCache {
    fn drop(&mut self) {
        if self.inner.in_flight_pages() != 0 || self.inner.cache_entries() != 0 {
            if let Some(root) = self.root_page.take() {
                free_test_table_tree(root, 0, &mut self.inner);
            }
            for slot in &mut self.in_flight {
                if let Some(page) = slot.take() {
                    // SAFETY: `page` was checked out from `self.inner` and is unlinked.
                    unsafe {
                        self.inner.return_page(page);
                    }
                }
            }
            self.inner.trim(0);
        }
    }
}

#[cfg(not(is_kernel))]
struct TestPmmLimitGuard;

#[cfg(not(is_kernel))]
impl Drop for TestPmmLimitGuard {
    fn drop(&mut self) {
        SmmuEnv::host_pmm_clear_allocation_limit();
    }
}

/// Tests `PageCache` allocation (`get_page`), zero-filling, `return_page`, and LIFO reuse
/// ordering.
pub fn test_page_cache_add_and_get() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] PageCacheAddAndGet\n");

    #[cfg(not(is_kernel))]
    let baseline_pmm = SmmuEnv::host_pmm_allocated_page_count();

    let mut cache = TestPageCache::new();
    check_eq!(cache.in_flight_pages(), 0);
    check_eq!(cache.cache_entries(), 0);

    let mut pages = [None; 4];
    for (i, slot) in pages.iter_mut().enumerate() {
        let Ok(page) = cache.get_page() else {
            return false;
        };
        check_eq!(cache.in_flight_pages(), (i + 1) as u32);
        check_eq!(cache.cache_entries(), 0);
        #[cfg(not(is_kernel))]
        check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm + i + 1);

        // Newly allocated pages from `PageCache::get_page` must be completely zero-filled and
        // cleaned to PoC DRAM.
        // SAFETY: `page` was just checked out from `cache` and is exclusively owned here.
        let bytes = unsafe { page_bytes_mut(page.paddr()) };
        check!(bytes.iter().all(|&b| b == 0));
        #[cfg(not(is_kernel))]
        check_eq!(SmmuEnv::host_pmm_read_poc_dram64(page.paddr()), Some(0));

        *slot = Some(page);
    }

    // Return all 4 pages to the cache.
    for (i, slot) in pages.iter().enumerate() {
        cache.return_page(slot.unwrap());
        check_eq!(cache.in_flight_pages(), (3 - i) as u32);
        check_eq!(cache.cache_entries(), (i + 1) as u32);
        #[cfg(not(is_kernel))]
        check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm + 4);
    }

    // Pop all 4 pages again: must come from the cache in LIFO order (`pages[3..=0]`, stack
    // behavior, not queue behavior) and be zeroed.
    for i in 0..4 {
        let Ok(page) = cache.get_page() else {
            return false;
        };
        check_eq!(page, pages[3 - i].unwrap());
        check_eq!(cache.in_flight_pages(), (i + 1) as u32);
        check_eq!(cache.cache_entries(), (3 - i) as u32);
        #[cfg(not(is_kernel))]
        check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm + 4);

        // SAFETY: `page` was just checked out from `cache` and is exclusively owned here.
        let bytes = unsafe { page_bytes_mut(page.paddr()) };
        check!(bytes.iter().all(|&b| b == 0));
    }

    // Return and trim to 0 before dropping `cache`.
    for slot in pages.iter().rev() {
        cache.return_page(slot.unwrap());
    }
    cache.trim(0);
    check_eq!(cache.in_flight_pages(), 0);
    check_eq!(cache.cache_entries(), 0);
    #[cfg(not(is_kernel))]
    check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm);

    true
}

/// Tests `PageCache::trim` FIFO tail eviction and LIFO head retention.
pub fn test_page_cache_trim() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] PageCacheTrim\n");

    #[cfg(not(is_kernel))]
    let baseline_pmm = SmmuEnv::host_pmm_allocated_page_count();

    let mut cache = TestPageCache::new();
    let mut pages = [None; 4];
    for slot in &mut pages {
        let Ok(p) = cache.get_page() else {
            return false;
        };
        *slot = Some(p);
    }

    // Return `pages[0..4]` in order so `pages[3]` is at the MRU head and `pages[0]` is at the LRU
    // tail.
    for slot in &pages {
        cache.return_page(slot.unwrap());
    }
    check_eq!(cache.cache_entries(), 4);

    // Trimming to `>= 4` is a no-op.
    cache.trim(5);
    check_eq!(cache.cache_entries(), 4);
    cache.trim(4);
    check_eq!(cache.cache_entries(), 4);

    // Trimming to 3 must evict the oldest tail page (`pages[0]`) and retain `pages[3], pages[2],
    // pages[1]` from head to tail.
    cache.trim(3);
    check_eq!(cache.cache_entries(), 3);
    #[cfg(not(is_kernel))]
    check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm + 3);

    let Ok(pop3) = cache.get_page() else {
        return false;
    };
    let Ok(pop2) = cache.get_page() else {
        return false;
    };
    let Ok(pop1) = cache.get_page() else {
        return false;
    };
    check_eq!(pop3, pages[3].unwrap());
    check_eq!(pop2, pages[2].unwrap());
    check_eq!(pop1, pages[1].unwrap());
    check_eq!(cache.cache_entries(), 0);

    // Return `pop1`, `pop2`, `pop3` so `pop3` is head and `pop1` is tail, then step `trim` down
    // to 0.
    cache.return_page(pop1);
    cache.return_page(pop2);
    cache.return_page(pop3);
    check_eq!(cache.cache_entries(), 3);

    cache.trim(2);
    check_eq!(cache.cache_entries(), 2);
    #[cfg(not(is_kernel))]
    check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm + 2);

    cache.trim(1);
    check_eq!(cache.cache_entries(), 1);
    #[cfg(not(is_kernel))]
    check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm + 1);

    // The single remaining cached page must be the MRU head (`pop3`).
    let Ok(last_pop) = cache.get_page() else {
        return false;
    };
    check_eq!(last_pop, pop3);
    cache.return_page(last_pop);

    cache.trim(0);
    check_eq!(cache.cache_entries(), 0);
    check_eq!(cache.in_flight_pages(), 0);
    #[cfg(not(is_kernel))]
    check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm);

    true
}

/// Tests `PageCache` OOM handling when the underlying PMM reaches its allocation limit.
pub fn test_page_cache_oom() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] PageCacheOom\n");

    #[cfg(not(is_kernel))]
    {
        #[cfg(not(is_kernel))]
        let baseline_pmm = SmmuEnv::host_pmm_allocated_page_count();
        SmmuEnv::host_pmm_set_allocation_limit(1);
        let _limit_guard = TestPmmLimitGuard;

        let mut cache = TestPageCache::new();
        let Ok(p1) = cache.get_page() else {
            return false;
        };
        check_eq!(cache.in_flight_pages(), 1);

        // Second allocation from PMM must fail with `Status::NO_MEMORY`.
        check_eq!(cache.get_page(), Err(Status::NO_MEMORY));
        check_eq!(cache.in_flight_pages(), 1);

        // Returning `p1` to the cache allows `get_page()` to succeed from the cache without
        // allocating a new page from the PMM.
        cache.return_page(p1);
        check_eq!(cache.cache_entries(), 1);
        let Ok(reused) = cache.get_page() else {
            return false;
        };
        check_eq!(reused, p1);

        cache.return_page(reused);
        cache.trim(0);
        drop(_limit_guard);
        #[cfg(not(is_kernel))]
        check_eq!(SmmuEnv::host_pmm_allocated_page_count(), baseline_pmm);
    }

    true
}

// ============================================================================
// 6. `TranslationTableHelper` Unit Tests
// ============================================================================

/// Tests `TranslationTableHelper` 4-level cursor walks across L1/L2/L3 boundaries, PoC dcache
/// flushing on `finish_operation`, and partial vs. full intermediate table reclamation on unmap.
pub fn test_translation_table_helper_walk_and_reclaim() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] TranslationTableHelperWalkAndReclaim\n");

    let aspace_start: u64 = 0x1000;
    let aspace_len: u64 = (1u64 << 39) - aspace_start;
    // `RegionAllocator` requires pinned in-place initialization (`PinInit`), and `kalloc::Box` does
    // not provide a standalone `Box::pin_init` constructor, so we allocate uninitialized heap
    // storage via `try_new_uninit` and run `__pinned_init` directly in place.
    let mut avail_regions_uninit = match kalloc::Box::<RegionAllocator>::try_new_uninit() {
        Ok(b) => b,
        Err(_) => return false,
    };
    let init = RegionAllocator::init();
    // SAFETY: `avail_regions_uninit` is a freshly allocated heap box whose address remains stable
    // inside `Pin<kalloc::Box<RegionAllocator>>` for the lifetime of this test.
    let avail_regions = unsafe {
        let Ok(()) = init.__pinned_init(avail_regions_uninit.as_mut_ptr());
        Pin::new_unchecked(avail_regions_uninit.assume_init())
    };
    check!(
        avail_regions
            .add_region(RegionSpan { base: aspace_start, size: aspace_len }, AllowOverlap::No)
            .is_ok()
    );

    let mut page_cache = TestPageCache::new();
    let Ok(root_page) = page_cache.get_root_page() else {
        return false;
    };
    check_eq!(page_cache.in_flight_pages(), 1);

    // Part A: Map 2 consecutive pages straddling the 1 GiB (L1[0] -> L1[1]) boundary:
    // Page 0 at `(1 << 30) - PAGE_SIZE` (`L0[0], L1[0], L2[511], L3[511]`)
    // Page 1 at `1 << 30` (`L0[0], L1[1], L2[0], L3[0]`)
    let straddle_va = (1u64 << 30) - PAGE_SIZE;
    let Ok(straddle_reg) =
        avail_regions.get_region_specific(RegionSpan { base: straddle_va, size: 2 * PAGE_SIZE })
    else {
        return false;
    };

    let mut helper = TranslationTableHelper::new();
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_map(straddle_va, root_page, &mut page_cache).is_ok());
        // `initialize_for_map` allocates L1[0], L2[511], and L3 (3 intermediate pages + 1 root = 4).
        check_eq!(page_cache.in_flight_pages(), 4);
        check!(!helper.current_page_entry_valid());

        let pa0: u64 = 0x0000_2000_0000;
        let pa1: u64 = 0x0000_2000_1000;
        let pte0 = make_page_entry(pa0, IOMMU_FLAG_PERM_READ);
        let pte1 = make_page_entry(pa1, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);

        helper.assign_page_entry(pte0);
        check!(helper.current_page_entry_valid());

        // Advance across the 1 GiB boundary: finishes L3 and L2 under L1[0] and allocates new L2[0]
        // and L3[0] under L1[1] (+2 pages -> 6 total in flight).
        check!(helper.advance(aspace_start, aspace_len, &avail_regions, &mut page_cache).is_ok());
        check_eq!(page_cache.in_flight_pages(), 6);
        check!(!helper.current_page_entry_valid());

        helper.assign_page_entry(pte1);
        check!(helper.current_page_entry_valid());
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }

    // Verify L0[0] was cleaned to PoC DRAM on host.
    #[cfg(not(is_kernel))]
    {
        let l0_entry = SmmuEnv::host_pmm_read_poc_dram64(root_page.paddr());
        check!(l0_entry.is_some());
        check!(is_table_entry(l0_entry.unwrap()));
    }

    // Unmap the 2 straddling pages after dropping `straddle_reg` (returning the VA range to
    // `avail_regions`), and verify all 5 intermediate tables (L1, two L2s, two L3s) are reclaimed!
    drop(straddle_reg);
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_unmap(straddle_va, root_page, &mut page_cache).is_ok());
        check!(helper.current_page_entry_valid());
        helper.assign_page_entry(0);
        check!(helper.advance(aspace_start, aspace_len, &avail_regions, &mut page_cache).is_ok());
        check!(helper.current_page_entry_valid());
        helper.assign_page_entry(0);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }

    // Only the root L0 page should remain in flight; the 5 intermediate pages are back in cache.
    check_eq!(page_cache.in_flight_pages(), 1);
    check_eq!(page_cache.cache_entries(), 5);

    // Part B: Map two sibling pages in the same L3 table (`0x1000` and `0x2000`), then unmap one
    // at a time to verify partial unmap retains intermediate tables while final unmap reclaims
    // them.
    let Ok(reg_a) = avail_regions.get_region_specific(RegionSpan { base: 0x1000, size: PAGE_SIZE })
    else {
        page_cache.return_page(root_page);
        page_cache.trim(0);
        return false;
    };
    let Ok(reg_b) = avail_regions.get_region_specific(RegionSpan { base: 0x2000, size: PAGE_SIZE })
    else {
        page_cache.return_page(root_page);
        page_cache.trim(0);
        return false;
    };

    let pa0: u64 = 0x0000_2000_0000;
    let pa1: u64 = 0x0000_2000_1000;
    let pte0 = make_page_entry(pa0, IOMMU_FLAG_PERM_READ);
    let pte1 = make_page_entry(pa1, IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE);

    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_map(0x1000, root_page, &mut page_cache).is_ok());
        helper.assign_page_entry(pte0);
        check!(helper.advance(aspace_start, aspace_len, &avail_regions, &mut page_cache).is_ok());
        helper.assign_page_entry(pte1);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }
    // Root L0 + L1 + L2 + L3 = 4 pages in flight, 2 pages remaining in cache.
    check_eq!(page_cache.in_flight_pages(), 4);
    check_eq!(page_cache.cache_entries(), 2);

    // Unmap `0x1000` while `0x2000` (`reg_b`) is still allocated: L1/L2/L3 must NOT be reclaimed.
    drop(reg_a);
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_unmap(0x1000, root_page, &mut page_cache).is_ok());
        helper.assign_page_entry(0);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }
    check_eq!(page_cache.in_flight_pages(), 4);
    check_eq!(page_cache.cache_entries(), 2);

    // Now drop `reg_b` and unmap `0x2000`: L3, L2, and L1 are now completely unused and must all
    // be reclaimed into `page_cache`.
    drop(reg_b);
    // SAFETY: `root_page` and all intermediate table pages are valid, live 4 KiB pages checked out
    // from `page_cache`.
    unsafe {
        check!(helper.initialize_for_unmap(0x2000, root_page, &mut page_cache).is_ok());
        helper.assign_page_entry(0);
        helper.finish_operation(aspace_start, aspace_len, &avail_regions, &mut page_cache);
    }
    check_eq!(page_cache.in_flight_pages(), 1);
    check_eq!(page_cache.cache_entries(), 5);

    page_cache.return_page(root_page);
    page_cache.trim(0);
    check_eq!(page_cache.in_flight_pages(), 0);
    check_eq!(page_cache.cache_entries(), 0);

    true
}

// ============================================================================
// 7. `DeviceAspace` Unit Tests
// ============================================================================

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ExpectedRegion {
    vaddr: u64,
    paddr: u64,
    len: u64,
    perms: u32,
}

/// Mock `PinnedVmoSource` backed by a slice of physical `(paddr, len)` segments.
struct MockPinnedVmo<'a> {
    segments: &'a [(u64, u64)],
    offset: u64,
    size: u64,
}

impl<'a> MockPinnedVmo<'a> {
    fn new(segments: &'a [(u64, u64)]) -> Self {
        let mut total = 0u64;
        for &(_pa, len) in segments {
            total += len;
        }
        Self { segments, offset: 0, size: total }
    }

    fn new_with_size(segments: &'a [(u64, u64)], size: u64) -> Self {
        Self { segments, offset: 0, size }
    }
}

impl PinnedVmoSource for MockPinnedVmo<'_> {
    fn offset(&self) -> u64 {
        self.offset
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        if offset < self.offset || size == 0 || (offset - self.offset) + size > self.size {
            return Err(Status::INVALID_ARGS);
        }
        let mut cur_off = self.offset;
        for &(seg_pa, seg_len) in self.segments {
            if offset >= cur_off && offset < cur_off + seg_len {
                let rel = offset - cur_off;
                if rel + size > seg_len {
                    return Err(Status::NOT_FOUND);
                }
                return Ok(seg_pa + rel);
            }
            cur_off += seg_len;
        }
        Err(Status::NOT_FOUND)
    }
}

#[derive(Default)]
struct RecordingInvalidator {
    count: usize,
    last_base: u64,
    last_size: u64,
    #[cfg(not(is_kernel))]
    pmm_pages_during_inval: usize,
}

impl TlbInvalidator for RecordingInvalidator {
    fn invalidate(&mut self, base: u64, size: u64) {
        self.count += 1;
        self.last_base = base;
        self.last_size = size;
        #[cfg(not(is_kernel))]
        {
            self.pmm_pages_during_inval = SmmuEnv::host_pmm_allocated_page_count();
        }
    }
}

/// Test guard that ensures `DeviceAspace::free_translation_tables` runs even if a `check!` returns
/// early.
struct TestAspace(kalloc::Box<DeviceAspace>);

impl core::ops::Deref for TestAspace {
    type Target = DeviceAspace;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for TestAspace {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestAspace {
    fn drop(&mut self) {
        #[cfg(not(is_kernel))]
        SmmuEnv::host_pmm_clear_allocation_limit();
        if self.0.page_cache().in_flight_pages() != 0 || self.0.page_cache().cache_entries() != 0 {
            let mut inval = RecordingInvalidator::default();
            self.0.free_translation_tables(&mut inval);
        }
    }
}

struct WalkVerificationState<'a> {
    expected: &'a [ExpectedRegion],
    region_idx: usize,
    region_offset: u64,
    table_pages: u32,
    aspace_start: u64,
    aspace_end: u64,
    avail_regions: &'a RegionAllocator,
}

/// Recursively walks levels `0..=3` of a VMSAv8-64 translation table rooted at `table_pa` and
/// verifies that the translation table entries accurately reflect the expected allocated and
/// available regions.
///
/// The invariants verified by this function are:
/// 1. Any valid leaf entry (`level == 3`) must be fully contained within the set of regions marked
///    as `Allocated` in `avail_regions`.
/// 2. Any valid leaf entry must map to the expected physical address and have the correct access
///    permissions (`make_page_entry(reg.paddr + state.region_offset, reg.perms)`).
/// 3. Any invalid entry (or portion of an invalid entry) that falls within the bounds of the
///    active address space (`[aspace_start, aspace_end)`) must be fully contained within the set of
///    regions marked as `Available` (free/unmapped space).
/// 4. There cannot be any Block entries (only Table entries at `level < 3` or Page entries at
///    `level == 3`).
/// 5. Invalid entries must be strictly zero (`entry == 0`), not just missing the valid bit.
/// 6. Intermediate tables (`level > 0`) are never empty (all 512 entries invalid), all entries
///    match PoC DRAM and the CPU cache on host, and the total count of visited translation table
///    pages (`state.table_pages`) equals `PageCache::in_flight_pages()`.
fn verify_table_level(
    table_pa: u64,
    level: usize,
    base_vaddr: u64,
    state: &mut WalkVerificationState<'_>,
) -> bool {
    state.table_pages += 1;
    let table_vaddr = SmmuEnv::paddr_to_physmap(table_pa);
    check!(table_vaddr != 0);
    // SAFETY: `table_vaddr` is the physmap virtual address of a valid 4 KiB translation table page.
    let table = unsafe { table_slice(table_vaddr) };

    let shift = PAGE_SHIFT + (9 * ((LEVELS - level - 1) as u32));
    let entry_span = 1u64 << shift;
    let mut non_empty = false;

    for (i, slot) in table.iter().enumerate() {
        let entry = slot.load(Ordering::Relaxed);
        let entry_vaddr = base_vaddr + (i as u64) * entry_span;
        let entry_end = entry_vaddr + entry_span;

        #[cfg(not(is_kernel))]
        {
            let entry_pa = table_pa + (i as u64) * 8;
            let poc_val = SmmuEnv::host_pmm_read_poc_dram64(entry_pa);
            let cpu_val = SmmuEnv::host_pmm_read_cpu_cache64(entry_pa);
            check_eq!(poc_val, Some(entry));
            check_eq!(cpu_val, Some(entry));
        }

        if !is_valid_entry(entry) {
            check_eq!(entry, 0);
            let check_start = entry_vaddr.max(state.aspace_start);
            let check_end = entry_end.min(state.aspace_end);
            if check_start < check_end {
                let contained = state.avail_regions.test_region_contained_by(
                    RegionSpan { base: check_start, size: check_end - check_start },
                    TestRegionSet::Available,
                );
                check_eq!(contained, Ok(true));
            }
            // If we were partway through matching an expected region, a gap inside that region is
            // an error.
            check_eq!(state.region_offset, 0);
            continue;
        }

        non_empty = true;
        if level < LEVELS - 1 {
            check!(is_table_entry(entry));
            check!(!is_block_entry(entry, level as u32));
            let next_pa = get_table_entry_paddr(entry);
            check!(is_valid_addr(next_pa));
            check!(verify_table_level(next_pa, level + 1, entry_vaddr, state));
        } else {
            // Level 3 leaf page descriptor.
            check!(!is_block_entry(entry, 3));
            let contained = state.avail_regions.test_region_contained_by(
                RegionSpan { base: entry_vaddr, size: entry_span },
                TestRegionSet::Allocated,
            );
            check_eq!(contained, Ok(true));

            // Walk `state.expected` (sorted in ascending virtual address order) in lockstep with
            // the depth-first page table traversal, verifying that each mapped leaf page matches
            // the expected region's virtual address, physical address, and permissions.
            check!(state.region_idx < state.expected.len());
            let reg = &state.expected[state.region_idx];
            check_eq!(entry_vaddr, reg.vaddr + state.region_offset);
            let expected_pte = make_page_entry(reg.paddr + state.region_offset, reg.perms);
            check_eq!(entry, expected_pte);

            state.region_offset += PAGE_SIZE;
            if state.region_offset == reg.len {
                state.region_idx += 1;
                state.region_offset = 0;
            }
        }
    }

    // No intermediate table (`level > 0`) is permitted to remain allocated if all 512 of its
    // entries are invalid (`0`).
    if level > 0 {
        check!(non_empty);
    }
    true
}

/// Helper to kick off the recursive verification of the page table against the region allocator's
/// state and `PageCache` in-flight page count. This checks that every mapped and unmapped region
/// is correctly accounted for in the 4-level page table structure.
fn verify_allocated_regions(aspace: &DeviceAspace, expected: &[ExpectedRegion]) -> bool {
    for reg in expected {
        let contained = aspace.avail_regions().test_region_contained_by(
            RegionSpan { base: reg.vaddr, size: reg.len },
            TestRegionSet::Allocated,
        );
        check_eq!(contained, Ok(true));
    }

    let root_pa = aspace.root_tt_paddr();
    check!(root_pa != 0);
    let aspace_start = aspace.first_valid_address();
    let aspace_end = aspace.last_valid_address() + 1;
    let mut state = WalkVerificationState {
        expected,
        region_idx: 0,
        region_offset: 0,
        table_pages: 0,
        aspace_start,
        aspace_end,
        avail_regions: aspace.avail_regions(),
    };
    check!(verify_table_level(root_pa, 0, 0, &mut state));
    check_eq!(state.region_idx, expected.len());
    check_eq!(state.region_offset, 0);
    check_eq!(state.table_pages, aspace.page_cache().in_flight_pages());
    true
}

/// Maps and unmaps a 3-segment scattered 6-page VMO (with both single-page and multi-page
/// contiguous segment runs) across all `0..=7` permission flag combinations, verifying the
/// 4-level page table tree at each step.
pub fn test_device_aspace_map_and_unmap() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapAndUnmap\n");

    // 48-bit address space, starting at 1 MiB.
    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    // Don't cache any pages (`max_cache_pages = 0`). This makes it easier to assert that we have
    // released the proper number of pages as we go through the tests.
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    // We should have 1 page allocated at this point (for the root translation table page);
    // `verify_allocated_regions` checks this.
    check!(verify_allocated_regions(&aspace, &[]));

    // Create mock VMO with virtual offset to physical address segment mappings.
    let segments: [(u64, u64); 3] =
        [(0x0010_0000, 2 * PAGE_SIZE), (0x0020_0000, PAGE_SIZE), (0x0030_0000, 3 * PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segments);

    // Now perform the mapping operation, then verify that the page tables match what we would
    // expect based on our region allocator and expected mappings state. Test all combinations of
    // the IOMMU permission flags.
    for perms in 0..=ALL_PERM_FLAGS {
        let mut inval = RecordingInvalidator::default();
        let Ok(alloc) = aspace.map(&vmo, perms, &mut inval, None) else {
            return false;
        };
        check_eq!(alloc.base(), aspace_start);
        check_eq!(alloc.size(), 6 * PAGE_SIZE);
        check_eq!(inval.count, 1);
        check_eq!(inval.last_base, alloc.base());
        check_eq!(inval.last_size, alloc.size());

        let expected = [
            ExpectedRegion { vaddr: alloc.base(), paddr: segments[0].0, len: segments[0].1, perms },
            ExpectedRegion {
                vaddr: alloc.base() + 2 * PAGE_SIZE,
                paddr: segments[1].0,
                len: segments[1].1,
                perms,
            },
            ExpectedRegion {
                vaddr: alloc.base() + 3 * PAGE_SIZE,
                paddr: segments[2].0,
                len: segments[2].1,
                perms,
            },
        ];
        check!(verify_allocated_regions(&aspace, &expected));

        // Unmap and revalidate the page tables once again.
        #[cfg(not(is_kernel))]
        let pages_before_unmap = SmmuEnv::host_pmm_allocated_page_count();
        aspace.unmap(alloc, &mut inval);
        check_eq!(inval.count, 2);
        check_eq!(inval.last_base, aspace_start);
        check_eq!(inval.last_size, 6 * PAGE_SIZE);
        #[cfg(not(is_kernel))]
        {
            // Reclaimed intermediate table pages must remain in `PageCache` during
            // `TlbInvalidator::invalidate` and be freed to the PMM only after invalidation returns.
            check_eq!(inval.pmm_pages_during_inval, pages_before_unmap);
            check_eq!(SmmuEnv::host_pmm_allocated_page_count(), pages_before_unmap - 3);
        }
        check!(verify_allocated_regions(&aspace, &[]));
        check_eq!(aspace.page_cache().in_flight_pages(), 1);
        check_eq!(aspace.page_cache().cache_entries(), 0);
    }

    #[cfg(not(is_kernel))]
    let pages_before_free = SmmuEnv::host_pmm_allocated_page_count();
    let mut inval = RecordingInvalidator::default();
    aspace.free_translation_tables(&mut inval);
    check_eq!(inval.count, 1);
    check_eq!(inval.last_base, 0);
    check_eq!(inval.last_size, 0);
    #[cfg(not(is_kernel))]
    {
        check_eq!(inval.pmm_pages_during_inval, pages_before_free);
        check_eq!(SmmuEnv::host_pmm_allocated_page_count(), pages_before_free - 1);
    }
    // The root table page has been returned and freed along with everything else. (Asking for
    // `root_tt_paddr()` at this point is a debug-asserted error.)
    check_eq!(aspace.page_cache().in_flight_pages(), 0);
    check_eq!(aspace.page_cache().cache_entries(), 0);

    true
}

/// Tests `DeviceAspace::map` at a caller-specified virtual address.
pub fn test_device_aspace_map_at_specific_location() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapAtSpecificLocation\n");

    // 48-bit address space, starting at 1 MiB.
    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    // Create mock VMO with virtual offset to physical address segment mappings.
    let segments = [(0x0040_0000, 4 * PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segments);
    // Pick a page-aligned target location in the address space.
    let target_vaddr: u64 = 0x1234_5000;
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;
    let mut inval = RecordingInvalidator::default();

    let Ok(alloc) = aspace.map(&vmo, perms, &mut inval, Some(target_vaddr)) else {
        return false;
    };
    check_eq!(alloc.base(), target_vaddr);
    check_eq!(alloc.size(), 4 * PAGE_SIZE);

    let expected =
        [ExpectedRegion { vaddr: target_vaddr, paddr: segments[0].0, len: segments[0].1, perms }];
    check!(verify_allocated_regions(&aspace, &expected));

    // Unmap and revalidate the page tables once again.
    aspace.unmap(alloc, &mut inval);
    check!(verify_allocated_regions(&aspace, &[]));
    aspace.free_translation_tables(&mut inval);
    true
}

/// Creates a 2-page mapping which straddles all of the translation table levels (`(1 << 39) -
/// PAGE_SIZE`). The first page requires an entry in the last slot of every level of the
/// translation tables, while the second page requires an entry in the first slot of every level of
/// the translation table.
pub fn test_device_aspace_map_straddling_table_levels() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapStraddlingTableLevels\n");

    // 48-bit address space, starting at 1 MiB.
    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    let segments = [(0x0050_0000, PAGE_SIZE), (0x0060_0000, PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segments);
    // Pick a location which straddles all of the translation table levels. The L0/L1 boundary is
    // at 1 << 39.
    let target_vaddr: u64 = (1u64 << 39) - PAGE_SIZE;
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_EXECUTE;
    let mut inval = RecordingInvalidator::default();

    let Ok(alloc) = aspace.map(&vmo, perms, &mut inval, Some(target_vaddr)) else {
        return false;
    };
    check_eq!(alloc.base(), target_vaddr);
    check_eq!(alloc.size(), 2 * PAGE_SIZE);
    // Root L0 + two full L1/L2/L3 chains = 1 + 6 = 7 translation table pages in flight.
    check_eq!(aspace.page_cache().in_flight_pages(), 7);

    let expected = [
        ExpectedRegion { vaddr: target_vaddr, paddr: segments[0].0, len: PAGE_SIZE, perms },
        ExpectedRegion {
            vaddr: target_vaddr + PAGE_SIZE,
            paddr: segments[1].0,
            len: PAGE_SIZE,
            perms,
        },
    ];
    check!(verify_allocated_regions(&aspace, &expected));

    // Unmap and revalidate the page tables once again.
    aspace.unmap(alloc, &mut inval);
    check!(verify_allocated_regions(&aspace, &[]));
    check_eq!(aspace.page_cache().in_flight_pages(), 1);
    aspace.free_translation_tables(&mut inval);
    true
}

/// Tests `DeviceAspace::map` failure injection and rollback.
///
/// Part 1 maps 2 pages straddling the 512 GiB `L0[0] -> L0[1]` boundary (`(1 << 39) - PAGE_SIZE`),
/// where the first page resides in the last slot of `L0[0]`'s L1/L2/L3 table chain and the second
/// page resides in the first slot of `L0[1]`'s L1/L2/L3 table chain. The full mapping requires 6
/// new intermediate table allocations:
/// 1. `L0[0]`'s L1 table (during `initialize_for_map` for page 0)
/// 2. `L0[0]`'s L2 table (during `initialize_for_map` for page 0)
/// 3. `L0[0]`'s L3 table (during `initialize_for_map` for page 0)
/// 4. `L0[1]`'s L1 table (during `advance` from page 0 to page 1)
/// 5. `L0[1]`'s L2 table (during `advance` from page 0 to page 1)
/// 6. `L0[1]`'s L3 table (during `advance` from page 0 to page 1)
///
/// By setting the allocation limit to `limit` in `0..=5`, we inject an allocation failure at each
/// of those 6 table allocations in turn and verify that `map` fails with `ZX_ERR_NO_MEMORY`,
/// invalidates the TLB, and rolls back all partially-allocated tables and regions.
///
/// Part 2 maps `vmo1` at `0x1000` and then injects failure across `limit` in `0..=2` while mapping
/// `vmo` across the 1 GiB `L1[0] -> L1[1]` boundary, verifying that rollback preserves `vmo1`'s
/// shared L0/L1/L2 tables.
pub fn test_device_aspace_map_failure_injection() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapFailureInjection\n");

    #[cfg(not(is_kernel))]
    {
        let aspace_start: u64 = 0x1000;
        let aspace_end: u64 = 1u64 << 48;
        let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
            return false;
        };
        let mut aspace = TestAspace(aspace);
        check!(verify_allocated_regions(&aspace, &[]));

        let segments = [(0x0070_0000, PAGE_SIZE), (0x0080_0000, PAGE_SIZE)];
        let vmo = MockPinnedVmo::new(&segments);
        let straddle_vaddr: u64 = (1u64 << 39) - PAGE_SIZE;
        let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;
        let mut inval = RecordingInvalidator::default();

        // `straddle_vaddr` requires 6 new intermediate table pages (`L0[0]`'s L1/L2/L3 + `L0[1]`'s
        // L1/L2/L3). Test every allocation countdown limit `0..=5` to verify failure, TLB
        // invalidation on rollback, and reclamation at each table level.
        #[cfg(not(is_kernel))]
        let pages_before = SmmuEnv::host_pmm_allocated_page_count();
        for limit in 0..=5u64 {
            inval = RecordingInvalidator::default();
            SmmuEnv::host_pmm_set_allocation_limit(limit);
            let res = aspace.map(&vmo, perms, &mut inval, Some(straddle_vaddr));
            SmmuEnv::host_pmm_clear_allocation_limit();
            check_eq!(res.err(), Some(Status::NO_MEMORY));
            // Since it failed, it should have unmapped and invalidated.
            check_eq!(inval.count, 1);
            check_eq!(inval.last_base, straddle_vaddr);
            check_eq!(inval.last_size, 2 * PAGE_SIZE);
            #[cfg(not(is_kernel))]
            {
                check_eq!(inval.pmm_pages_during_inval, pages_before + limit as usize);
                check_eq!(SmmuEnv::host_pmm_allocated_page_count(), pages_before);
            }

            // Confirm that nothing remained allocated in the RegionAllocator and the page tables
            // were properly cleaned up.
            check!(verify_allocated_regions(&aspace, &[]));
            check_eq!(aspace.page_cache().in_flight_pages(), 1);
            check_eq!(aspace.page_cache().cache_entries(), 0);
        }

        // Now map `vmo1` at `0x1000`, then inject failure while mapping `vmo2` at `(1 << 30) -
        // PAGE_SIZE` (which shares L0[0] and L1[0] with `vmo1` for its first page, then fails when
        // allocating L1[1]/L2[0]/L3[0] for its second page). Verify `vmo1` remains intact!
        let vmo1_segs = [(0x0090_0000, PAGE_SIZE)];
        let vmo1 = MockPinnedVmo::new(&vmo1_segs);
        let Ok(alloc1) = aspace.map(&vmo1, perms, &mut inval, Some(0x1000)) else {
            return false;
        };
        let expected_vmo1 =
            [ExpectedRegion { vaddr: 0x1000, paddr: 0x0090_0000, len: PAGE_SIZE, perms }];
        check!(verify_allocated_regions(&aspace, &expected_vmo1));

        let straddle_l1 = (1u64 << 30) - PAGE_SIZE;
        #[cfg(not(is_kernel))]
        let base_after_vmo1 = SmmuEnv::host_pmm_allocated_page_count();
        // Because `vmo1` at `0x1000` already allocated the L1 table under `L0[0]` and the L2 table
        // under `L1[0]`, `straddle_l1` needs 1 new page (`L3` under `L2[511]`) for page 0 and 2 new
        // pages (`L2` and `L3` under `L1[1]`) for page 1 (3 new pages total). Limit the countdown
        // to `0..=2` pages so it fails at each step and preserves `vmo1`'s shared L0/L1/L2 tables!
        for limit in 0..=2u64 {
            inval = RecordingInvalidator::default();
            SmmuEnv::host_pmm_set_allocation_limit(limit);
            let res2 = aspace.map(&vmo, perms, &mut inval, Some(straddle_l1));
            SmmuEnv::host_pmm_clear_allocation_limit();
            check_eq!(res2.err(), Some(Status::NO_MEMORY));
            check_eq!(inval.count, 1);
            check_eq!(inval.last_base, straddle_l1);
            check_eq!(inval.last_size, 2 * PAGE_SIZE);
            #[cfg(not(is_kernel))]
            {
                check_eq!(inval.pmm_pages_during_inval, base_after_vmo1 + limit as usize);
                check_eq!(SmmuEnv::host_pmm_allocated_page_count(), base_after_vmo1);
            }

            // `vmo1` must still be mapped and intact, and all tables allocated for `res2` must be
            // reclaimed.
            check!(verify_allocated_regions(&aspace, &expected_vmo1));
            check_eq!(aspace.page_cache().in_flight_pages(), 4);
        }

        aspace.unmap(alloc1, &mut inval);
        check!(verify_allocated_regions(&aspace, &[]));
        aspace.free_translation_tables(&mut inval);
    }

    true
}

/// Verifies reference-free reclamation of shared L0/L1/L2/L3 translation tables when 5 mappings
/// sharing progressively deeper table levels are mapped and then unmapped in reverse order.
pub fn test_device_aspace_shared_table_levels() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceSharedTableLevels\n");

    let aspace_start: u64 = 1u64 << 20;
    let aspace_end: u64 = 1u64 << 48;
    // Configure `max_cache_pages = 12` to retain all reclaimed intermediate table pages on unmap.
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 12) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    let seg = [(0x0100_0000, PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&seg);
    let perms = IOMMU_FLAG_PERM_READ;
    let mut inval = RecordingInvalidator::default();

    // Define a set of virtual addresses to map. The first one is our "base". The subsequent ones
    // share L0, L1, L2, and L3 tables with the base respectively.
    let base_vaddr: u64 = 2u64 << 39;
    let vaddr_l0 = base_vaddr + (1u64 << 39); // Different L0 index (shares L0 root table)
    let vaddr_l1 = base_vaddr + (1u64 << 30); // Different L1 index (shares L0, L1)
    let vaddr_l2 = base_vaddr + (1u64 << 21); // Different L2 index (shares L0, L1, L2)
    let vaddr_l3 = base_vaddr + (1u64 << 12); // Different L3 index (shares L0, L1, L2, L3)

    let reg_base = ExpectedRegion { vaddr: base_vaddr, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l3 = ExpectedRegion { vaddr: vaddr_l3, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l2 = ExpectedRegion { vaddr: vaddr_l2, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l1 = ExpectedRegion { vaddr: vaddr_l1, paddr: seg[0].0, len: PAGE_SIZE, perms };
    let reg_l0 = ExpectedRegion { vaddr: vaddr_l0, paddr: seg[0].0, len: PAGE_SIZE, perms };

    // 1. Map `base_vaddr` (`L0[2], L1[0], L2[0], L3[0]`): root + 3 new tables = 4 in flight.
    let Ok(alloc_base) = aspace.map(&vmo, perms, &mut inval, Some(base_vaddr)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 4);
    check!(verify_allocated_regions(&aspace, &[reg_base]));

    // 2. Map `vaddr_l0` (`L0[3], L1[0], L2[0], L3[0]`): +3 new tables = 7 in flight.
    let Ok(alloc_l0) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l0)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 7);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l0]));

    // 3. Map `vaddr_l1` (`L0[2], L1[1], L2[0], L3[0]`): shares L1, +2 new tables = 9 in flight.
    let Ok(alloc_l1) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l1)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 9);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l1, reg_l0]));

    // 4. Map `vaddr_l2` (`L0[2], L1[0], L2[1], L3[0]`): shares L1/L2, +1 new table = 10 in flight.
    let Ok(alloc_l2) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l2)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 10);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l2, reg_l1, reg_l0]));

    // 5. Map `vaddr_l3` (`L0[2], L1[0], L2[0], L3[1]`): shares L1/L2/L3, +0 tables = 10 in flight.
    let Ok(alloc_l3) = aspace.map(&vmo, perms, &mut inval, Some(vaddr_l3)) else {
        return false;
    };
    check_eq!(aspace.page_cache().in_flight_pages(), 10);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l3, reg_l2, reg_l1, reg_l0]));

    // Now unmap in reverse order (`alloc_l3`, `alloc_l2`, `alloc_l1`, `alloc_l0`, `alloc_base`):
    // Unmap `alloc_l3`: `alloc_base` still uses L3, so 0 pages reclaimed.
    aspace.unmap(alloc_l3, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 10);
    check_eq!(aspace.page_cache().cache_entries(), 0);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l2, reg_l1, reg_l0]));

    // Unmap `alloc_l2`: its L3 table is reclaimed (1 cached); L2 remains shared with `alloc_base`.
    aspace.unmap(alloc_l2, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 9);
    check_eq!(aspace.page_cache().cache_entries(), 1);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l1, reg_l0]));

    // Unmap `alloc_l1`: its L3 and L2 tables are reclaimed (3 cached); L1 remains shared with
    // `alloc_base`.
    aspace.unmap(alloc_l1, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 7);
    check_eq!(aspace.page_cache().cache_entries(), 3);
    check!(verify_allocated_regions(&aspace, &[reg_base, reg_l0]));

    // Unmap `alloc_l0`: its L3, L2, and L1 tables under `L0[3]` are reclaimed (6 cached).
    aspace.unmap(alloc_l0, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 4);
    check_eq!(aspace.page_cache().cache_entries(), 6);
    check!(verify_allocated_regions(&aspace, &[reg_base]));

    // Unmap `alloc_base`: its L3, L2, and L1 tables under `L0[2]` are reclaimed (9 cached).
    aspace.unmap(alloc_base, &mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 1);
    check_eq!(aspace.page_cache().cache_entries(), 9);
    check!(verify_allocated_regions(&aspace, &[]));

    aspace.free_translation_tables(&mut inval);
    check_eq!(aspace.page_cache().in_flight_pages(), 0);
    check_eq!(aspace.page_cache().cache_entries(), 0);
    true
}

/// Verifies alignment, `u64` overflow, bounds checks, overlap rejection, and adjacent region
/// allocation.
pub fn test_device_aspace_map_at_location_edge_cases() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] DeviceAspaceMapAtLocationEdgeCases\n");

    let aspace_start: u64 = 1u64 << 20;
    let aspace_len: u64 = (1u64 << 40) - aspace_start;
    let aspace_end = aspace_start + aspace_len;
    let Ok(aspace) = DeviceAspace::create(aspace_start, aspace_len, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    check!(verify_allocated_regions(&aspace, &[]));

    let segs = [(0x0200_0000, 2 * PAGE_SIZE)];
    let vmo = MockPinnedVmo::new(&segs);
    let unaligned_vmo = MockPinnedVmo::new_with_size(&segs, 2 * PAGE_SIZE - 1);
    let perms = IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE;
    let mut inval = RecordingInvalidator::default();

    // 1. Unaligned location or unaligned VMO size -> `INVALID_ARGS` with no TLB invalidation.
    // A. Location not page aligned.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x1000_0001)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // B. Size not page aligned.
    check_eq!(
        aspace.map(&unaligned_vmo, perms, &mut inval, Some(0x1000_0000)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // 2. `u64` addition overflow (`u64::MAX - PAGE_SIZE + 1 + 2 * PAGE_SIZE`) -> `INVALID_ARGS`.
    // Use a location close to `u64::MAX` such that `location + size` overflows.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(u64::MAX - PAGE_SIZE + 1)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // 3. Below `aspace_start` or extending past `aspace_end` -> `INVALID_ARGS`.
    // A. Region starts before `first_valid_address()`.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(aspace_start - PAGE_SIZE)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // B. Region ends after `last_valid_address()` (`aspace_end` is the exclusive end of the valid
    //    address range, i.e. the last valid address is `aspace_end - 1`).
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(aspace_end - PAGE_SIZE)).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(inval.count, 0);

    // 4. Map at `0x1000_0000` (`[0x1000_0000, 0x1000_2000)`), then test overlapping requests
    //    (`ALREADY_EXISTS` with no TLB invalidation) and adjacent requests (`Ok`).
    // Map first region successfully.
    let Ok(alloc_mid) = aspace.map(&vmo, perms, &mut inval, Some(0x1000_0000)) else {
        return false;
    };
    inval = RecordingInvalidator::default();
    // Attempt to map another region overlapping the first region:
    // A. Exactly the same location.
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x1000_0000)).err(),
        Some(Status::ALREADY_EXISTS)
    );
    check_eq!(inval.count, 0);
    // B. Partially overlapping location (1 page offset forward).
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x1000_1000)).err(),
        Some(Status::ALREADY_EXISTS)
    );
    check_eq!(inval.count, 0);
    // C. Partially overlapping location (1 page offset backward).
    check_eq!(
        aspace.map(&vmo, perms, &mut inval, Some(0x0fff_f000)).err(),
        Some(Status::ALREADY_EXISTS)
    );
    check_eq!(inval.count, 0);

    // Immediately before (`0x0fff_e000`) and immediately after (`0x1000_2000`) must succeed.
    let Ok(alloc_before) = aspace.map(&vmo, perms, &mut inval, Some(0x0fff_e000)) else {
        return false;
    };
    let Ok(alloc_after) = aspace.map(&vmo, perms, &mut inval, Some(0x1000_2000)) else {
        return false;
    };

    aspace.unmap(alloc_before, &mut inval);
    aspace.unmap(alloc_mid, &mut inval);
    aspace.unmap(alloc_after, &mut inval);
    check!(verify_allocated_regions(&aspace, &[]));
    aspace.free_translation_tables(&mut inval);
    true
}

// ============================================================================
// 8. `StreamMatchRegGroup` Unit Tests
// ============================================================================

/// Test guard that ensures `StreamMatchRegGroup::disable` runs even if a `check!` returns early.
struct TestSmrg(fbl::UniquePtr<StreamMatchRegGroup>);

impl core::ops::Deref for TestSmrg {
    type Target = StreamMatchRegGroup;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for TestSmrg {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestSmrg {
    fn drop(&mut self) {
        if !self.0.gr0_base().is_null() {
            self.0.disable();
        }
    }
}

/// Tests `StreamMatchRegGroup` creation, lockdown, adoption (valid `Translation`/`Bypass`/`Fault`
/// vs. invalid `S2crType::Invalid` and out-of-range `cbndx`), `enable_for_context_bank`,
/// `invalidate`, `revalidate`, `disable`, and `disable_at`.
pub fn test_stream_match_reg_group() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] StreamMatchRegGroup\n");

    let Ok(mut fake_gr0) = fbl::Array::<u64>::try_new(512) else {
        return false;
    };
    // SAFETY: `fake_gr0` is a 4096-byte heap buffer that outlives `gr0_base`.
    let gr0_base = unsafe { SmmuMmio::with_size(fake_gr0.as_mut_ptr().addr(), 4096) };
    let mut avail_smrgs = Bitmask128::new();
    avail_smrgs.set_lowest_n_bits(4);
    avail_smrgs.clear_bit(2); // Mark SMRG #2 unavailable (`ALREADY_BOUND`).
    let mut avail_cbs = Bitmask128::new();
    avail_cbs.set_lowest_n_bits(4);

    let smmu = SmmuParams {
        name: "SMMU@smrg_test",
        op_mode: ArmSmmuMode::Enforced,
        num_smrgs: 4,
        num_cbs: 4,
        available_smrgs: &avail_smrgs,
        available_cbs: &avail_cbs,
        gr0_base,
        gr1_base: SmmuMmio::null(),
        cb0_base: SmmuMmio::null(),
        reg_page_size: 4096,
    };

    let sid = SmrValue::new(0x0123, 0x0003);
    // Index validation: `>= num_smrgs` -> `INVALID_ARGS`, unavailable bit -> `ALREADY_BOUND`.
    check_eq!(
        StreamMatchRegGroup::create_and_lockdown(&smmu, 4, sid).err(),
        Some(Status::INVALID_ARGS)
    );
    check_eq!(
        StreamMatchRegGroup::create_and_lockdown(&smmu, 2, sid).err(),
        Some(Status::ALREADY_BOUND)
    );

    // `create_and_lockdown` disables SMR[0] (`valid = 0, id = 0, mask = 0`) and sets S2CR[0] to
    // `S2crType::Fault`.
    gr0::Smr::from_value(0xffff_ffff).write_at(&gr0_base, 0);
    let Ok(smrg) = StreamMatchRegGroup::create_and_lockdown(&smmu, 0, sid) else {
        return false;
    };
    let mut smrg = TestSmrg(smrg);
    check_eq!(smrg.smrg_ndx(), 0);
    check_eq!(smrg.stream_ids(), sid);
    check_eq!(smrg.mode(), S2crType::Invalid);
    check_eq!(smrg.cb_ndx(), StreamMatchRegGroup::INVALID_CB_NDX);
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).valid(), 0);
    check_eq!(gr0::S2cr::read_at(&gr0_base, 0).s2cr_type(), S2crType::Fault);

    // `enable_for_context_bank(1)` programs S2CR[0] (`Translation`, `cbndx = 1`, `privcfg = 2`)
    // and SMR[0] (`valid = 1`, `id = 0x0123`, written exactly as given, `mask = 0x0003`).
    smrg.enable_for_context_bank(1);
    check_eq!(smrg.mode(), S2crType::Translation);
    check_eq!(smrg.cb_ndx(), 1);
    let s2cr0 = gr0::S2crType0::from_value(gr0::S2cr::read_at(&gr0_base, 0).reg_value());
    check_eq!(s2cr0.s2cr_type(), S2crType::Translation);
    check_eq!(s2cr0.cbndx(), 1);
    check_eq!(s2cr0.privcfg(), S2crPrivCfg::Unprivileged);
    let smr0 = gr0::Smr::read_at(&gr0_base, 0);
    check_eq!(smr0.valid(), 1);
    check_eq!(smr0.id(), 0x0123);
    check_eq!(smr0.mask(), 0x0003);

    // `invalidate` and `revalidate` toggle `SMR.VALID` while preserving `ID` and `MASK`.
    smrg.invalidate();
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).valid(), 0);
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).id(), 0x0123);
    smrg.revalidate();
    check_eq!(gr0::Smr::read_at(&gr0_base, 0).valid(), 1);

    // `disable` resets registers and internal fields so `Drop` succeeds cleanly.
    smrg.disable();
    check_eq!(smrg.mode(), S2crType::Invalid);
    check_eq!(smrg.cb_ndx(), StreamMatchRegGroup::INVALID_CB_NDX);
    check!(smrg.gr0_base().is_null());
    drop(smrg);

    // `create_and_adopt` tests:
    // 1. Adopting `S2crType::Invalid` -> `Err(Status::BAD_STATE)` with clean Drop.
    gr0::S2cr::from_value(0).set_s2cr_type(S2crType::Invalid).write_at(&gr0_base, 1);
    check_eq!(StreamMatchRegGroup::create_and_adopt(&smmu, 1, sid).err(), Some(Status::BAD_STATE));

    // 2. Adopting `S2crType::Translation` with out-of-range `cbndx = 7 >= num_cbs` ->
    //    `Err(Status::OUT_OF_RANGE)` with clean Drop.
    gr0::S2crType0::from_value(0)
        .set_s2cr_type(S2crType::Translation)
        .set_cbndx(7)
        .write_at(&gr0_base, 1);
    check_eq!(
        StreamMatchRegGroup::create_and_adopt(&smmu, 1, sid).err(),
        Some(Status::OUT_OF_RANGE)
    );

    // 3. Adopting valid `S2crType::Translation` (`cbndx = 2`) and `S2crType::Bypass`.
    gr0::S2crType0::from_value(0)
        .set_s2cr_type(S2crType::Translation)
        .set_cbndx(2)
        .write_at(&gr0_base, 1);
    let Ok(adopted_trans) = StreamMatchRegGroup::create_and_adopt(&smmu, 1, sid) else {
        return false;
    };
    let mut adopted_trans = TestSmrg(adopted_trans);
    check_eq!(adopted_trans.mode(), S2crType::Translation);
    check_eq!(adopted_trans.cb_ndx(), 2);
    adopted_trans.disable();
    drop(adopted_trans);

    gr0::S2cr::from_value(0).set_s2cr_type(S2crType::Bypass).write_at(&gr0_base, 3);
    let Ok(adopted_bypass) = StreamMatchRegGroup::create_and_adopt(&smmu, 3, sid) else {
        return false;
    };
    let mut adopted_bypass = TestSmrg(adopted_bypass);
    check_eq!(adopted_bypass.mode(), S2crType::Bypass);
    check_eq!(adopted_bypass.cb_ndx(), StreamMatchRegGroup::INVALID_CB_NDX);
    adopted_bypass.disable();
    drop(adopted_bypass);

    // `disable_at` static helper.
    gr0::Smr::from_value(0).set_valid(1).set_id(0x55).write_at(&gr0_base, 3);
    StreamMatchRegGroup::disable_at(&smmu, 3);
    check_eq!(gr0::Smr::read_at(&gr0_base, 3).valid(), 0);
    check_eq!(gr0::S2cr::read_at(&gr0_base, 3).s2cr_type(), S2crType::Fault);

    true
}

// ============================================================================
// 9. `ContextBank` Unit Tests
// ============================================================================

/// Test guard that ensures `ContextBank` transitions to `BtiMode::Shutdown` even if a `check!`
/// returns early.
struct TestCb<'a>(fbl::UniquePtr<ContextBank>, &'a str);

impl core::ops::Deref for TestCb<'_> {
    type Target = ContextBank;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl core::ops::DerefMut for TestCb<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl Drop for TestCb<'_> {
    fn drop(&mut self) {
        if self.0.mode() != BtiMode::Shutdown {
            if self.0.mode() == BtiMode::Adopted {
                self.0.force_mode_for_test(BtiMode::Fault);
            }
            self.0.set_mode(self.1, BtiMode::Shutdown);
        }
    }
}

/// Tests `ContextBank` lockdown, mode transitions (`Fault`, `Bypass`, `Translation`, `Shutdown`),
/// fault interrupt toggling, ASID and region TLB invalidations
/// (`INVALIDATE_ALL_PAGE_COUNT_THRESHOLD`), and hardware state adoption across 64-bit, Ext32Bit,
/// and 32-bit addressing modes.
pub fn test_context_bank() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] ContextBank\n");

    let Ok(mut fake_regs) = fbl::Array::<u64>::try_new(512 * 6) else {
        return false;
    };
    let base_vaddr = fake_regs.as_mut_ptr().addr();
    // SAFETY: `fake_regs` is a 6 * 4096-byte heap buffer that outlives `gr0_base`, `gr1_base`,
    // and `cb0_base`.
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
    avail_cbs.clear_bit(3); // Mark CB #3 unavailable.

    let smmu = SmmuParams {
        name: "SMMU@cb_test",
        op_mode: ArmSmmuMode::Disabled,
        num_smrgs: 4,
        num_cbs: 4,
        available_smrgs: &avail_smrgs,
        available_cbs: &avail_cbs,
        gr0_base,
        gr1_base,
        cb0_base,
        reg_page_size: 4096,
    };

    // Index validation: `>= num_cbs` -> `INVALID_ARGS`, unavailable bit -> `ALREADY_BOUND`.
    check_eq!(ContextBank::create_and_lockdown(&smmu, 4).err(), Some(Status::INVALID_ARGS));
    check_eq!(ContextBank::create_and_lockdown(&smmu, 3).err(), Some(Status::ALREADY_BOUND));

    let Ok(cb) = ContextBank::create_and_lockdown(&smmu, 1) else {
        return false;
    };
    let mut cb = TestCb(cb, smmu.name);
    let cb1_base = smmu.get_cb_base(1);
    check_eq!(cb.cb_ndx(), 1);
    check_eq!(cb.asid(), ContextBank::UPPER_ASID_BITS | 1);
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb1_base).asid(), UNUSED_ASID as u64);
    check_eq!(s1cbr::Ttbr1_64Bit::read_from(&cb1_base).asid(), UNUSED_ASID as u64);

    // Transition to `BtiMode::Fault`, then `BtiMode::Bypass`, then back to `BtiMode::Fault`.
    cb.set_mode(smmu.name, BtiMode::Fault);
    check_eq!(cb.mode(), BtiMode::Fault);
    check_eq!(cb.addr_mode(), AddrMode::Mode64Bit);
    check_eq!(cb.aspace_size(), 0);
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).m(), 1);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd0(), 1);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd1(), 1);

    cb.set_mode(smmu.name, BtiMode::Bypass);
    check_eq!(cb.mode(), BtiMode::Bypass);
    check_eq!(cb.aspace_size(), 1u64 << 48);
    check_eq!(gr1::Cbar::read_at(&gr1_base, 1).cbar_type(), CbarType::S1TS2Bypass);
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).m(), 0);

    cb.set_mode(smmu.name, BtiMode::Fault);
    check_eq!(cb.mode(), BtiMode::Fault);

    // Attach a `DeviceAspace` and transition to `BtiMode::Translation`.
    let Ok(aspace) = DeviceAspace::create(0x1000, (1u64 << 48) - 0x1000, 0) else {
        return false;
    };
    let mut aspace = TestAspace(aspace);
    cb.attach_aspace(&aspace);
    cb.set_mode(smmu.name, BtiMode::Translation);
    check_eq!(cb.mode(), BtiMode::Translation);
    check_eq!(cb.aspace_size(), 1u64 << (64 - DEFAULT_TCR_T0SZ));
    check!(cb.ttbrs()[0].enabled);
    check_eq!(cb.ttbrs()[0].ttbr_paddr, aspace.root_tt_paddr());
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb1_base).base_addr_value(), aspace.root_tt_paddr());
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb1_base).asid(), cb.asid() as u64);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd0(), 0);
    check_eq!(s1cbr::Tcr64Bit::read_from(&cb1_base).epd1(), 1);
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).m(), 1);

    // Fault interrupt toggling (`SCTLR.CFIE`).
    cb.enable_fault_interrupt();
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).cfie(), 1);
    cb.disable_fault_interrupt();
    check_eq!(s1cbr::Sctlr::read_from(&cb1_base).cfie(), 0);

    // TLB invalidation by region (below threshold `<= 256` pages uses `TLBIVA`; above threshold
    // `> 256` pages falls back to `TLBIASID`).
    s1cbr::Tlbiasid::from_value(0).write_to(&cb1_base);
    s1cbr::TlbivaAArch64::from_value(0).write_to(&cb1_base);
    let mut inval_op = cb.make_inval_op();
    inval_op.invalidate(0x1234_5000, 2 * PAGE_SIZE);
    let tlbiva = s1cbr::TlbivaAArch64::read_from(&cb1_base);
    check_eq!(tlbiva.asid(), cb.asid() as u64);
    check_eq!(tlbiva.address(), 0x1234_6000);
    check_eq!(s1cbr::Tlbiasid::read_from(&cb1_base).asid(), 0);

    // Region larger than `INVALIDATE_ALL_PAGE_COUNT_THRESHOLD` (257 pages) must issue `TLBIASID`.
    inval_op.invalidate(0x1000_0000, (INVALIDATE_ALL_PAGE_COUNT_THRESHOLD + 1) * PAGE_SIZE);
    check_eq!(s1cbr::Tlbiasid::read_from(&cb1_base).asid(), cb.asid() as u32);

    // Zero-size invalidation via `ContextBankInvalidator` also issues `TLBIASID`.
    s1cbr::Tlbiasid::from_value(0).write_to(&cb1_base);
    aspace.free_translation_tables(&mut inval_op);
    check_eq!(s1cbr::Tlbiasid::read_from(&cb1_base).asid(), cb.asid() as u32);

    // Shut down `cb` cleanly.
    cb.set_mode(smmu.name, BtiMode::Shutdown);
    check_eq!(cb.mode(), BtiMode::Shutdown);
    check!(cb.gr1_base().is_null());
    check!(cb.cb_base().is_null());
    drop(cb);

    // Test `ContextBank::create_and_adopt` across:
    // (a) Stage-1 64-bit translation (`VA64 = 1`, `SCTLR.M = 1`, `TCR.T0SZ = 16`, `EPD0 = 0`,
    //     `EPD1 = 1`).
    let cb2_base = smmu.get_cb_base(2);
    gr1::Cbar::from_value(0).set_cbar_type(CbarType::S1TS2Bypass).write_at(&gr1_base, 2);
    gr1::Cba2r::from_value(0).set_va64(Cba2rVa64::Va64).write_at(&gr1_base, 2);
    s1cbr::Tcr64Bit::from_value(0)
        .set_t0sz(16)
        .set_t1sz(24)
        .set_tg0(TcrTg0::Granule4KB)
        .set_epd0(0)
        .set_epd1(1)
        .write_to(&cb2_base);
    s1cbr::Ttbr0_64Bit::from_value(0).set_base_addr_from_value(0xabcd_0000).write_to(&cb2_base);
    s1cbr::Sctlr::from_value(0).set_m(1).write_to(&cb2_base);

    let Ok(adopted_64) = ContextBank::create_and_adopt(&smmu, 2) else {
        return false;
    };
    let mut adopted_64 = TestCb(adopted_64, smmu.name);
    check_eq!(adopted_64.mode(), BtiMode::Adopted);
    check_eq!(adopted_64.addr_mode(), AddrMode::Mode64Bit);
    check!(adopted_64.ttbrs()[0].enabled);
    check!(!adopted_64.ttbrs()[1].enabled);
    check_eq!(adopted_64.ttbrs()[0].granule_size_bits, 12);
    check_eq!(adopted_64.ttbrs()[0].first_valid_addr, 0);
    check_eq!(adopted_64.ttbrs()[0].last_valid_addr, (1u64 << 48) - 1);
    check_eq!(adopted_64.ttbrs()[0].ttbr_paddr, 0xabcd_0000);
    adopted_64.force_mode_for_test(BtiMode::Fault);
    adopted_64.set_mode(smmu.name, BtiMode::Shutdown);
    drop(adopted_64);

    // (b) Stage-1 Ext32Bit translation (`VA64 = 0`, `TCR.EAE = 1`, `T0SZ = 2`, `T1SZ = 0`).
    gr1::Cba2r::from_value(0).set_va64(Cba2rVa64::Va32).write_at(&gr1_base, 2);
    s1cbr::TcrExt32Bit::from_value(0)
        .set_eae(1)
        .set_t0sz(2)
        .set_t1sz(0)
        .set_epd0(0)
        .set_epd1(1)
        .write_to(&cb2_base);
    s1cbr::Ttbr0Ext32Bit::from_value(0).set_base_addr_from_value(0x1234_0000).write_to(&cb2_base);
    s1cbr::Sctlr::from_value(0).set_m(1).write_to(&cb2_base);

    let Ok(adopted_ext32) = ContextBank::create_and_adopt(&smmu, 2) else {
        return false;
    };
    let mut adopted_ext32 = TestCb(adopted_ext32, smmu.name);
    check_eq!(adopted_ext32.mode(), BtiMode::Adopted);
    check_eq!(adopted_ext32.addr_mode(), AddrMode::Ext32Bit);
    check_eq!(adopted_ext32.ttbrs()[0].last_valid_addr, (1u64 << 30) - 1);
    check_eq!(adopted_ext32.ttbrs()[0].ttbr_paddr, 0x1234_0000);
    adopted_ext32.force_mode_for_test(BtiMode::Fault);
    adopted_ext32.set_mode(smmu.name, BtiMode::Shutdown);
    drop(adopted_ext32);

    // (c) Stage-1 32-bit short-descriptor translation (`VA64 = 0`, `TCR.EAE = 0`, `T0SZ = 1`).
    gr1::Cba2r::from_value(0).set_va64(Cba2rVa64::Va32).write_at(&gr1_base, 2);
    s1cbr::Tcr32Bit::from_value(0).set_eae(0).set_t0sz(1).set_pd0(0).set_pd1(1).write_to(&cb2_base);
    s1cbr::Ttbr0_32Bit::from_value(0).set_base_addr_from_value(0x5678_0000).write_to(&cb2_base);
    s1cbr::Sctlr::from_value(0).set_m(1).write_to(&cb2_base);

    let Ok(adopted_32) = ContextBank::create_and_adopt(&smmu, 2) else {
        return false;
    };
    let mut adopted_32 = TestCb(adopted_32, smmu.name);
    check_eq!(adopted_32.mode(), BtiMode::Adopted);
    check_eq!(adopted_32.addr_mode(), AddrMode::Mode32Bit);
    check_eq!(adopted_32.ttbrs()[0].last_valid_addr, (1u64 << 31) - 1);
    check_eq!(adopted_32.ttbrs()[1].first_valid_addr, 1u64 << 31);
    check_eq!(adopted_32.ttbrs()[1].last_valid_addr, 0xFFFF_FFFF);
    check_eq!(adopted_32.ttbrs()[0].ttbr_paddr, 0x5678_0000);
    adopted_32.force_mode_for_test(BtiMode::Fault);
    adopted_32.set_mode(smmu.name, BtiMode::Shutdown);
    drop(adopted_32);

    // `ContextBank::disable_at` static helper.
    ContextBank::disable_at(&smmu, 0);
    check_eq!(s1cbr::Ttbr0_64Bit::read_from(&cb0_base).asid(), UNUSED_ASID as u64);

    true
}

// ============================================================================
// 10. `define_smmu_reg!` Formatting & Bitfields Unit Tests
// ============================================================================

/// Tests `define_smmu_reg!` 32-bit, 64-bit, and indexed register bitfields, `RSVDZ_MASK` write
/// masking, enum conversions, `FieldDesc::extract`, and string formatting helpers.
pub fn test_registers_and_bitfields() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] RegistersAndBitfields\n");

    let Ok(mut fake_mmio) = fbl::Array::<u64>::try_new(512) else {
        return false;
    };
    // SAFETY: `fake_mmio` is a 4096-byte heap buffer that outlives `mmio`.
    let mmio = unsafe { SmmuMmio::with_size(fake_mmio.as_mut_ptr().addr(), 4096) };

    // 1. 32-bit fixed-offset register (`gr0::Cr0`) round-trip and `RSVDZ_MASK` stripping on write.
    let mut cr0 = gr0::Cr0::from_value(u32::MAX);
    check_eq!(cr0.clientpd(), ClientPd::Disabled);
    check_eq!(cr0.stalld(), Stalld::Disabled);
    check_eq!(cr0.exidenable(), ExidEnable::Enabled);
    cr0.write_to(&mmio);
    let cr0_readback = gr0::Cr0::read_from(&mmio);
    check_eq!(cr0_readback.reg_value() & gr0::Cr0::RSVDZ_MASK, 0);
    check_eq!(cr0_readback.reg_value(), !gr0::Cr0::RSVDZ_MASK);

    cr0.set_clientpd(ClientPd::Enabled)
        .set_stalld(Stalld::Enabled)
        .set_exidenable(ExidEnable::Disabled)
        .set_usfcfg(1)
        .set_gfre(1)
        .set_gfie(1)
        .write_to(&mmio);
    let cr0_mod = gr0::Cr0::read_from(&mmio);
    check_eq!(cr0_mod.clientpd(), ClientPd::Enabled);
    check_eq!(cr0_mod.stalld(), Stalld::Enabled);
    check_eq!(cr0_mod.exidenable(), ExidEnable::Disabled);
    check_eq!(cr0_mod.usfcfg(), 1);
    check_eq!(cr0_mod.gfre(), 1);
    check_eq!(cr0_mod.gfie(), 1);

    // 2. Bitfields in `gr0::Idr0`.
    let mut idr0 = gr0::Idr0::from_value(0);
    idr0.set_s1ts(1)
        .set_s2ts(0)
        .set_nts(1)
        .set_sms(1)
        .set_cttw(1)
        .set_numirpt(16)
        .set_numsmrg(64)
        .write_to(&mmio);
    let idr0_rb = gr0::Idr0::read_from(&mmio);
    check_eq!(idr0_rb.s1ts(), 1);
    check_eq!(idr0_rb.s2ts(), 0);
    check_eq!(idr0_rb.nts(), 1);
    check_eq!(idr0_rb.sms(), 1);
    check_eq!(idr0_rb.cttw(), 1);
    check_eq!(idr0_rb.numirpt(), 16);
    check_eq!(idr0_rb.numsmrg(), 64);
    check_eq!(idr0_rb.reg_value() & gr0::Idr0::RSVDZ_MASK, 0);

    // 3. 64-bit register (`s1cbr::Ttbr0_64Bit`, `s2cbr::Ttbr0`, `s1cbr::Far`) round-trip and
    //    `RSVDZ_MASK` enforcement.
    let mut ttbr0 = s1cbr::Ttbr0_64Bit::from_value(u64::MAX);
    ttbr0.write_to(&mmio);
    let ttbr0_rb = s1cbr::Ttbr0_64Bit::read_from(&mmio);
    check_eq!(ttbr0_rb.reg_value() & s1cbr::Ttbr0_64Bit::RSVDZ_MASK, 0);

    let test_pa: u64 = 0x0000_DEAD_BEEF_F000;
    s1cbr::Ttbr0_64Bit::from_value(0)
        .set_asid(0x5A03)
        .set_base_addr_from_value(test_pa)
        .write_to(&mmio);
    let ttbr0_pa = s1cbr::Ttbr0_64Bit::read_from(&mmio);
    check_eq!(ttbr0_pa.asid(), 0x5A03);
    check_eq!(ttbr0_pa.base_addr_value(), test_pa);

    s2cbr::Ttbr0::from_value(u64::MAX).write_to(&mmio);
    check_eq!(s2cbr::Ttbr0::read_from(&mmio).reg_value() & s2cbr::Ttbr0::RSVDZ_MASK, 0);
    s2cbr::Ttbr0::from_value(0).set_base_addr_from_value(test_pa).write_to(&mmio);
    check_eq!(s2cbr::Ttbr0::read_from(&mmio).base_addr_value(), test_pa);

    s1cbr::Far::from_value(0).set_faddr(0xCAFE_BABE_1234_5678).write_to(&mmio);
    check_eq!(s1cbr::Far::read_from(&mmio).faddr(), 0xCAFE_BABE_1234_5678);

    // 4. Indexed register arrays (`gr0::Smr`, `gr0::S2cr`, `gr1::Cbar`, `gr1::Cba2r`,
    //    `gr1::Cbfrsynra`) across indices `0..4`.
    for ndx in 0..4u32 {
        check_eq!(gr0::Smr::offset(ndx), 0x800 + (ndx << 2));
        check_eq!(gr0::S2cr::offset(ndx), 0xc00 + (ndx << 2));
        check_eq!(gr1::Cbar::offset(ndx), ndx << 2);
        check_eq!(gr1::Cbfrsynra::offset(ndx), 0x400 + (ndx << 2));
        check_eq!(gr1::Cba2r::offset(ndx), 0x800 + (ndx << 2));

        gr0::Smr::from_value(u32::MAX).write_at(&mmio, ndx);
        check_eq!(gr0::Smr::read_at(&mmio, ndx).reg_value() & gr0::Smr::RSVDZ_MASK, 0);

        gr1::Cba2r::from_value(u32::MAX).write_at(&mmio, ndx);
        check_eq!(gr1::Cba2r::read_at(&mmio, ndx).reg_value() & gr1::Cba2r::RSVDZ_MASK, 0);
    }

    // 5. Enum `from_raw` and string helper coverage.
    check_eq!(S2crType::from_raw(0), S2crType::Translation);
    check_eq!(S2crType::from_raw(1), S2crType::Bypass);
    check_eq!(S2crType::from_raw(2), S2crType::Fault);
    check_eq!(S2crType::from_raw(3), S2crType::Invalid);

    check_eq!(S2crPrivCfg::from_raw(0), S2crPrivCfg::Default);
    check_eq!(S2crPrivCfg::from_raw(1), S2crPrivCfg::Reserved1);
    check_eq!(S2crPrivCfg::from_raw(2), S2crPrivCfg::Unprivileged);
    check_eq!(S2crPrivCfg::from_raw(3), S2crPrivCfg::Privileged);

    check_eq!(CbarType::from_raw(0), CbarType::S2Translation);
    check_eq!(CbarType::from_raw(1), CbarType::S1TS2Bypass);
    check_eq!(CbarType::from_raw(2), CbarType::S1TS2Fault);
    check_eq!(CbarType::from_raw(3), CbarType::S1TS2Translate);

    check_eq!(Cba2rVa64::from_raw(0), Cba2rVa64::Va32);
    check_eq!(Cba2rVa64::from_raw(1), Cba2rVa64::Va64);
    check_eq!(TcrTg0::from_raw(0), TcrTg0::Granule4KB);
    check_eq!(TcrTg0::from_raw(1), TcrTg0::Granule64KB);
    check_eq!(TcrTg0::from_raw(2), TcrTg0::Granule16KB);
    check_eq!(TcrTg0::from_raw(3), TcrTg0::Reserved);
    check_eq!(TcrTg1::from_raw(0), TcrTg1::Reserved0);
    check_eq!(TcrTg1::from_raw(1), TcrTg1::Granule16KB);
    check_eq!(TcrTg1::from_raw(2), TcrTg1::Granule4KB);
    check_eq!(TcrTg1::from_raw(3), TcrTg1::Granule64KB);
    check_eq!(TcrSh0::from_raw(3), TcrSh0::InnerShareable);
    check_eq!(TcrOrg0::from_raw(1), TcrOrg0::WriteBackWriteAllocate);
    check_eq!(TcrIrg0::from_raw(1), TcrIrg0::WriteBackWriteAllocate);

    check_eq!(arm_smmu_mode_to_string(ArmSmmuMode::Disabled), "Disabled");
    check_eq!(arm_smmu_mode_to_string(ArmSmmuMode::Passthru), "Passthru");
    check_eq!(arm_smmu_mode_to_string(ArmSmmuMode::Enforced), "Enforced");
    check_eq!(arm_s2cr_type_to_string(S2crType::Translation), "Translation");
    check_eq!(
        arm_cbar_type_to_string(CbarType::S1TS2Bypass),
        "Translate: Stage 1 Context with Stage 2 Bypass"
    );
    check_eq!(addr_mode_to_string(AddrMode::Mode64Bit), "64Bit");
    check_eq!(bti_mode_to_string(BtiMode::Translation), "Translation");

    // 6. `FieldDesc::extract` and generated `FIELDS` metadata tables.
    let fd_single = FieldDesc { name: "BIT5", msb: 5, lsb: 5 };
    check_eq!(fd_single.extract(1u64 << 5), 1);
    check_eq!(fd_single.extract(!(1u64 << 5)), 0);

    let fd_full = FieldDesc { name: "ALL64", msb: 63, lsb: 0 };
    check_eq!(fd_full.extract(0xDEAD_BEEF_CAFE_BABE), 0xDEAD_BEEF_CAFE_BABE);

    check!(!gr0::Cr0::FIELDS.is_empty());
    check!(!gr0::Cr2::FIELDS.is_empty());
    check!(!gr0::Smr::FIELDS.is_empty());
    check!(!s1cbr::Ttbr0_64Bit::FIELDS.is_empty());
    // Verify that `[rsvdz]` fields in `gr0::Cr2::FIELDS` have `name == ""` while named fields in
    // `gr0::Smr::FIELDS` have non-empty names.
    let has_rsvdz_field = gr0::Cr2::FIELDS.iter().any(|f| f.name.is_empty());
    let has_valid_field =
        gr0::Smr::FIELDS.iter().any(|f| f.name == "VALID" && f.msb == 31 && f.lsb == 31);
    check!(has_rsvdz_field);
    check!(has_valid_field);

    // Exercise `dump_reg` formatting for both 32-bit (`Cr0`) and 64-bit (`Ttbr0_64Bit`) registers.
    super::v2::smmu::dump_reg(
        "GR0.CR0",
        cr0_mod.reg_value() as u64,
        4,
        gr0::Cr0::OFFSET,
        gr0::Cr0::FIELDS,
        &mmio,
        1,
    );
    super::v2::smmu::dump_reg(
        "CB0.TTBR0",
        ttbr0_pa.reg_value(),
        8,
        s1cbr::Ttbr0_64Bit::OFFSET,
        s1cbr::Ttbr0_64Bit::FIELDS,
        &mmio,
        1,
    );

    true
}

// ============================================================================
// 11. `EscapedName` Unit Tests
// ============================================================================

/// Tests that `EscapedName` stops at the first NUL, prints valid UTF-8 unchanged, escapes bytes
/// which are not valid UTF-8 as `\xNN`, and substitutes `<unknown>` for empty names on request.
pub fn test_escaped_name() -> bool {
    dprintf!(INFO, "[Backend: Rust->Rust] EscapedName\n");

    let cases: &[(&[u8], &str)] = &[
        (b"", ""),
        (b"\0ignored", ""),
        (b"usb-bti\0\0\0", "usb-bti"),
        (b"no-terminator", "no-terminator"),
        // Valid multi-byte UTF-8 is printed as is.
        ("caf\u{e9}".as_bytes(), "caf\u{e9}"),
        // Latin-1 and other invalid bytes are escaped, keeping the valid text around them.
        (b"caf\xe9\0", "caf\\xe9"),
        (b"\xff\xfename", "\\xff\\xfename"),
        // A multi-byte character split by truncation is escaped byte by byte.
        (b"name-\xe2\x82", "name-\\xe2\\x82"),
    ];
    for &(raw, expected) in cases {
        let mut buf = TagBuf::new();
        check!(write!(&mut buf, "{}", EscapedName::new(raw)).is_ok());
        check_eq!(buf.as_str(), expected);
        check_eq!(EscapedName::new(raw).is_empty(), expected.is_empty());
    }

    let mut buf = TagBuf::new();
    check!(write!(&mut buf, "{}", EscapedName::new_or_unknown(b"\0")).is_ok());
    check_eq!(buf.as_str(), "<unknown>");
    buf.clear();
    check!(write!(&mut buf, "{}", EscapedName::new_or_unknown(b"x\0")).is_ok());
    check_eq!(buf.as_str(), "x");

    true
}

/// Runs all 22 native Rust (`Rust -> Rust`) component unit tests in sequence.
pub fn run_all_unit_tests() -> bool {
    test_smmu_mode_defaults()
        && test_smmu_mode_advanced()
        && test_smmu_mode_case_insensitive()
        && test_smmu_mode_whitespace()
        && test_smmu_mode_validation()
        && test_vmsav8_64_descriptors()
        && test_smr_value_and_iter()
        && test_bitmask128()
        && test_page_cache_add_and_get()
        && test_page_cache_trim()
        && test_page_cache_oom()
        && test_translation_table_helper_walk_and_reclaim()
        && test_device_aspace_map_and_unmap()
        && test_device_aspace_map_at_specific_location()
        && test_device_aspace_map_straddling_table_levels()
        && test_device_aspace_map_failure_injection()
        && test_device_aspace_shared_table_levels()
        && test_device_aspace_map_at_location_edge_cases()
        && test_stream_match_reg_group()
        && test_context_bank()
        && test_registers_and_bitfields()
        && test_escaped_name()
}
