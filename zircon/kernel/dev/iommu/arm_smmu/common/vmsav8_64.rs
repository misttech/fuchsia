// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARMv8-A 64-bit VMSAv8-64 stage-1 page table format definitions.

use core::sync::atomic::{AtomicU64, Ordering};

use super::constants::ALL_PERM_FLAGS;
use super::env::{HwVisibleWrite, SmmuEnv};

/// VMSAv8-64 with 4k granules means 512 64-bit entries per page.
pub const ENTRIES_PER_PAGE: usize = 512;

/// All of the addresses we are using here should all be (at most) 48-bit addresses which are 4k
/// page aligned. This includes:
///
/// 1) All device virtual addresses (the addresses we will translate).
/// 2) All translation table physical page addresses.
/// 3) All of the final translated PA/IPAs.
pub const VALID_ADDR_MASK: u64 = 0x0000_FFFF_FFFF_F000;

/// Regardless of the translation table entry type (Table, Block, or Page), the "valid" bit is
/// always bit zero.
pub const VALID_ENTRY_BIT: u64 = 0x1;

/// For levels `[0, 2]`, entries with bit one set are Table entries. When cleared, they are block
/// entries.
pub const TABLE_ENTRY_BIT: u64 = 0x2;

/// At level 3, all entries are Page entries and should have bit one set.
pub const PAGE_ENTRY_BIT: u64 = 0x2;

/// Bit `10`: Access Flag (`AF`). Must be set to `1` when hardware access flag updates are disabled.
pub const PAGE_ENTRY_ACCESS_FLAG_BIT: u64 = 1 << 10;

/// Bit `51`: Dirty Bit Modifier (`DBM`). Must be `0` when hardware dirty state updates are
/// disabled.
pub const PAGE_ENTRY_DBM_BIT: u64 = 1 << 51;

/// Returns `true` if `addr` is a 4 KiB-aligned 48-bit physical/IOVA address.
#[inline]
pub const fn is_valid_addr(addr: u64) -> bool {
    (addr & !VALID_ADDR_MASK) == 0
}

/// Returns `true` if descriptor `e` has its valid bit (`bit 0`) set.
#[inline]
pub const fn is_valid_entry(e: u64) -> bool {
    (e & VALID_ENTRY_BIT) != 0
}

/// Returns `true` if descriptor `e` has its table-descriptor bit (`bit 1`) set.
#[inline]
pub const fn is_table_entry(e: u64) -> bool {
    (e & TABLE_ENTRY_BIT) != 0
}

/// Returns `true` if descriptor `e` at `level` (`0..=2`) is a valid block descriptor.
#[inline]
pub const fn is_block_entry(e: u64, level: u32) -> bool {
    is_valid_entry(e) && (level < 3) && !is_table_entry(e)
}

/// Extracts the 4 KiB-aligned next-level table physical address from table descriptor `e`.
#[inline]
pub const fn get_table_entry_paddr(e: u64) -> u64 {
    e & VALID_ADDR_MASK
}

/// Creates a "Table" entry which points at the physical address of the next level of the
/// translation tables. Used in levels `[0, 2]` of the translation tables.
///
/// Note: The passed address must be valid, and we debug assert this, but we also unconditionally
/// mask the passed address just in case.
#[inline]
pub fn make_table_entry(addr: u64) -> u64 {
    debug_assert!(is_valid_addr(addr), "Bad Address {addr:#018x}");
    (addr & VALID_ADDR_MASK) | VALID_ENTRY_BIT | TABLE_ENTRY_BIT
}

/// Encodes the `UXN` (bit 54), `PXN` (bit 53), and `AP[2:1]` (bits `[7:6]`) permission fields for a
/// VMSAv8-64 stage-1 page descriptor.
#[inline]
pub const fn make_perms(uxn: u64, pxn: u64, ap21: u64) -> u64 {
    // See Section D8.3.1.2 Figure D8-16 of the ARM ARM.
    ((uxn & 0x1) << 54) | ((pxn & 0x1) << 53) | ((ap21 & 0x3) << 6)
}

/// Lookup table mapping `IOMMU_FLAG_PERM_{READ,WRITE,EXECUTE}` combinations (`0..8`) to VMSAv8-64
/// stage-1 page descriptor permission bits (`UXN`, `PXN`, `AP[2:1]`).
///
/// Permission mappings from basic RWX to Page Entry Attributes is, sadly, not a straightforward
/// exercise. There are many factors in play, not all of them are 100% under our control.
///
/// Effective permissions are determined by several fields encoded in the Page/Block entry, as well
/// as the WXN/UWXN bits encoded in the Context Bank's SCTLR register. These bits are:
///
/// + UXN (unprivileged execute never)
/// + PXN (privileged execute never)
/// + AP21 (the Access Permission bits)
/// + WXN (writeable execute never)
/// + UWXN (unprivileged execute never)
///
/// Additionally, accesses can come in two flavors: Privileged and Unprivileged. In the AP world,
/// an unprivileged access is one which comes from EL0, while a privileged access is one which
/// comes from EL1-3. In the SMMU world, it is not clear what initially configures the access
/// flavor, but it is most likely a decision that a system designer makes on a per-HW-unit basis.
///
/// Based on the permission bits in the Page/Block entry and the SCTLR register, along with whether
/// the access is privileged or unprivileged, the final effective permissions are determined. A
/// table showing the possible combinations can be found in the ARM ARM, Section D8.4.1.2.6 Table
/// D8-62.
///
/// The current SMMU driver attempts to do the following for user-created BTIs:
///
/// + Force all transactions to be considered unprivileged using the `S2CR.PRIVCFG` field in the
///   BTI's SMRG(s). Whether or not this override takes effect depends on static SMMU configuration
///   which can be read via `IDR2.DIPANS`.
/// + Disable WXN/UWXN via the context bank's SCTLR register.
///
/// The final set of possible mappings from `BTI_PERM` flags to effective permissions is given in
/// the LUT below. It is not possible to create a perfect mapping from flags to permissions, and
/// the issue is complicated by the different final effective permissions based on privileged vs.
/// unprivileged access. This said, it is _mostly_ correct, however there are a few notes to keep
/// in mind:
///
/// + It is not possible to revoke Read access from a privileged access. If there exists any valid
///   translation, it will always allow privileged accesses to read memory.
/// + Regardless of the access flavor, Write access implies Read access. IOW - it is not possible
///   to create Write-only memory for a device. We do our best to inform user-mode of this
///   limitation by rejecting requests to create write only mappings at a higher level of the
///   stack.
/// + It is not possible to allow both write and execute for privileged accesses, however it is for
///   unprivileged accesses.
///
/// See also ARM ARM Table D8-63:
/// - `AP[2:1] = 0b01` (`1`): Read/Write at EL0 and higher
/// - `AP[2:1] = 0b11` (`3`): Read-only at EL0 and higher
/// - `AP[2:1] = 0b10` (`2`): Read-only at EL1+, no access at EL0 (used when neither R nor W is set)
pub const PERM_LUT: [u64; 8] = [
    make_perms(1, 1, 2), // [0]: --- ==> pR-- : u---
    make_perms(1, 1, 3), // [1]: R-- ==> pR-- : uR--
    make_perms(1, 1, 1), // [2]: -W- ==> pRW- : uRW-
    make_perms(1, 1, 1), // [3]: RW- ==> pRW- : uRW-
    make_perms(0, 0, 2), // [4]: --X ==> pR-X : u--X
    make_perms(0, 0, 3), // [5]: R-X ==> pR-X : uR-X
    make_perms(0, 0, 1), // [6]: -WX ==> pRW- : uRWX
    make_perms(0, 0, 1), // [7]: RWX ==> pRW- : uRWX
];

/// Returns the VMSAv8-64 stage-1 page descriptor permission bits for `perms`.
#[inline]
pub const fn get_page_entry_perms(perms: u32) -> u64 {
    PERM_LUT[(perms & ALL_PERM_FLAGS) as usize]
}

/// Decodes the effective unprivileged (`EL0`) `IOMMU_FLAG_PERM_*` bits from a VMSAv8-64 stage-1
/// leaf page descriptor (`AP[2:1]` at bits `[7:6]` and `UXN` at bit `54`).
#[inline]
pub const fn decode_page_entry_perms(pte: u64) -> u32 {
    let ap21 = (pte >> 6) & 0x3;
    let uxn = (pte >> 54) & 0x1;
    let mut perms = 0u32;
    if ap21 == 1 {
        perms |= super::constants::IOMMU_FLAG_PERM_READ | super::constants::IOMMU_FLAG_PERM_WRITE;
    } else if ap21 == 3 {
        perms |= super::constants::IOMMU_FLAG_PERM_READ;
    }
    if uxn == 0 {
        perms |= super::constants::IOMMU_FLAG_PERM_EXECUTE;
    }
    perms
}

/// Creates a valid level 3 leaf page descriptor mapping to physical address `addr` with `perms`.
#[inline]
pub fn make_page_entry(addr: u64, perms: u32) -> u64 {
    // Note: We set the access flag bit to 1 from the start because we don't make
    // use of it, and we don't want TLB fills to be writing to the page
    // descriptors.
    debug_assert!(is_valid_addr(addr));
    (addr & VALID_ADDR_MASK)
        | VALID_ENTRY_BIT
        | PAGE_ENTRY_BIT
        | PAGE_ENTRY_ACCESS_FLAG_BIT
        | get_page_entry_perms(perms)
}

/// Atomically stores `val` into `slot` with `Relaxed` ordering, first reporting the store to the
/// host test hardware model (a no-op in the kernel).
///
/// `AtomicU64` with `Ordering::Relaxed` is used to perform a single-copy 64-bit store without
/// requiring `&mut` aliasing; visibility of the store to the SMMU hardware table walker is
/// ordered by per-page [`SmmuEnv::clean_dcache_range`] (`dc cvac` + `dsb sy`).
///
/// `level` is the level the caller believes the table is at. The model cross-checks it against
/// how the table is actually linked.
#[inline]
pub fn write_pte(slot: &AtomicU64, val: u64, level: u32) {
    // SAFETY: `slot` is a valid, 8-byte-aligned reference to a 64-bit PTE slot inside an allocated
    // translation table page.
    unsafe {
        SmmuEnv::observe_hw_visible_write(
            slot.as_ptr().expose_provenance(),
            HwVisibleWrite::Descriptor { level, new_val: val },
        );
    }
    slot.store(val, Ordering::Relaxed);
}

/// Zero-fills a whole translation table page, first reporting it to the host test hardware model
/// (a no-op in the kernel).
///
/// Every store into translation table memory goes through [`write_pte`] or this function, so
/// that the model observes all of them.
///
/// # Safety
///
/// - `table_vaddr` must be a non-null, 4 KiB page-aligned physmap virtual address with exposed
///   mutable provenance valid for reading and writing `4096` bytes (`512` initialized `u64` words)
///   of an allocated translation table page.
/// - No live references (including shared `&[AtomicU64; ENTRIES_PER_PAGE]` slices from
///   `translation_table::table_slice`) may alias the page for the duration of this call.
#[inline]
pub unsafe fn zero_table_page(table_vaddr: usize) {
    const TABLE_BYTES: usize = ENTRIES_PER_PAGE * core::mem::size_of::<u64>();
    debug_assert_ne!(table_vaddr, 0);
    debug_assert_eq!(table_vaddr & (super::constants::PAGE_MASK as usize), 0);
    // SAFETY: Guaranteed by the caller.
    unsafe {
        SmmuEnv::observe_hw_visible_write(
            table_vaddr,
            HwVisibleWrite::TableZeroFill { len: TABLE_BYTES },
        );
        core::ptr::write_bytes(
            core::ptr::with_exposed_provenance_mut::<u8>(table_vaddr),
            0,
            TABLE_BYTES,
        );
    }
}
