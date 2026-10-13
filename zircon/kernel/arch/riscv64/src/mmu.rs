// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! RISC-V 64 Memory Management Unit (MMU), Sv39 page tables, and TLB management.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::needless_late_init)]
#![allow(clippy::only_used_in_recursion)]

use super::asid_allocator::*;
use super::vm::is_kernel_address;
use super::{KERNEL_ASPACE_BASE, KERNEL_ASPACE_SIZE};
use crate::counters;
use crate::kernel::types::PAddr;
use crate::vm::arch_vm_aspace::*;
use crate::vm::page::VmPageDoublyLinkedList;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use debug::dprintf;
use ksync::lock;
use lazy_init::LazyInit;
use pin_init::stack_pin_init;
use riscv64_aspace_constants_bindings as aspace_constants;
use zx_status::Status;

use debug::ltracef;
const LOCAL_TRACE: u32 = 0;

counters::define_kcounter!(
    VM_MMU_PROTECT_MAKE_EXECUTE_PAGES,
    "vm.mmu.protect.make_execute_pages",
    Sum
);
counters::define_kcounter!(
    VM_MMU_PROTECT_MAKE_EXECUTE_CALLS,
    "vm.mmu.protect.make_execute_calls",
    Sum
);
counters::define_kcounter!(CM_FLUSH_CALL, "mmu.consistency_manager.flush_call", Sum);
counters::define_kcounter!(
    CM_LOCAL_PAGE_INVALIDATE,
    "mmu.consistency_manager.local_page_invalidate",
    Sum
);
counters::define_kcounter!(CM_ASID_INVALIDATE, "mmu.consistency_manager.asid_invalidate", Sum);
counters::define_kcounter!(CM_GLOBAL_INVALIDATE, "mmu.consistency_manager.global_invalidate", Sum);
counters::define_kcounter!(
    CM_PAGE_RUN_INVALIDATE,
    "mmu.consistency_manager.page_run_invalidate",
    Sum
);
counters::define_kcounter!(
    CM_SINGLE_PAGE_INVALIDATE,
    "mmu.consistency_manager.single_page_invalidate",
    Sum
);

// Architectural Page Table and Virtual Memory dimensions.
// This code is currently set up to handle Sv39 only.
const PAGE_MASK: usize = page::SIZE - 1;

const NUM_PAGE_TABLE_LEVELS: usize = 3;
const PAGE_TABLE_LEVEL_SHIFT: usize = 9;
const NUM_PAGE_TABLE_ENTRIES: usize = 1 << PAGE_TABLE_LEVEL_SHIFT; // 512 entries per page table

const VIRTUAL_ADDRESS_SIZE: usize = 39;
const VIRTUAL_ADDRESS_MASK: usize = (1usize << VIRTUAL_ADDRESS_SIZE) - 1;

// Constants to assist indexing into the top portion of the kernel top level page table.
const MMU_PT_KERNEL_BASE_INDEX: usize = NUM_PAGE_TABLE_ENTRIES / 2; // 256
const MMU_PT_KERNEL_ENTRIES: usize = NUM_PAGE_TABLE_ENTRIES / 2; // 256

// Page Table Entry (PTE) bits for RISC-V.
const RISCV64_PTE_V: u64 = 1 << 0; // Valid
const RISCV64_PTE_R: u64 = 1 << 1; // Read
const RISCV64_PTE_W: u64 = 1 << 2; // Write
const RISCV64_PTE_X: u64 = 1 << 3; // Execute
const RISCV64_PTE_PERM_MASK: u64 = 7 << 1; // Read | Write | Execute
const RISCV64_PTE_U: u64 = 1 << 4; // User
const RISCV64_PTE_G: u64 = 1 << 5; // Global
const RISCV64_PTE_A: u64 = 1 << 6; // Accessed
const RISCV64_PTE_D: u64 = 1 << 7; // Dirty
const RISCV64_PTE_RSW_MASK: u64 = 3 << 8; // Reserved for software
const RISCV64_PTE_PPN_SHIFT: usize = 10;
const RISCV64_PTE_PPN_BITS: usize = 56;
const RISCV64_PTE_PPN_MASK: u64 =
    ((1u64 << (RISCV64_PTE_PPN_BITS - PAGE_TABLE_LEVEL_SHIFT)) - 1) << RISCV64_PTE_PPN_SHIFT;

// Svpbmt (Page-Based Memory Types) extension bit definitions in PTE[62:61].
const RISCV64_PTE_PBMT_PMA: u64 = 0 << 61; // Normal memory (Power-on default / PMA)
const RISCV64_PTE_PBMT_NC: u64 = 1 << 61; // Non-cacheable, idempotent, weakly-ordered (main memory)
const RISCV64_PTE_PBMT_IO: u64 = 2 << 61; // Non-cacheable, non-idempotent, strongly-ordered (I/O)
const RISCV64_PTE_PBMT_MASK: u64 = 3 << 61;

// Architectural MMU flags defined in vm/arch_vm_aspace.h.

/// Validates whether `vaddr` is within `[base, base + size - 1]`.
#[inline(always)]
const fn is_valid_vaddr(base: usize, size: usize, vaddr: usize) -> bool {
    if size == 0 {
        return false;
    }
    vaddr >= base && vaddr <= base + (size - 1)
}

/// Last byte of the `count`-page run starting at `vaddr`, or `None` if the run
/// wraps around the top of the address space.  `count` must be non-zero.
#[inline(always)]
const fn page_run_end(vaddr: usize, count: usize) -> Option<usize> {
    match count.checked_mul(page::SIZE) {
        Some(len) => vaddr.checked_add(len - 1),
        None => None,
    }
}

type Pte = u64;

// SATP CSR bitfield layout and constants.
const SATP_MODE_BARE: u64 = 0;
const SATP_MODE_SV39: u64 = 8;
const SATP_MODE_SHIFT: usize = 60;
const SATP_ASID_SHIFT: usize = 44;
const SATP_ASID_MASK: u64 = 0xffff;
const SATP_PPN_MASK: u64 = (1u64 << 44) - 1;

/// SATP CSR wrapper representation.
///
/// Deliberately not `libarch::riscv64::paging::SATP`, although the field layout
/// is identical.  Its `mode` is typed `TranslationMode`, whose getter is
/// `try_mode().unwrap()`, so reading back a reserved encoding (1-7, 12-15)
/// panics -- and `riscv64_mmu_early_init()` below reads SATP back right after
/// deliberately writing an out-of-range ASID to discover the implemented width,
/// on the earliest MMU path, where a panic prints nothing at all.  Its
/// `set_root_address()` also asserts 4KiB alignment where `from_fields()` masks,
/// which would turn a benign misalignment into a dead machine on the context
/// switch path.  `sstatus` has neither problem and does use libarch, in
/// `fpu.rs` and `vector.rs`.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct Satp(u64);

impl Satp {
    #[inline(always)]
    const fn new(val: u64) -> Self {
        Self(val)
    }

    #[inline(always)]
    const fn from_fields(mode: u64, asid: u16, root_paddr: u64) -> Self {
        Self(
            (mode << SATP_MODE_SHIFT)
                | ((asid as u64) << SATP_ASID_SHIFT)
                | ((root_paddr >> page::SHIFT) & SATP_PPN_MASK),
        )
    }

    #[inline(always)]
    fn read() -> Self {
        // SAFETY: reading `satp` has no side effects.
        let val = unsafe { super::arch::riscv64_csr_read::<{ super::arch::RISCV64_CSR_SATP }>() };
        Self(val)
    }

    #[inline(always)]
    fn write(self) {
        // SAFETY: writing `satp` installs a root page table and ASID. Callers build the
        // value with `Satp::from_fields` from a table that is already fully populated.
        unsafe { super::arch::riscv64_csr_write::<{ super::arch::RISCV64_CSR_SATP }>(self.0) };
    }

    #[inline(always)]
    const fn mode(self) -> u64 {
        (self.0 >> SATP_MODE_SHIFT) & 0xf
    }

    #[inline(always)]
    const fn asid(self) -> u16 {
        ((self.0 >> SATP_ASID_SHIFT) & SATP_ASID_MASK) as u16
    }

    #[inline(always)]
    const fn root_address(self) -> u64 {
        (self.0 & SATP_PPN_MASK) << page::SHIFT
    }
}

// Global kernel and bootstrap translation tables.
#[derive(Clone, Copy)]
#[repr(C, align(4096))]
struct PageTable([Pte; NUM_PAGE_TABLE_ENTRIES]);

/// The main translation table for the kernel.  Used by the one kernel address space
/// when kernel only threads are active.  **NOTE:** `KernelPhysicalAddressOf` requires
/// that this be annotated into `.data` so it is sure to be contiguous in the image.
#[unsafe(link_section = ".data")]
static mut KERNEL_TRANSLATION_TABLE: PageTable = PageTable([0; NUM_PAGE_TABLE_ENTRIES]);

/// Physical address of `KERNEL_TRANSLATION_TABLE`.
///
/// Written once on the boot CPU in `riscv64_mmu_early_init`, before any secondary
/// hart is started, and only read afterwards; hart startup provides the ordering
/// that publishes it.
static KERNEL_TRANSLATION_TABLE_PHYS: LazyInit<u64> = LazyInit::uninit();

/// A copy of the bootstrap translation table with user space identity mapped, used
/// when booting secondary cores.  **NOTE:** `KernelPhysicalAddressOf` requires that
/// this be annotated into `.data` so it is sure to be contiguous in the image.
#[unsafe(link_section = ".data")]
static mut BOOTSTRAP_TRANSLATION_TABLE: PageTable = PageTable([0; NUM_PAGE_TABLE_ENTRIES]);

/// Implemented ASID width, discovered by writing all ones to `satp.asid` and
/// reading back which bits stuck. Written once during early MMU init, like
/// [`KERNEL_TRANSLATION_TABLE_PHYS`].
static RISCV_ASID_MASK: LazyInit<u64> = LazyInit::uninit();

/// Whether ASIDs are used at all: requires a full 16-bit ASID field and the
/// `riscv64_enable_asid` boot option. Written once during early MMU init.
static RISCV_USE_ASID: LazyInit<bool> = LazyInit::uninit();

/// Computes the page table index for a virtual address at a given level.
#[inline(always)]
const fn vaddr_to_index(va: usize, level: usize) -> usize {
    // levels count down from NUM_PAGE_TABLE_LEVELS - 1
    debug_assert!(level < NUM_PAGE_TABLE_LEVELS);
    let canonical_va = va & VIRTUAL_ADDRESS_MASK;
    ((canonical_va >> page::SHIFT) >> (level * PAGE_TABLE_LEVEL_SHIFT))
        & (NUM_PAGE_TABLE_ENTRIES - 1)
}

/// Returns the size in bytes mapped by a page table entry at the given level.
#[inline(always)]
const fn page_size_per_level(level: usize) -> usize {
    // levels count down from NUM_PAGE_TABLE_LEVELS - 1
    debug_assert!(level < NUM_PAGE_TABLE_LEVELS);
    1usize << (page::SHIFT + level * PAGE_TABLE_LEVEL_SHIFT)
}

/// Returns the address mask for a page table entry at the given level.
#[inline(always)]
const fn page_mask_per_level(level: usize) -> usize {
    page_size_per_level(level) - 1
}

// Helper routines for various page table entry manipulation.

/// Tests whether a page table entry is valid (V bit set).
#[inline(always)]
const fn pte_is_valid(pte: Pte) -> bool {
    (pte & RISCV64_PTE_V) != 0
}

/// Tests whether a page table entry is a leaf entry (has R, W, or X permissions).
#[inline(always)]
const fn pte_is_leaf(pte: Pte) -> bool {
    (pte & RISCV64_PTE_PERM_MASK) != 0
}

/// Extracts the physical address from a page table entry.
#[inline(always)]
const fn pte_paddr(pte: Pte) -> u64 {
    // riscv PPN is stored shifted over 2 from the natural alignment
    (pte & RISCV64_PTE_PPN_MASK) << (page::SHIFT - RISCV64_PTE_PPN_SHIFT)
}

/// Encodes a physical address into the PPN field of a page table entry.
#[inline(always)]
const fn paddr_to_pte(pa: u64) -> Pte {
    (pa >> page::SHIFT) << RISCV64_PTE_PPN_SHIFT
}

/// Constructs a non-leaf page table entry pointing to a child page table at physical address `pa`.
///
/// For all inner page tables for the entire kernel hierarchy, set the global bit.
#[inline(always)]
const fn mmu_non_leaf_pte(pa: u64, global: bool) -> Pte {
    paddr_to_pte(pa) | if global { RISCV64_PTE_G } else { 0 } | RISCV64_PTE_V
}

/// Converts high-level architectural MMU flags to low-level leaf descriptor attributes.
fn mmu_flags_to_pte_attr(flags: u32, global: bool) -> Pte {
    let mut attr = RISCV64_PTE_V | RISCV64_PTE_A | RISCV64_PTE_D;
    if (flags & (ARCH_MMU_FLAG_PERM_USER as u32)) != 0 {
        attr |= RISCV64_PTE_U;
    }
    if (flags & (ARCH_MMU_FLAG_PERM_READ as u32)) != 0 {
        attr |= RISCV64_PTE_R;
    }
    if (flags & (ARCH_MMU_FLAG_PERM_WRITE as u32)) != 0 {
        attr |= RISCV64_PTE_W;
    }
    if (flags & (ARCH_MMU_FLAG_PERM_EXECUTE as u32)) != 0 {
        attr |= RISCV64_PTE_X;
    }
    if global {
        attr |= RISCV64_PTE_G;
    }

    if super::feature::has_svpbmt() {
        match (flags & (ARCH_MMU_FLAG_CACHE_MASK as u32)) as u8 {
            ARCH_MMU_FLAG_CACHED => attr |= RISCV64_PTE_PBMT_PMA,
            ARCH_MMU_FLAG_UNCACHED | ARCH_MMU_FLAG_WRITE_COMBINING => attr |= RISCV64_PTE_PBMT_NC,
            ARCH_MMU_FLAG_UNCACHED_DEVICE => attr |= RISCV64_PTE_PBMT_IO,
            _ => {}
        }
    }

    attr
}

/// Converts low-level leaf descriptor attributes to high-level architectural MMU flags.
fn mmu_flags_from_pte(pte: Pte) -> u8 {
    let mut flags = 0u8;
    if (pte & RISCV64_PTE_U) != 0 {
        flags |= ARCH_MMU_FLAG_PERM_USER;
    }
    if (pte & RISCV64_PTE_R) != 0 {
        flags |= ARCH_MMU_FLAG_PERM_READ;
    }
    if (pte & RISCV64_PTE_W) != 0 {
        flags |= ARCH_MMU_FLAG_PERM_WRITE;
    }
    if (pte & RISCV64_PTE_X) != 0 {
        flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
    }

    if super::feature::has_svpbmt() {
        match pte & RISCV64_PTE_PBMT_MASK {
            // PMA state basically means default cache parameters, as determined by
            // physical address.  Don't actually report it as CACHED here since we
            // can't know here what the actual underlying physical range's type is.
            RISCV64_PTE_PBMT_PMA => {}
            RISCV64_PTE_PBMT_NC => flags |= ARCH_MMU_FLAG_UNCACHED,
            RISCV64_PTE_PBMT_IO => flags |= ARCH_MMU_FLAG_UNCACHED_DEVICE,
            _ => panic!("unexpected pte value {pte:#x}"),
        }
    }

    flags
}

/// Validates that a user address space base and size are page-aligned and entirely within the user half of Sv39.
const fn is_user_base_size_valid(base: usize, size: usize) -> bool {
    if size == 0 {
        return false;
    }
    if (base & PAGE_MASK) != 0 || (size & PAGE_MASK) != 0 {
        return false;
    }
    if (base & super::vm::RISCV64_CANONICAL_ADDRESS_MASK) != 0 {
        return false;
    }
    match base.checked_add(size) {
        Some(top) => ((top - 1) & super::vm::RISCV64_CANONICAL_ADDRESS_MASK) == 0,
        None => false,
    }
}

/// Zeroes an entire page of memory at `ptr` using hardware Zicboz cache instructions if available,
/// or an optimized unrolled 64-byte scalar store loop.
///
/// # Safety
/// `ptr` must point to a valid, page-aligned block of at least `page::SIZE` writable bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_zero_page(ptr: *mut core::ffi::c_void) {
    let ptr = ptr as *mut u8;
    let end_address = (ptr as usize) + page::SIZE;

    if super::feature::has_zicboz() {
        let cboz_size = super::feature::cboz_size() as usize;
        let mut curr = ptr as usize;
        while curr < end_address {
            // SAFETY: `cbo.zero` zeroes the cache block holding `curr`, and the loop keeps
            // `curr` inside the page `ptr` points at. Zicboz support was checked above.
            unsafe {
                core::arch::asm!(
                    "cbo.zero 0({curr})",
                    curr = in(reg) curr,
                    options(nostack, preserves_flags),
                );
            }
            curr += cboz_size;
        }
    } else {
        let mut curr = ptr as usize;
        while curr < end_address {
            // SAFETY: the stores cover 64 bytes from `curr`, which the loop keeps inside the
            // page `ptr` points at.
            unsafe {
                core::arch::asm!(
                    "sd zero, 0({curr})",
                    "sd zero, 8({curr})",
                    "sd zero, 16({curr})",
                    "sd zero, 24({curr})",
                    "sd zero, 32({curr})",
                    "sd zero, 40({curr})",
                    "sd zero, 48({curr})",
                    "sd zero, 56({curr})",
                    curr = in(reg) curr,
                    options(nostack, preserves_flags),
                );
            }
            curr += 64;
        }
    }
}

/// Query hardware address tagging capabilities. Returns 0 for RISC-V 64.
#[unsafe(no_mangle)]
pub extern "C" fn arch_address_tagging_features() -> u32 {
    0
}

// TLB invalidation primitives (sfence.vma).

/// Flushes the entire TLB completely on the local CPU, including global mappings.
#[inline(always)]
fn riscv64_tlb_flush_all() {
    // SAFETY: sfence.vma zero, zero flushes all TLB entries for all ASIDs.
    unsafe {
        core::arch::asm!("sfence.vma zero, zero", options(nostack, preserves_flags));
    }
}

/// Flushes all non-global TLB entries for the specified `asid` on the local CPU.
#[inline(always)]
fn riscv64_tlb_flush_asid(asid: u16) {
    let asid_val = asid as u64;
    // SAFETY: sfence.vma zero, asid flushes all non-global translations for the specified ASID.
    unsafe {
        core::arch::asm!(
            "sfence.vma zero, {asid}",
            asid = in(reg) asid_val,
            options(nostack, preserves_flags),
        );
    }
}

/// Flushes all translations for `va` across all ASIDs, including global mappings, on the local CPU.
#[inline(always)]
fn riscv64_tlb_flush_address_all_asids(va: usize) {
    // SAFETY: sfence.vma va, zero flushes matching translations across all ASIDs.
    unsafe {
        core::arch::asm!(
            "sfence.vma {va}, zero",
            va = in(reg) va,
            options(nostack, preserves_flags),
        );
    }
}

/// Flushes all non-global translations for `va` in `asid` on the local CPU.
#[inline(always)]
fn riscv64_tlb_flush_address_one_asid(va: usize, asid: u16) {
    let asid_val = asid as u64;
    // SAFETY: sfence.vma va, asid flushes translations matching va for the specified ASID.
    unsafe {
        core::arch::asm!(
            "sfence.vma {va}, {asid}",
            va = in(reg) va,
            asid = in(reg) asid_val,
            options(nostack, preserves_flags),
        );
    }
}

/// Reads the current ASID configured in the SATP CSR on the local CPU.
#[inline(always)]
fn riscv64_current_asid() -> u16 {
    Satp::read().asid()
}

// C FFI imports

unsafe extern "C" {
    fn cpp_kernel_physical_address_of(va: usize) -> u64;
    static gPhysmapBase: usize;
    static gPhysmapSize: usize;
}

/// Marks `paddr`'s page accessed, returning whether it has a `vm_page_t` at all.
///
/// Mappings for physical VMOs do not, and so have no accessed state to update --
/// or to harvest.
fn mark_page_accessed(paddr: u64) -> bool {
    match crate::vm::pmm::paddr_to_vm_page(PAddr(paddr as usize)) {
        Some(page) => {
            // SAFETY: `page` is a valid `vm_page_t` obtained from `paddr_to_vm_page`.
            unsafe { crate::vm::pmm::page_queues().mark_accessed(page) };
            true
        }
        None => false,
    }
}

/// Frees one page table page back to the PMM.
fn free_page_table_page(paddr: u64) {
    let page = crate::vm::pmm::paddr_to_vm_page(PAddr(paddr as usize));
    debug_assert!(page.is_some());
    if let Some(page) = page {
        // SAFETY: page table pages come from `alloc_page_table()` and are freed once.
        unsafe { crate::vm::pmm::free_page(page) };
    }
}

#[inline(always)]
fn paddr_to_physmap(pa: u64) -> *mut u8 {
    // SAFETY: `gPhysmapBase` is a C++ global fixed before any of this code runs, and
    // the arithmetic only forms an address without accessing memory.
    unsafe { (gPhysmapBase + (pa as usize)) as *mut u8 }
}

#[inline(always)]
fn physmap_to_paddr(ptr: *const u8) -> u64 {
    // SAFETY: `gPhysmapBase` is a C++ global fixed before any of this code runs, and
    // the arithmetic only forms an address without accessing memory.
    unsafe { (ptr as usize - gPhysmapBase) as u64 }
}

/// Argument to SfenceVma.  Used to perform TLB invalidation on an optional range
/// with an optional ASID.  When no range is present, the target is all addresses.
/// When no ASID is present the target is invalidated for all ASIDs.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct SfenceVmaArgs {
    pub range_base: usize,
    pub range_size: usize,
    pub has_range: bool,
    pub asid: u16,
    pub has_asid: bool,
    pub unified_asid: u16,
    pub has_unified_asid: bool,
}

/// Issues a sequence of sfence.vma instructions as specified by SfenceVmaArgs.
fn sfence_vma(args: &SfenceVmaArgs) {
    debug_assert!(super::arch::arch_ints_disabled());
    if args.has_range {
        let base = args.range_base;
        let end = base + args.range_size;
        if args.has_asid {
            let asid = args.asid;
            let mut va = base;
            while va < end {
                riscv64_tlb_flush_address_one_asid(va, asid);
                // If a unified ASID was provided, then this is a restricted aspace, so
                // flush the same address in the unified ASID as well.
                if args.has_unified_asid {
                    riscv64_tlb_flush_address_one_asid(va, args.unified_asid);
                }
                va += page::SIZE;
            }
        } else {
            let mut va = base;
            while va < end {
                riscv64_tlb_flush_address_all_asids(va);
                va += page::SIZE;
            }
        }
    } else {
        if args.has_asid {
            // All addresses, one ASID.
            riscv64_tlb_flush_asid(args.asid);
            if args.has_unified_asid {
                riscv64_tlb_flush_asid(args.unified_asid);
            }
        } else {
            riscv64_tlb_flush_all();
        }
    }
}

/// Maximum number of TLB runs queued before switching to full ASID invalidation.
const MAX_PENDING_TLB_RUNS: usize = 8;

/// A contiguous run of virtual addresses to flush.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct PendingTlbRun {
    pub va: usize,
    pub count: usize,
}

/// State tracking for delayed/coalesced TLB flushes and page freeing.
#[derive(Debug, PartialEq, Eq)]
struct ConsistencyManagerTracker {
    /// Perform a full flush of the entire ASID (or all ASIDs if a kernel aspace) in
    /// these cases:
    /// 1) We've accumulated more than `MAX_PENDING_TLB_RUNS` runs of pages, which are
    ///    expensive to perform because of cross cpu TLB shootdowns.
    /// 2) We've been asked to flush a non terminal page, which according to the RISC-V
    ///    privileged spec should involve clearing the entire ASID.
    pub full_flush: bool,
    pub num_pending_tlb_runs: usize,
    /// Pending TLBs to flush, stored as a virtual address plus a count of pages to
    /// flush in a run.
    pub pending_tlbs: [PendingTlbRun; MAX_PENDING_TLB_RUNS],
}

impl ConsistencyManagerTracker {
    pub const fn new() -> Self {
        Self {
            full_flush: false,
            num_pending_tlb_runs: 0,
            pending_tlbs: [PendingTlbRun { va: 0, count: 0 }; MAX_PENDING_TLB_RUNS],
        }
    }

    /// Queue a TLB entry for flushing.  This may get turned into a complete ASID flush.
    ///
    /// Returns true if the caller (in kernel mode) needs to flush immediately due to a
    /// full queue.
    pub fn flush_entry(&mut self, va: usize, flush: Flush, is_kernel: bool) -> bool {
        debug_assert!((va & (page::SIZE - 1)) == 0);
        // If we've already decided to do a full flush, nothing more to track here.
        if self.full_flush {
            return false;
        }
        // If we're asked to flush a non terminal entry, we're going to need to dump the
        // entire ASID, so skip tracking this VA and exit now.
        if flush == Flush::NonTerminal {
            self.full_flush = true;
            return false;
        }
        // Check whether we have queued too many entries already.
        if self.num_pending_tlb_runs >= MAX_PENDING_TLB_RUNS {
            // Most of the time we will now prefer to invalidate the entire ASID; the
            // exception is if this aspace is for the kernel, in which case all pages are
            // global and we need to flush them one at a time.
            if !is_kernel {
                self.full_flush = true;
                return false;
            }
            // Kernel case: tell the caller to flush what we've cached up until now and
            // reset the counter to zero.
            return true;
        }
        if self.num_pending_tlb_runs > 0 {
            let last_idx = self.num_pending_tlb_runs - 1;
            let last_run = &mut self.pending_tlbs[last_idx];
            // See if this entry completes the previous run or is the start of the previous
            // run.  The latter catches a fairly common case of multiple flushes of the same
            // page in a row.
            if last_run.va + last_run.count * page::SIZE == va {
                last_run.count += 1;
                return false;
            }
            if last_run.va == va {
                return false;
            }
        }
        // Start a new run of entries to track.
        self.pending_tlbs[self.num_pending_tlb_runs] = PendingTlbRun { va, count: 1 };
        self.num_pending_tlb_runs += 1;
        false
    }

    pub fn reset(&mut self) {
        self.num_pending_tlb_runs = 0;
        self.full_flush = false;
    }
}

/// Which address spaces a TLB invalidation has to cover.
///
/// Kernel and shared mappings are present in every address space, so they must be
/// invalidated without an ASID; anything else is invalidated in its own, plus the
/// unified ASID that maps it when there is one.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
struct FlushTarget {
    pub is_kernel: bool,
    pub is_shared: bool,
    pub asid: u16,
    pub unified_asid: u16,
    pub has_unified_asid: bool,
}

impl FlushTarget {
    /// True if the mapping is not confined to a single ASID.
    fn is_global(&self) -> bool {
        self.is_kernel || self.is_shared
    }
}

/// Whether a flushed entry is a leaf mapping or an interior page table entry.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Flush {
    /// A leaf mapping, which can be invalidated one page run at a time.
    Terminal,
    /// An interior entry.  Accessed information is not tracked on inner nodes, so
    /// this forces a full invalidation rather than being queued.
    NonTerminal,
}

/// Use the appropriate TLB flush instruction to globally flush the modified run of pages
/// across all CPUs.
///
/// Future optimization here and in `flush_asid()` when asids are disabled: based on which
/// cpu has the aspace active, only send IPIs (either directly or via SBI) to the cores from
/// that list to shoot down TLBs.
fn flush_tlb_entry_run(vaddr: usize, count: usize, target: &FlushTarget) {
    CM_PAGE_RUN_INVALIDATE.add(1);
    CM_SINGLE_PAGE_INVALIDATE.add(count as i64);

    // Kernel and shared mappings are present in every address space, so they must
    // be invalidated without an ASID; anything else only in its own.
    let range = SfenceVmaArgs {
        range_base: vaddr,
        range_size: count * page::SIZE,
        has_range: true,
        ..Default::default()
    };
    let args = if target.is_global() {
        range
    } else {
        SfenceVmaArgs {
            asid: target.asid,
            has_asid: true,
            unified_asid: target.unified_asid,
            has_unified_asid: target.has_unified_asid,
            ..range
        }
    };
    mp_sync_sfence(&args);
}

/// Executes `sfence.vma` for `args` on every CPU.
fn mp_sync_sfence(args: &SfenceVmaArgs) {
    crate::kernel::mp::sync_exec(crate::kernel::mp::MpIpiTarget::All, 0, || sfence_vma(args));
}

/// Flush an entire ASID on all CPUs.
fn flush_asid(target: &FlushTarget) {
    if target.is_global() {
        // Perform a full flush of all cpus across all ASIDs.
        mp_sync_sfence(&SfenceVmaArgs::default());
        CM_GLOBAL_INVALIDATE.add(1);
    } else {
        // Perform a full flush of all cpus of a single ASID.
        mp_sync_sfence(&SfenceVmaArgs {
            asid: target.asid,
            has_asid: true,
            unified_asid: target.unified_asid,
            has_unified_asid: target.has_unified_asid,
            ..Default::default()
        });
        CM_ASID_INVALIDATE.add(1);
    }
}

/// A consistency manager that tracks TLB updates, walker syncs and free pages in an
/// effort to minimize MBs (by delaying and coalescing TLB invalidations) and switching
/// to full ASID invalidations if too many TLB invalidations are requested.
///
/// The aspace lock *must* be held over the full operation of the ConsistencyManager,
/// from construction to deletion.  The lock must be held continuously to deletion, and
/// specifically till the actual TLB invalidations occur, due to the strategy employed
/// here of only invalidating actual vaddrs with changing entries, and not all vaddrs an
/// operation applies to.  Otherwise the following scenario is possible:
///  1. Thread 1 performs an Unmap and removes PTE entries, but drops the lock prior to
///     invalidation.
///  2. Thread 2 performs an Unmap, no PTE entries are removed, no invalidations occur.
///  3. Thread 2 now believes the resources (pages) for the region are no longer
///     accessible, and returns them to the pmm.
///  4. Thread 3 attempts to access this region and is now able to read/write to returned
///     pages as invalidations have not occurred.
///
/// This scenario is possible as the mappings here are not the source of truth of resource
/// management, but a cache of information from other parts of the system.  If thread 2
/// wanted to guarantee that the pages were free it could issue its own TLB invalidations
/// for the vaddr range, even though it found no entries.  However this is not the strategy
/// employed here at the moment.
///
/// That lock is `Riscv64ArchVmAspace::mutex`, and the fields it protects carry
/// `#[guarded_by(mutex)]`, so the compiler enforces what the C++ expressed with
/// `TA_REQ(lock_)` / `TA_GUARDED(lock_)`.
#[pin_init::pin_data(PinnedDrop)]
struct ConsistencyManager<'a> {
    tracker: ConsistencyManagerTracker,
    /// Page table pages to release to the PMM after the TLB invalidation occurs,
    /// threaded through the pages themselves as the C++ `VmPageDoublyLinkedList`
    /// does.
    ///
    /// This deliberately is not an array.  A `ConsistencyManager` is a local of
    /// the map/unmap/protect/harvest entry points, so its frame stays live across
    /// the cross-CPU TLB shootdown in `flush()`; an inline array would put its
    /// whole capacity on an 8 KiB kernel stack for that whole time.  A list head
    /// is one pointer, and -- like the C++ -- has no capacity limit.
    #[pin]
    to_free: VmPageDoublyLinkedList,
    aspace: &'a Riscv64ArchVmAspace,
    /// The address space every invalidation issued through this manager applies
    /// to, fixed at construction -- the C++ holds a reference to its aspace for
    /// the same reason.
    target: FlushTarget,
}

impl<'a> ConsistencyManager<'a> {
    fn new(aspace: &'a LockedAspace<'_, '_>) -> impl pin_init::PinInit<Self> {
        let target = aspace.flush_target();
        let aspace_ref = aspace.aspace;
        pin_init::pin_init!(Self {
            tracker: ConsistencyManagerTracker::new(),
            to_free <- VmPageDoublyLinkedList::new(),
            aspace: aspace_ref,
            target,
        })
    }

    fn flush_entry(mut self: core::pin::Pin<&mut Self>, va: usize, flush: Flush) {
        ltracef!(
            "va {va:#x}, asid {:#x}, terminal {}\n",
            self.target.asid,
            flush == Flush::Terminal
        );
        debug_assert!((va & PAGE_MASK) == 0);
        debug_assert!(self.aspace.size == 0 || self.aspace.is_valid_vaddr(va));
        let is_kernel = self.target.is_kernel;
        if self.as_mut().project().tracker.flush_entry(va, flush, is_kernel) {
            self.as_mut().flush();
            let full = self.as_mut().project().tracker.flush_entry(va, flush, is_kernel);
            debug_assert!(!full);
        }
    }

    /// Perform any pending synchronization of TLBs and page table walkers.  Includes
    /// the MB to ensure TLB flushes have completed prior to returning to user.
    fn flush(self: core::pin::Pin<&mut Self>) {
        CM_FLUSH_CALL.add(1);
        let this = self.project();
        if !this.tracker.full_flush && this.tracker.num_pending_tlb_runs == 0 {
            return;
        }

        // Need a mb to synchronize any page table updates prior to flushing the TLBs.
        mb();

        // Check if we should just be performing a full ASID invalidation.
        if this.tracker.full_flush {
            // If this is a restricted aspace, `flush_asid` will also flush the associated
            // unified aspace's ASID.
            flush_asid(this.target);
        } else {
            for i in 0..this.tracker.num_pending_tlb_runs {
                let run = this.tracker.pending_tlbs[i];
                // If this is a restricted aspace, `flush_tlb_entry_run` will also flush the
                // given range in the associated unified aspace.
                flush_tlb_entry_run(run.va, run.count, this.target);
            }
        }

        // mb to ensure TLB flushes happen prior to returning to user.
        mb();
        this.tracker.reset();
    }

    /// Queue a page for freeing that is dependent on TLB flushing.  This is for pages
    /// that were previously installed as page tables; they should not be reused until
    /// the non-terminal TLB flush has occurred.
    fn free_page_table(self: core::pin::Pin<&mut Self>, vaddr: *mut Pte, paddr: u64) {
        ltracef!("vaddr {:p} paddr {paddr:#x}\n", vaddr);
        let page = crate::vm::pmm::paddr_to_vm_page(PAddr(paddr as usize));
        debug_assert!(page.is_some());
        let Some(page) = page else {
            return;
        };
        debug_assert_eq!(
            page.state(),
            crate::vm::page_state::VmPageState(crate::vm::page_state::bindings::vm_page_state::MMU)
        );
        let this = self.project();
        // SAFETY: `paddr` names a page table page from `alloc_page_table()` that has
        // just been unlinked from its parent table, so it is in no other container.
        unsafe { this.to_free.push_back_raw(page.as_non_null()) };
        this.aspace.pt_pages.fetch_sub(1, Ordering::Relaxed);
    }
}

#[pin_init::pinned_drop]
impl pin_init::PinnedDrop for ConsistencyManager<'_> {
    fn drop(mut self: core::pin::Pin<&mut Self>) {
        self.as_mut().flush();
        if !self.to_free.is_empty() {
            // Handed over as a list so the PMM lock is taken once, as the C++ does.
            // SAFETY: every queued page is a page table page allocated by
            // `alloc_page_table()`, unlinked from its parent table, and freed once.
            unsafe { crate::vm::pmm::free_list(self.project().to_free) };
        }
    }
}

/// Populates a new child page table by splitting an existing large page entry.
fn populate_split_page_table(
    new_table: &mut [Pte; NUM_PAGE_TABLE_ENTRIES],
    old_pte: Pte,
    parent_level: usize,
) {
    debug_assert!(parent_level > 0);
    debug_assert!(pte_is_leaf(old_pte));
    let next_size = page_size_per_level(parent_level - 1) as u64;
    // Inherit all of the page table entry bits that aren't part of the address.
    let new_attrs = old_pte & !RISCV64_PTE_PPN_MASK;
    let mut mapped_pa = pte_paddr(old_pte);
    for entry in new_table.iter_mut() {
        // directly write to the pte, no need to update since this is a completely new table
        *entry = paddr_to_pte(mapped_pa) | new_attrs;
        mapped_pa += next_size;
    }
}

/// Queries a virtual address in the given root page table, returning the translated physical address and MMU flags.
fn query_page_table(
    root: *const Pte,
    base: usize,
    size: usize,
    vaddr: usize,
) -> Result<(u64, u8), Status> {
    debug_assert!(!root.is_null());
    if root.is_null() {
        return Err(Status::BAD_STATE);
    }
    debug_assert!(size == 0 || is_valid_vaddr(base, size, vaddr));
    if size > 0 && !is_valid_vaddr(base, size, vaddr) {
        return Err(Status::OUT_OF_RANGE);
    }
    let mut level = NUM_PAGE_TABLE_LEVELS - 1;
    let mut page_table = root;

    loop {
        let index = vaddr_to_index(vaddr, level);
        // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
        // read stays inside the 512-entry table `page_table` points at.
        let pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };
        if !pte_is_valid(pte) {
            return Err(Status::NOT_FOUND);
        }
        if pte_is_leaf(pte) {
            let page_size = page_size_per_level(level);
            let page_mask = page_size - 1;
            let offset = (vaddr & page_mask) as u64;
            let base_pa = pte_paddr(pte);
            let mmu_flags = mmu_flags_from_pte(pte);
            return Ok((base_pa + offset, mmu_flags));
        }
        if level == 0 {
            return Err(Status::NOT_FOUND);
        }
        level -= 1;
        let next_paddr = pte_paddr(pte);
        page_table = paddr_to_physmap(next_paddr) as *const Pte;
    }
}

/// Returns the ASID to be assigned to the kernel address space.
#[inline(always)]
fn kernel_asid() -> u16 {
    // When using ASIDs, the kernel is assigned KERNEL_ASID (1) instead of UNUSED_ASID (0)
    // for two reasons:
    // a) To keep it logically separate from UNUSED_ASID for debug and assert reasons.
    // b) A note in SiFive documentation for various cores that says
    //   "Supervisor software that uses ASIDs should use a nonzero ASID value to refer to the
    //   same address space across all harts in the supervisor execution environment (SEE) and
    //   should not use an ASID value of 0. If supervisor software does not use ASIDs, then the
    //   ASID field in the satp CSR should be set to 0."
    // Unclear if this is simply a suggestion or hardware will perform some sort of optimization
    // based on this.
    if *RISCV_USE_ASID { MMU_RISCV64_KERNEL_ASID } else { MMU_RISCV64_UNUSED_ASID }
}

/// Load the kernel page tables and set the passed in asid.
fn riscv64_switch_kernel_asid(asid: u16) {
    let kernel_phys = *KERNEL_TRANSLATION_TABLE_PHYS;
    let satp = Satp::from_fields(SATP_MODE_SV39, asid, kernel_phys);
    satp.write();
    riscv64_tlb_flush_all();
}

/// Early architecture MMU initialization for the boot CPU.
pub(crate) fn riscv64_mmu_early_init() {
    let satp = Satp::read();
    // Figure out the number of supported ASID bits by writing all 1s to the asid field
    // in satp and seeing which ones 'stick'.
    let satp_test = Satp::new(satp.0 | (0xffffu64 << SATP_ASID_SHIFT));
    satp_test.write();
    let satp_read = Satp::read();
    let asid_mask = satp_read.asid() as u64;
    // Put the old value back.
    satp.write();

    // Use asids if hardware has full 16 bit support and our command line switches allow.
    let enable_asid_opt = boot_options::BootOptions::get().riscv64_enable_asid;
    let use_asid = enable_asid_opt && (asid_mask == 0xffff);
    // SAFETY: this runs once on the boot CPU before any secondary hart exists and
    // before anything reads these.
    unsafe {
        RISCV_ASID_MASK.init(asid_mask);
        RISCV_USE_ASID.init(use_asid);
    }
    asid_allocator_init();

    // Save a copy of the bootstrap translation table as passed to us by physboot.
    // This will later be used to bootstrap new secondary cpus.
    let boot_translation_table_phys = satp.root_address();
    let boot_translation_table = paddr_to_physmap(boot_translation_table_phys) as *const Pte;

    // SAFETY: `boot_translation_table` is the root table physboot installed, reached
    // through the physmap, and both destinations are kernel-lifetime statics of exactly
    // NUM_PAGE_TABLE_ENTRIES entries. Neither overlaps the source, and this runs on the
    // boot CPU before any secondary hart exists.
    unsafe {
        // Copy boot page table to bootstrap translation table.
        core::ptr::copy_nonoverlapping(
            boot_translation_table,
            core::ptr::addr_of_mut!(BOOTSTRAP_TRANSLATION_TABLE.0) as *mut Pte,
            NUM_PAGE_TABLE_ENTRIES,
        );

        // Copy kernel half of boot table to kernel translation table (user half remains zero).
        core::ptr::copy_nonoverlapping(
            boot_translation_table.add(MMU_PT_KERNEL_BASE_INDEX),
            (core::ptr::addr_of_mut!(KERNEL_TRANSLATION_TABLE.0) as *mut Pte)
                .add(MMU_PT_KERNEL_BASE_INDEX),
            MMU_PT_KERNEL_ENTRIES,
        );

        KERNEL_TRANSLATION_TABLE_PHYS.init(cpp_kernel_physical_address_of(core::ptr::addr_of!(
            KERNEL_TRANSLATION_TABLE.0
        ) as usize));
    }

    // Make sure it's visible to the cpu.
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    // Run the per cpu mmu code on the bootstrap cpu, which will also switch to the
    // new kernel translation table.
    riscv64_mmu_early_init_percpu();
}

/// Early MMU initialization per CPU.
fn riscv64_mmu_early_init_percpu() {
    riscv64_switch_kernel_asid(kernel_asid());
    riscv64_tlb_flush_all();
}

/// Pre-VM initialization: populates top-level kernel page table pointers.
pub(crate) fn riscv64_mmu_prevm_init() {
    // Fill in all of the unused top level page table pointers for the kernel half of both
    // the bootstrap and the kernel top level table.  These entries will be copied to all
    // new address spaces, thus ensuring the top level entries are synchronized.
    for i in MMU_PT_KERNEL_BASE_INDEX..NUM_PAGE_TABLE_ENTRIES {
        // SAFETY: reading one entry of the kernel translation table, a kernel-lifetime
        // static that is only mutated during early MMU init before secondaries run.
        let pte = unsafe { KERNEL_TRANSLATION_TABLE.0[i] };
        if !pte_is_valid(pte) {
            let (page, paddr) = match crate::vm::pmm::alloc_page(0) {
                Ok((page, paddr)) => (page, paddr),
                Err(status) => {
                    panic!("error allocating page to back kernel page table: {:?}", status);
                }
            };
            // SAFETY: `page` is the `vm_page_t` just returned by `pmm::alloc_page`, so this code
            // owns it and is the only thing that can be setting its state.
            unsafe {
                page.set_state(crate::vm::page_state::VmPageState(
                    crate::vm::page_state::bindings::vm_page_state::MMU,
                ));
                let out_paddr = paddr.0 as u64;
                arch_zero_page(paddr_to_physmap(out_paddr) as *mut core::ffi::c_void);
                let non_leaf = mmu_non_leaf_pte(out_paddr, true);
                core::ptr::write_volatile(&mut KERNEL_TRANSLATION_TABLE.0[i], non_leaf);
                core::ptr::write_volatile(&mut BOOTSTRAP_TRANSLATION_TABLE.0[i], non_leaf);
            }
        }
    }

    // Make sure any updates to the page tables are visible to the cpu.
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);
}

/// MMU subsystem post-init report.
pub(crate) fn riscv64_mmu_init() {
    dprintf!(INFO, "RISCV: MMU enabled sv39\n");
    dprintf!(
        INFO,
        "RISCV: MMU ASID mask {:#x}, using asids {}\n",
        *RISCV_ASID_MASK,
        *RISCV_USE_ASID
    );
}

/// Returns the physical address of the bootstrap translation table for secondary CPU boot.
fn riscv64_get_bootstrap_translation_table() -> u64 {
    // SAFETY: taking the address of `BOOTSTRAP_TRANSLATION_TABLE` accesses nothing. The table is a
    // kernel-lifetime static, mutated only during early MMU init before secondaries run.
    unsafe {
        cpp_kernel_physical_address_of(core::ptr::addr_of!(BOOTSTRAP_TRANSLATION_TABLE.0) as usize)
    }
}

/// Finds the index of the first valid entry in a page table.
fn first_used_page_table_entry(page_table: &[Pte; NUM_PAGE_TABLE_ENTRIES]) -> Option<usize> {
    for (i, &pte) in page_table.iter().enumerate() {
        if pte_is_valid(pte) {
            return Some(i);
        }
    }
    None
}

/// Returns true if all entries in the given page table are invalid.
fn page_table_is_clear(page_table: &[Pte; NUM_PAGE_TABLE_ENTRIES]) -> bool {
    first_used_page_table_entry(page_table).is_none()
}

/// Address space type classification matching Riscv64AspaceType.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum AspaceType {
    User = 0,
    Kernel = 1,
    Guest = 2,
}

impl AspaceType {
    const fn from_flags(mmu_flags: u32) -> Self {
        // Kernel/Guest flags are mutually exclusive. Ensure at most 1 is set.
        let is_kernel = (mmu_flags & (ARCH_ASPACE_FLAG_KERNEL as u32)) != 0;
        let is_guest = (mmu_flags & (ARCH_ASPACE_FLAG_GUEST as u32)) != 0;
        debug_assert!(!(is_kernel && is_guest));
        if is_kernel {
            AspaceType::Kernel
        } else if is_guest {
            AspaceType::Guest
        } else {
            AspaceType::User
        }
    }

    const fn name(self) -> &'static str {
        match self {
            AspaceType::User => "user",
            AspaceType::Kernel => "kernel",
            AspaceType::Guest => "guest",
        }
    }
}

/// Address space role in unified address space architecture.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum AspaceRole {
    Independent = 0,
    Restricted = 1,
    Shared = 2,
    Unified = 3,
}

/// Helper struct for tracking physical and virtual address ranges during mapping.
struct MappingCursor<'a> {
    paddrs: &'a [u64],
    paddr_idx: usize,
    paddr_consumed: usize,
    page_size: usize,
    vaddr: usize,
    vaddr_rel_offset: usize,
    size: usize,
}

impl<'a> MappingCursor<'a> {
    pub fn new(paddrs: &'a [u64], page_size: usize, vaddr: usize) -> Self {
        let size = page_size * paddrs.len();
        Self {
            paddrs,
            paddr_idx: 0,
            paddr_consumed: 0,
            page_size,
            vaddr,
            vaddr_rel_offset: 0,
            size,
        }
    }

    pub fn set_vaddr_relative_offset(&mut self, offset: usize, max: usize) -> bool {
        let rel = self.vaddr - offset;
        if rel > max - self.size || self.size > max {
            return false;
        }
        self.vaddr_rel_offset = offset;
        true
    }

    pub fn consume(&mut self, ps: usize) {
        debug_assert!(self.size >= ps);
        self.paddr_consumed += ps;
        debug_assert!(self.paddr_consumed <= self.page_size);
        if self.paddr_consumed == self.page_size {
            self.paddr_idx += 1;
            self.paddr_consumed = 0;
        }
        self.vaddr += ps;
        self.size -= ps;
    }

    pub fn paddr(&self) -> u64 {
        debug_assert!(self.paddr_consumed < self.page_size);
        self.paddrs[self.paddr_idx] + self.paddr_consumed as u64
    }

    pub fn page_remaining(&self) -> usize {
        self.page_size - self.paddr_consumed
    }

    pub fn vaddr(&self) -> usize {
        self.vaddr
    }

    pub fn vaddr_rel(&self) -> usize {
        self.vaddr - self.vaddr_rel_offset
    }

    pub fn size(&self) -> usize {
        self.size
    }
}

/// Splits a large page table entry (1 GiB or 2 MiB) into a child page table of next-level entries.
///
/// # Safety
/// `page_table` must point to a valid, page-aligned page table of 512 entries.
unsafe fn split_large_page(
    vaddr: usize,
    level: usize,
    pt_index: usize,
    page_table: *mut Pte,
    cm: core::pin::Pin<&mut ConsistencyManager<'_>>,
    aspace: &LockedAspace<'_, '_>,
) -> Result<(), Status> {
    if level == 0 || pt_index >= NUM_PAGE_TABLE_ENTRIES {
        return Err(Status::INVALID_ARGS);
    }
    // SAFETY: pt_index is within bounds of page_table.
    let old_pte = unsafe { core::ptr::read_volatile(page_table.add(pt_index)) };
    if !pte_is_leaf(old_pte) {
        return Err(Status::BAD_STATE);
    }

    let paddr = match aspace.alloc_page_table() {
        Ok(paddr) => paddr,
        Err(status) => {
            dprintf!(INFO, "split_large_page: failed to allocate page table\n");
            return Err(status);
        }
    };

    let new_page_table_ptr = paddr_to_physmap(paddr) as *mut Pte;
    // SAFETY: new_page_table_ptr is a valid allocated page mapped into physmap.
    let new_page_table =
        unsafe { &mut *(new_page_table_ptr as *mut [Pte; NUM_PAGE_TABLE_ENTRIES]) };
    populate_split_page_table(new_page_table, old_pte, level);

    // Ensure page table initialization becomes visible prior to page table installation.
    core::sync::atomic::fence(core::sync::atomic::Ordering::SeqCst);

    let non_leaf_pte = mmu_non_leaf_pte(paddr, aspace.is_kernel());
    // SAFETY: Write non-leaf PTE pointing to new child page table into parent table.
    unsafe {
        core::ptr::write_volatile(page_table.add(pt_index), non_leaf_pte);
    }

    // Queue non-terminal TLB flush.  No need to update the page table count here since
    // we're replacing a block entry with a table entry.
    cm.flush_entry(vaddr, Flush::NonTerminal);

    Ok(())
}

/// Recursively unmaps a virtual address range from the page table hierarchy.
///
/// # Safety
/// `page_table` must point to a valid, page-aligned page table of 512 entries.
unsafe fn unmap_page_table(
    mut vaddr: usize,
    mut vaddr_rel: usize,
    mut size: usize,
    enlarge: u8,
    level: usize,
    page_table: *mut Pte,
    mut cm: core::pin::Pin<&mut ConsistencyManager<'_>>,
    aspace: &LockedAspace<'_, '_>,
) -> Result<usize, Status> {
    let block_size = page_size_per_level(level);
    let block_mask = block_size - 1;
    let mut unmap_size = 0usize;

    while size > 0 {
        let vaddr_rem = vaddr_rel & block_mask;
        let chunk_size = core::cmp::min(size, block_size - vaddr_rem);
        let index = vaddr_to_index(vaddr_rel, level);

        // SAFETY: index is within bounds of page_table.
        let mut pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };

        // If the input range partially covers a large page, attempt to split.
        if level > 0 && pte_is_valid(pte) && pte_is_leaf(pte) && chunk_size != block_size {
            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            let split_res =
                unsafe { split_large_page(vaddr, level, index, page_table, cm.as_mut(), aspace) };
            match split_res {
                Ok(()) => {
                    // SAFETY: Reload split PTE from page_table.
                    pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };
                }
                Err(err) => {
                    // If the split failed then we just fall through and unmap the entire
                    // large page.
                    if enlarge == ARCH_UNMAP_OPTION_NONE {
                        return Err(err);
                    }
                }
            }
        }

        // Check for an inner page table pointer.
        if level > 0 && pte_is_valid(pte) && !pte_is_leaf(pte) {
            let page_table_paddr = pte_paddr(pte);
            let next_page_table = paddr_to_physmap(page_table_paddr) as *mut Pte;

            // Recurse a level.
            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            let _ = unsafe {
                unmap_page_table(
                    vaddr,
                    vaddr_rem,
                    chunk_size,
                    enlarge,
                    level - 1,
                    next_page_table,
                    cm.as_mut(),
                    aspace,
                )?
            };

            let is_top_level = level == NUM_PAGE_TABLE_LEVELS - 1;
            // If this is an entry corresponding to a top level kernel page table, skip
            // freeing it so that we always keep these kernel page tables populated in all
            // address spaces.
            let kernel_top_level =
                aspace.is_kernel() && (index >= MMU_PT_KERNEL_BASE_INDEX) && is_top_level;
            // Similarly, if this is an entry corresponding to a top level shared page table,
            // skip freeing it as there may be several unified aspaces referencing its
            // contents.
            let shared_top_level = aspace.is_shared() && is_top_level;

            // Check if next page table is now empty or if entire chunk was unmapped.
            // SAFETY: `next_page_table` addresses a page-table page through the physmap, which is
            // exactly NUM_PAGE_TABLE_ENTRIES PTEs and is page-aligned.
            let next_is_clear = || unsafe {
                let next_slice = &*(next_page_table as *const [Pte; NUM_PAGE_TABLE_ENTRIES]);
                page_table_is_clear(next_slice)
            };

            if !kernel_top_level
                && !shared_top_level
                && (chunk_size == block_size || next_is_clear())
            {
                // Free the page table entry.
                // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
                // write stays inside the 512-entry table `page_table` points at.
                unsafe {
                    core::ptr::write_volatile(page_table.add(index), 0);
                }

                // If this is a restricted aspace and we are updating the top level page table,
                // update the top level page of the associated unified aspace as well.
                if is_top_level {
                    aspace.publish_unified_top_level(index, 0);
                }

                // If we unmapped an entire page table leaf and/or the unmap made the level
                // below us empty, free the page table.  We can safely defer TLB flushing as
                // the consistency manager will not return the backing page to the PMM until
                // after the tlb is flushed.
                cm.as_mut().flush_entry(vaddr, Flush::NonTerminal);
                cm.as_mut().free_page_table(next_page_table, page_table_paddr);
            }
        } else if pte_is_valid(pte) {
            // Unmap leaf page.
            // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
            // write stays inside the 512-entry table `page_table` points at.
            unsafe {
                core::ptr::write_volatile(page_table.add(index), 0);
            }
            cm.as_mut().flush_entry(vaddr, Flush::Terminal);
        }

        vaddr += chunk_size;
        vaddr_rel += chunk_size;
        size -= chunk_size;
        unmap_size += chunk_size;
    }

    Ok(unmap_size)
}

/// Recursively updates permission attributes on a virtual address range.
///
/// # Safety
/// `page_table` must point to a valid, page-aligned page table of 512 entries.
unsafe fn protect_page_table(
    mut vaddr: usize,
    mut vaddr_rel: usize,
    mut size: usize,
    attrs: Pte,
    level: usize,
    page_table: *mut Pte,
    mut cm: core::pin::Pin<&mut ConsistencyManager<'_>>,
    aspace: &LockedAspace<'_, '_>,
) -> Result<(), Status> {
    let block_size = page_size_per_level(level);
    let block_mask = block_size - 1;

    // vaddr_rel and size must be page aligned
    debug_assert!(((vaddr_rel | size) & PAGE_MASK) == 0);

    while size > 0 {
        let vaddr_rem = vaddr_rel & block_mask;
        let chunk_size = core::cmp::min(size, block_size - vaddr_rem);
        let index = vaddr_to_index(vaddr_rel, level);

        // SAFETY: index is within bounds of page_table.
        let mut pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };

        // If the input range partially covers a large page, split the page.
        if level > 0 && pte_is_valid(pte) && pte_is_leaf(pte) && chunk_size != block_size {
            // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
            // read stays inside the 512-entry table `page_table` points at.
            unsafe { split_large_page(vaddr, level, index, page_table, cm.as_mut(), aspace)? };
            // SAFETY: Reload split PTE from page_table.
            pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };
        }

        if level > 0 && pte_is_valid(pte) && !pte_is_leaf(pte) {
            let page_table_paddr = pte_paddr(pte);
            let next_page_table = paddr_to_physmap(page_table_paddr) as *mut Pte;

            // Recurse a level.
            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            unsafe {
                protect_page_table(
                    vaddr,
                    vaddr_rem,
                    chunk_size,
                    attrs,
                    level - 1,
                    next_page_table,
                    cm.as_mut(),
                    aspace,
                )?;
            }
        } else if pte_is_valid(pte) {
            let new_pte = (pte & !RISCV64_PTE_PERM_MASK) | attrs;
            // Skip updating the page table entry if the new value is the same as before.
            if new_pte != pte {
                // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
                // write stays inside the 512-entry table `page_table` points at.
                unsafe {
                    core::ptr::write_volatile(page_table.add(index), new_pte);
                }
                cm.as_mut().flush_entry(vaddr, Flush::Terminal);
            }
        }

        vaddr += chunk_size;
        vaddr_rel += chunk_size;
        size -= chunk_size;
    }

    Ok(())
}

/// Recursively harvests access bits for a virtual address range.
///
/// # Safety
/// `page_table` must point to a valid, page-aligned page table of 512 entries.
unsafe fn harvest_accessed_page_table(
    mut vaddr: usize,
    mut vaddr_rel: usize,
    mut size: usize,
    terminal_action: TerminalAction,
    level: usize,
    page_table: *mut Pte,
    mut cm: core::pin::Pin<&mut ConsistencyManager<'_>>,
    aspace: &LockedAspace<'_, '_>,
) {
    let block_size = page_size_per_level(level);
    let block_mask = block_size - 1;

    // vaddr_rel and size must be page aligned
    debug_assert!(((vaddr_rel | size) & PAGE_MASK) == 0);

    while size > 0 {
        let vaddr_rem = vaddr_rel & block_mask;
        let chunk_size = core::cmp::min(size, block_size - vaddr_rem);
        let index = vaddr_to_index(vaddr_rel, level);

        // SAFETY: index is within bounds of page_table.
        let mut pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };

        if level > 0 && pte_is_valid(pte) && pte_is_leaf(pte) && chunk_size != block_size {
            // Ignore large pages, we do not support harvesting accessed bits from them.
            // Having this empty branch simplifies the overall logic.
        } else if level > 0 && pte_is_valid(pte) && !pte_is_leaf(pte) {
            let page_table_paddr = pte_paddr(pte);
            let next_page_table = paddr_to_physmap(page_table_paddr) as *mut Pte;
            // Recurse into the next level.
            // NOTE: we currently cannot honor `NonTerminalAction::FreeUnaccessed` since
            // accessed information is not being tracked on inner nodes.
            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            unsafe {
                harvest_accessed_page_table(
                    vaddr,
                    vaddr_rem,
                    chunk_size,
                    terminal_action,
                    level - 1,
                    next_page_table,
                    cm.as_mut(),
                    aspace,
                );
            }
        } else if pte_is_valid(pte) && (pte & RISCV64_PTE_A) != 0 {
            let pte_addr = pte_paddr(pte);
            let paddr = pte_addr + (vaddr_rem as u64);

            // False for a mapping with no `vm_page_t` behind it (a physical VMO),
            // which has no accessed state to harvest.
            let has_page = mark_page_accessed(paddr);

            if has_page && terminal_action == TerminalAction::UpdateAgeAndHarvest {
                // Modifying the access flag does not require break-before-make for
                // correctness, and as we do not support hardware access flag setting at the
                // moment we do not have to deal with potential concurrent modifications.
                pte &= !RISCV64_PTE_A;
                // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
                // write stays inside the 512-entry table `page_table` points at.
                unsafe {
                    core::ptr::write_volatile(page_table.add(index), pte);
                }
                cm.as_mut().flush_entry(vaddr, Flush::Terminal);
            }
        }

        vaddr += chunk_size;
        vaddr_rel += chunk_size;
        size -= chunk_size;
    }
}

/// Recursively marks the accessed bit on valid PTEs in a virtual address range.
///
/// # Safety
/// `page_table` must point to a valid, page-aligned page table of 512 entries.
unsafe fn mark_accessed_page_table(
    mut vaddr: usize,
    mut vaddr_rel: usize,
    mut size: usize,
    level: usize,
    page_table: *mut Pte,
    aspace: &LockedAspace<'_, '_>,
) {
    let block_size = page_size_per_level(level);
    let block_mask = block_size - 1;

    // vaddr_rel and size must be page aligned
    debug_assert!(((vaddr_rel | size) & PAGE_MASK) == 0);

    while size > 0 {
        let vaddr_rem = vaddr_rel & block_mask;
        let chunk_size = core::cmp::min(size, block_size - vaddr_rem);
        let index = vaddr_to_index(vaddr_rel, level);

        // SAFETY: index is within bounds of page_table.
        let pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };

        if level > 0 && pte_is_valid(pte) && pte_is_leaf(pte) && chunk_size != block_size {
            // Ignore large pages as we don't support modifying their access flags.
        } else if level > 0 && pte_is_valid(pte) && !pte_is_leaf(pte) {
            let page_table_paddr = pte_paddr(pte);
            let next_page_table = paddr_to_physmap(page_table_paddr) as *mut Pte;
            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            unsafe {
                mark_accessed_page_table(
                    vaddr,
                    vaddr_rem,
                    chunk_size,
                    level - 1,
                    next_page_table,
                    aspace,
                );
            }
        } else if pte_is_valid(pte) {
            let new_pte = pte | RISCV64_PTE_A;
            // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
            // write stays inside the 512-entry table `page_table` points at.
            unsafe {
                core::ptr::write_volatile(page_table.add(index), new_pte);
            }
        }

        vaddr += chunk_size;
        vaddr_rel += chunk_size;
        size -= chunk_size;
    }
}

/// Recursively maps a physical address range into the page table hierarchy using the cursor.
///
/// # Safety
/// `page_table` must point to a valid, page-aligned page table of 512 entries.
unsafe fn map_page_table(
    attrs: Pte,
    ro: bool,
    level: usize,
    page_table: *mut Pte,
    existing_action: ExistingEntryAction,
    cursor: &mut MappingCursor<'_>,
    mut cm: core::pin::Pin<&mut ConsistencyManager<'_>>,
    aspace: &LockedAspace<'_, '_>,
) -> Result<(), Status> {
    let block_size = page_size_per_level(level);
    let mut index = vaddr_to_index(cursor.vaddr(), level);

    while index < NUM_PAGE_TABLE_ENTRIES && cursor.size() != 0 {
        // SAFETY: index is within bounds [0..512) of page_table.
        let mut pte = unsafe { core::ptr::read_volatile(page_table.add(index)) };

        // If we're at an unaligned address, and not trying to map a block larger than 1GB,
        // recurse one more level of the page table tree.
        let level_valigned = (cursor.vaddr_rel() & (block_size - 1)) == 0;
        let level_paligned = (cursor.paddr() & (block_size as u64 - 1)) == 0;
        if !level_valigned || !level_paligned || cursor.page_remaining() < block_size || level > 2 {
            let page_table_paddr: u64;
            let next_page_table: *mut Pte;

            if !pte_is_valid(pte) {
                page_table_paddr = match aspace.alloc_page_table() {
                    Ok(paddr) => paddr,
                    Err(status) => {
                        dprintf!(INFO, "map_page_table: failed to allocate page table\n");
                        // The mapping wasn't fully updated, but there is work here that
                        // might need to be undone as we may have allocated various levels
                        // of page tables.  By consuming a single page we make the cleanup
                        // operation think we have added a mapping here, causing it to
                        // check the page table for potential cleanup.
                        cursor.consume(page::SIZE);
                        return Err(status);
                    }
                };
                let pt_vaddr = paddr_to_physmap(page_table_paddr) as *mut Pte;
                ltracef!("allocated page table, vaddr {pt_vaddr:p}, paddr {page_table_paddr:#x}\n");
                // SAFETY: `pt_vaddr` is the physmap address of the page just allocated for this
                // table, so the whole page::SIZE range is ours to zero.
                unsafe {
                    core::ptr::write_bytes(pt_vaddr as *mut u8, 0, page::SIZE);
                }

                // Ensure that the zeroing is observable from hardware page table walkers;
                // as this must happen before the pte is written it cannot be deferred to
                // the consistency manager.
                mb();

                pte = mmu_non_leaf_pte(page_table_paddr, aspace.is_kernel());
                // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
                // write stays inside the 512-entry table `page_table` points at.
                unsafe {
                    core::ptr::write_volatile(page_table.add(index), pte);
                }

                // If this is a restricted aspace and we are updating the top level page table,
                // add the page table entry to the top level page of the associated unified
                // aspace as well.
                if level == NUM_PAGE_TABLE_LEVELS - 1 {
                    aspace.publish_unified_top_level(index, pte);
                }
                // We do not need to sync the walker, despite writing a new entry, as this is
                // a non-terminal entry and so is irrelevant to the walker anyway.

                next_page_table = pt_vaddr;
            } else if !pte_is_leaf(pte) {
                page_table_paddr = pte_paddr(pte);
                next_page_table = paddr_to_physmap(page_table_paddr) as *mut Pte;
            } else {
                return Err(Status::ALREADY_EXISTS);
            }

            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            unsafe {
                map_page_table(
                    attrs,
                    ro,
                    level - 1,
                    next_page_table,
                    existing_action,
                    cursor,
                    cm.as_mut(),
                    aspace,
                )?;
            }
        } else {
            let new_pte = paddr_to_pte(cursor.paddr()) | attrs;
            let valid = pte_is_valid(pte);

            if valid && existing_action == ExistingEntryAction::Error {
                return Err(Status::ALREADY_EXISTS);
            } else if valid && existing_action == ExistingEntryAction::Skip {
                // Empty case to simplify the other branches.
            } else if valid
                && existing_action == ExistingEntryAction::Upgrade
                && pte_paddr(pte) == cursor.paddr()
            {
                // Doing an upgrade of an existing entry where the output address is not
                // changing.  This is just a protect, which we can skip if either nothing is
                // actually changing, or if we would potentially be reducing permissions.
                if !ro && new_pte != pte {
                    // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
                    // write stays inside the 512-entry table `page_table` points at.
                    unsafe {
                        core::ptr::write_volatile(page_table.add(index), new_pte);
                    }
                    cm.as_mut().flush_entry(cursor.vaddr(), Flush::Terminal);
                }
            } else {
                // Either no current entry, or we need to upgrade the existing one,
                // potentially performing a break-before-make.
                if valid && !ro {
                    // If the output address were not changing we would have hit the protect
                    // case above, so if the new entry is not read only then we must perform
                    // break-before-make before installing it.  Failing to do this could
                    // result in writes being temporarily lost due to the different output
                    // addresses.
                    // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
                    // write stays inside the 512-entry table `page_table` points at.
                    unsafe {
                        core::ptr::write_volatile(page_table.add(index), 0);
                    }
                    cm.as_mut().flush_entry(cursor.vaddr(), Flush::Terminal);
                    // Must force the flush to happen now, before installing the new entry.
                    // This will also ensure the page table entries we wrote will be visible
                    // before we install it.
                    cm.as_mut().flush();
                }
                // SAFETY: `index` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
                // write stays inside the 512-entry table `page_table` points at.
                unsafe {
                    core::ptr::write_volatile(page_table.add(index), new_pte);
                }

                // Flush the TLB on map as well, unlike most architectures.
                if aspace.is_kernel() {
                    // Normally we only need a local fence here and secondary cpus at worst
                    // would only get a spurious page fault.  However, since spurious PFs are
                    // not tolerated in the kernel, we want to do a full flush via the
                    // ConsistencyManager for kernel addresses.
                    cm.as_mut().flush_entry(cursor.vaddr(), Flush::Terminal);
                } else {
                    // Perform a local sfence.vma on the single page in the local asid.  If
                    // another cpu were to page fault on this user address, it will sfence.vma
                    // in its PF handler.
                    riscv64_tlb_flush_address_one_asid(cursor.vaddr(), aspace.asid());
                    CM_LOCAL_PAGE_INVALIDATE.add(1);
                }
            }

            cursor.consume(block_size);
        }

        index += 1;
    }

    Ok(())
}

/// High-level mapping function with rollback on partial allocation failure and full consistency management.
///
/// # Safety
/// `page_table` must point to a valid root translation table.
unsafe fn map_pages(
    vaddr: usize,
    paddrs: &[u64],
    page_size: usize,
    mmu_flags: u32,
    existing_action: ExistingEntryAction,
    page_table: *mut Pte,
    aspace: &LockedAspace<'_, '_>,
) -> Result<(), Status> {
    let attrs = mmu_flags_to_pte_attr(mmu_flags, aspace.is_kernel());
    let ro = (mmu_flags & (ARCH_MMU_FLAG_PERM_RWX_MASK as u32)) == (ARCH_MMU_FLAG_PERM_READ as u32);
    stack_pin_init!(let cm = ConsistencyManager::new(aspace));
    let mut cursor = MappingCursor::new(paddrs, page_size, vaddr);
    // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
    // entries -- either the root table the aspace owns or the child reached via
    // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
    // callee documents that it requires.
    let result = unsafe {
        map_page_table(
            attrs,
            ro,
            NUM_PAGE_TABLE_LEVELS - 1,
            page_table,
            existing_action,
            &mut cursor,
            cm.as_mut(),
            aspace,
        )
    };

    aspace.aspace.accessed_since_last_check.store(true, Ordering::Relaxed);
    mb();

    if let Err(err) = result {
        if cursor.vaddr() > vaddr {
            let rollback_size = cursor.vaddr() - vaddr;
            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            let unmap_res = unsafe {
                unmap_page_table(
                    vaddr,
                    vaddr,
                    rollback_size,
                    ARCH_UNMAP_OPTION_NONE,
                    NUM_PAGE_TABLE_LEVELS - 1,
                    page_table,
                    cm.as_mut(),
                    aspace,
                )
            };
            assert!(unmap_res.is_ok());
        }
        return Err(err);
    }

    debug_assert_eq!(cursor.size(), 0);
    Ok(())
}

/// High-level unmap function with full consistency management.
///
/// # Safety
/// `page_table` must point to a valid root translation table.
unsafe fn unmap_pages(
    vaddr: usize,
    vaddr_rel: usize,
    size: usize,
    enlarge: u8,
    level: usize,
    page_table: *mut Pte,
    aspace: &LockedAspace<'_, '_>,
) -> Result<(), Status> {
    stack_pin_init!(let cm = ConsistencyManager::new(aspace));
    // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
    // entries -- either the root table the aspace owns or the child reached via
    // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
    // callee documents that it requires.
    unsafe {
        unmap_page_table(vaddr, vaddr_rel, size, enlarge, level, page_table, cm.as_mut(), aspace)
    }
    .map(|_| ())
}

/// High-level protect function with full consistency management.
///
/// # Safety
/// `page_table` must point to a valid root translation table.
unsafe fn protect_pages(
    vaddr: usize,
    vaddr_rel: usize,
    size: usize,
    mmu_flags: u32,
    level: usize,
    page_table: *mut Pte,
    aspace: &LockedAspace<'_, '_>,
) -> Result<(), Status> {
    let attrs = mmu_flags_to_pte_attr(mmu_flags, aspace.is_kernel());
    stack_pin_init!(let cm = ConsistencyManager::new(aspace));
    // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
    // entries -- either the root table the aspace owns or the child reached via
    // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
    // callee documents that it requires.
    unsafe {
        protect_page_table(vaddr, vaddr_rel, size, attrs, level, page_table, cm.as_mut(), aspace)
    }
}

/// High-level harvest accessed function with full consistency management.
///
/// # Safety
/// `page_table` must point to a valid root translation table.
unsafe fn harvest_accessed(
    vaddr: usize,
    vaddr_rel: usize,
    size: usize,
    terminal_action: TerminalAction,
    level: usize,
    page_table: *mut Pte,
    aspace: &LockedAspace<'_, '_>,
) {
    stack_pin_init!(let cm = ConsistencyManager::new(aspace));
    // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
    // entries -- either the root table the aspace owns or the child reached via
    // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
    // callee documents that it requires.
    unsafe {
        harvest_accessed_page_table(
            vaddr,
            vaddr_rel,
            size,
            terminal_action,
            level,
            page_table,
            cm.as_mut(),
            aspace,
        );
    }
}

/// Initialize a new user top-level page table: zeroes lower half (user entries 0..256)
/// and copies upper half (kernel entries 256..512) from the kernel translation table.
///
/// # Safety
/// `top_table` must point to a valid 512-entry page table.
unsafe fn init_user_top_level_page_table(top_table: *mut Pte) {
    let half_entries = NUM_PAGE_TABLE_ENTRIES / 2;
    // SAFETY: `top_table` is a page-aligned table of NUM_PAGE_TABLE_ENTRIES entries, so
    // the two halves written here are in bounds, and the kernel table is a distinct
    // kernel-lifetime static so the copy does not overlap.
    unsafe {
        // Zero lower half (user space entries)
        core::ptr::write_bytes(top_table, 0, half_entries);
        // Copy upper half (kernel space entries) from kernel translation table
        let kernel_table = core::ptr::addr_of!(KERNEL_TRANSLATION_TABLE.0) as *const Pte;
        core::ptr::copy_nonoverlapping(
            kernel_table.add(half_entries),
            top_table.add(half_entries),
            half_entries,
        );
    }
}

/// Prepopulate shared aspace top-level page table by allocating and zeroing L1 tables.
///
/// # Safety
/// Memory barrier: issues `fence iorw,iorw`.
#[inline(always)]
fn mb() {
    // SAFETY: a fence orders memory accesses; it touches no memory itself.
    unsafe {
        core::arch::asm!("fence iorw,iorw", options(nostack, preserves_flags));
    }
}

/// Prepopulate shared aspace top-level page table by allocating and zeroing L1 tables.
///
/// # Safety
/// `aspace` must have a valid root translation table installed.
unsafe fn init_shared_aspace_page_tables(aspace: &LockedAspace<'_, '_>) -> Result<(), Status> {
    let top_table = aspace.top_table();
    let top_level = NUM_PAGE_TABLE_LEVELS - 1;
    let start = vaddr_to_index(aspace.base(), top_level);
    let end = vaddr_to_index(aspace.base() + aspace.size() - 1, top_level);
    for i in start..=end {
        let pt_paddr = aspace.alloc_page_table()?;
        let pt_virt = paddr_to_physmap(pt_paddr);
        // SAFETY: the argument is the physmap address of a page this code just allocated,
        // so the whole page is ours to zero.
        unsafe {
            arch_zero_page(pt_virt as *mut core::ffi::c_void);
        }
        // Ensure that the zeroing is observable from hardware page table walkers
        // before the pte pointing at the page is written.
        mb();
        let pte = mmu_non_leaf_pte(pt_paddr, false);
        // SAFETY: `i` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
        // write stays inside the 512-entry table `top_table` points at.
        unsafe {
            core::ptr::write_volatile(top_table.add(i), pte);
        }
    }
    Ok(())
}

/// Free the statically allocated L1 page tables for a shared aspace.
///
/// # Safety
/// `top_table` must point to a valid root translation table.
unsafe fn destroy_shared_aspace_page_tables(
    aspace: &Riscv64ArchVmAspace,
    base: usize,
    size: usize,
    top_table: *mut Pte,
) {
    let top_level = NUM_PAGE_TABLE_LEVELS - 1;
    let start = vaddr_to_index(base, top_level);
    let end = vaddr_to_index(base + size - 1, top_level);
    for i in start..=end {
        // SAFETY: `i` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
        // read stays inside the 512-entry table `top_table` points at.
        let pte = unsafe { core::ptr::read_volatile(top_table.add(i)) };
        if pte_is_valid(pte) {
            let paddr = pte_paddr(pte);
            free_page_table_page(paddr);
            aspace.pt_pages.fetch_sub(1, Ordering::Relaxed);
            // SAFETY: `i` is within `top_table`, which the caller guarantees.
            unsafe { core::ptr::write_volatile(top_table.add(i), 0) };
        }
    }
}

/// Checks whether the user portion (entries 0..256) of a top-level page table is empty.
///
/// # Safety
/// `top_table` must point to a valid 512-entry page table.
unsafe fn is_user_top_level_page_table_empty(top_table: *const Pte) -> bool {
    let half_entries = NUM_PAGE_TABLE_ENTRIES / 2;
    for i in 0..half_entries {
        // SAFETY: `i` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
        // read stays inside the 512-entry table `top_table` points at.
        let pte = unsafe { core::ptr::read_volatile(top_table.add(i)) };
        if pte != 0 {
            return false;
        }
    }
    true
}

/// Context switch between address spaces: configures SATP and issues necessary fences.
///
/// # Safety
/// `new_tt_phys` must be the physical address of a live root translation table
/// and `new_asid` the ASID it was initialized with; the kernel table is used
/// when `is_user` is false.
unsafe fn riscv64_aspace_context_switch(
    new_asid: u16,
    new_tt_phys: u64,
    is_user: bool,
    use_asid: bool,
) {
    let satp = if is_user {
        Satp::from_fields(SATP_MODE_SV39, new_asid, new_tt_phys)
    } else {
        let kernel_phys = *KERNEL_TRANSLATION_TABLE_PHYS;
        Satp::from_fields(SATP_MODE_SV39, kernel_asid(), kernel_phys)
    };

    satp.write();
    mb();

    // If we're not using hardware features, flush all non global TLB entries on
    // context switch.
    if !use_asid {
        riscv64_tlb_flush_asid(MMU_RISCV64_UNUSED_ASID);
    }
}

/// Information returned by `init_aspace`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
struct AspaceInitInfo {
    pub tt_phys: u64,
    pub tt_virt: *mut Pte,
    pub asid: u16,
}

/// Allocates an address space's root translation table and ASID.  Nothing is
/// recorded in `aspace` itself; see `commit_init()`.
fn init_aspace(aspace: &Riscv64ArchVmAspace, is_unified: bool) -> Result<AspaceInitInfo, Status> {
    let (base, size, aspace_type) = (aspace.base, aspace.size, aspace.aspace_type);
    // Validate that the base + size is valid and doesn't wrap.
    debug_assert!(size > 0 || is_unified);
    debug_assert!((base & PAGE_MASK) == 0);
    debug_assert!((size & PAGE_MASK) == 0);
    debug_assert!(size == 0 || base.checked_add(size - 1).is_some());

    // The base and size of a unified aspace are expected to be zero.
    if !is_unified && size == 0 {
        return Err(Status::INVALID_ARGS);
    }
    if (base & PAGE_MASK) != 0 || (size & PAGE_MASK) != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if size > 0 && base.checked_add(size - 1).is_none() {
        return Err(Status::INVALID_ARGS);
    }

    match aspace_type {
        AspaceType::Kernel => {
            // At the moment we can only deal with address spaces as globally defined.
            debug_assert!(base == KERNEL_ASPACE_BASE);
            debug_assert!(size == KERNEL_ASPACE_SIZE);
            aspace.pt_pages.store(1, Ordering::Relaxed);
            Ok(AspaceInitInfo {
                tt_phys: *KERNEL_TRANSLATION_TABLE_PHYS,
                // SAFETY: taking the address of `KERNEL_TRANSLATION_TABLE` accesses nothing.
                // The table is a kernel-lifetime static, mutated only during early MMU
                // init before secondaries run.
                tt_virt: unsafe { core::ptr::addr_of_mut!(KERNEL_TRANSLATION_TABLE.0) as *mut Pte },
                asid: kernel_asid(),
            })
        }
        AspaceType::User => {
            debug_assert!(
                is_unified || is_user_base_size_valid(base, size),
                "base {base:#x} size {size:#x}"
            );
            if !is_unified && !is_user_base_size_valid(base, size) {
                return Err(Status::INVALID_ARGS);
            }
            // If using asids, assign a unique asid per process.  If not, set the UNUSED
            // asid to this address space, which will be the same between all aspaces.
            let asid = if *RISCV_USE_ASID {
                match super::asid_allocator::asid_alloc() {
                    Ok(asid) => asid,
                    Err(status) => {
                        dprintf!(CRITICAL, "RISC-V: out of ASIDs!\n");
                        return Err(status);
                    }
                }
            } else {
                MMU_RISCV64_UNUSED_ASID
            };

            let pa = match aspace.alloc_page_table() {
                Ok(pa) => pa,
                Err(status) => {
                    if asid != MMU_RISCV64_UNUSED_ASID {
                        let _ = super::asid_allocator::asid_free(asid);
                    }
                    return Err(status);
                }
            };

            let va = paddr_to_physmap(pa) as *mut Pte;
            // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
            // entries -- either the root table the aspace owns or the child reached via
            // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
            // callee documents that it requires.
            unsafe { init_user_top_level_page_table(va) };

            Ok(AspaceInitInfo { tt_phys: pa, tt_virt: va, asid })
        }
        AspaceType::Guest => Err(Status::NOT_SUPPORTED),
    }
}

/// Frees the top-level page table page of `aspace` and decrements its `pt_pages` count.
fn free_top_level_page(aspace: &Riscv64ArchVmAspace, tt_phys: u64) {
    if tt_phys != 0 {
        free_page_table_page(tt_phys);
        aspace.pt_pages.fetch_sub(1, Ordering::Relaxed);
    }
}

/// Destroys an individual (non-unified) address space: frees shared L1 tables if shared,
/// validates user table is empty, flushes and frees ASID, and frees root translation table.
///
/// # Safety
/// `top_table` must point to a valid root translation table.
unsafe fn destroy_individual_aspace(
    aspace: &Riscv64ArchVmAspace,
    role: AspaceRole,
    num_references: u32,
    base: usize,
    size: usize,
    top_table: *mut Pte,
    asid: u16,
    tt_phys: u64,
) -> Result<(), Status> {
    debug_assert_ne!(role, AspaceRole::Unified);
    debug_assert_eq!(num_references, 0);
    // If this is a shared aspace, its top level page table was statically prepopulated.
    // Therefore, we need to clean up all of those entries manually here.
    if role == AspaceRole::Shared && !top_table.is_null() {
        // SAFETY: the table passed here is page-aligned with NUM_PAGE_TABLE_ENTRIES
        // entries -- either the root table the aspace owns or the child reached via
        // `paddr_to_physmap` from the non-leaf PTE just read -- which is what the
        // callee documents that it requires.
        unsafe { destroy_shared_aspace_page_tables(aspace, base, size, top_table) };
    }
    // Check to see if the top level page table is empty.  If not, the user didn't
    // properly unmap everything before destroying the aspace.  These are debug
    // checks only: the root page and the ASID are released either way.
    if !top_table.is_null() {
        // SAFETY: `top_table` is non-null and points to the aspace's 512-entry root table.
        let is_empty = unsafe { is_user_top_level_page_table_empty(top_table) };
        debug_assert!(
            is_empty,
            "Top level page table still in use: aspace {:p} tt_virt {:p}",
            aspace, top_table
        );
        let pt_pages = aspace.pt_pages.load(Ordering::Relaxed);
        debug_assert_eq!(
            pt_pages, 1,
            "Too many page table pages: aspace {:p} pt_pages {}",
            aspace, pt_pages
        );
    }
    // Flush the ASID associated with this aspace.
    if *RISCV_USE_ASID && asid != MMU_RISCV64_UNUSED_ASID {
        flush_asid(&FlushTarget {
            is_shared: role == AspaceRole::Shared,
            asid,
            ..Default::default()
        });
        assert!(super::asid_allocator::asid_free(asid).is_ok());
    }
    free_top_level_page(aspace, tt_phys);
    Ok(())
}

/// Destroys a unified address space: flushes and frees ASID and frees root translation table.
fn destroy_unified_aspace(aspace: &Riscv64ArchVmAspace, asid: u16, tt_phys: u64) {
    // Flush the ASID associated with this aspace.
    if *RISCV_USE_ASID && asid != MMU_RISCV64_UNUSED_ASID {
        flush_asid(&FlushTarget { asid, ..Default::default() });
        assert!(super::asid_allocator::asid_free(asid).is_ok());
    }
    free_top_level_page(aspace, tt_phys);
}

/// Instruction cache consistency manager for memory mapping operations.
struct VmICacheConsistencyManager {
    need_invalidate: bool,
}

impl VmICacheConsistencyManager {
    pub const fn new() -> Self {
        Self { need_invalidate: false }
    }

    pub fn sync_addr(&mut self, start: usize, len: usize) {
        // Validate we are operating on a kernel address range.
        debug_assert!(is_kernel_address(start));
        // Clean the data cache to ensure the instructions are written back to main memory
        // (or PoC) before fence.i is executed.
        super::cache::arch_clean_cache_range(start, len);
        // Track that we'll need to fence.i at the end; the address is not important.
        self.need_invalidate = true;
    }

    pub fn finish(&mut self) {
        if self.need_invalidate {
            rust_riscv64_icache_finish();
            self.need_invalidate = false;
        }
    }
}

impl Drop for VmICacheConsistencyManager {
    fn drop(&mut self) {
        self.finish();
    }
}

/// Maps a contiguous run of pages.
///
/// # Safety
/// `aspace` must be a live aspace whose `Init*()` succeeded.
unsafe fn aspace_map_contiguous(
    aspace: &LockedAspace<'_, '_>,
    vaddr: usize,
    paddr: u64,
    count: usize,
    mmu_flags: u8,
) -> Result<(), Status> {
    let top_table = aspace.top_table();
    debug_assert!(!top_table.is_null());
    if top_table.is_null() {
        return Err(Status::BAD_STATE);
    }
    debug_assert!(aspace.is_valid_vaddr(vaddr));
    debug_assert!((vaddr & PAGE_MASK) == 0);
    debug_assert!((paddr as usize & PAGE_MASK) == 0);
    if !aspace.is_valid_vaddr(vaddr) {
        return Err(Status::OUT_OF_RANGE);
    }
    if (mmu_flags & ARCH_MMU_FLAG_PERM_READ) == 0 {
        return Err(Status::INVALID_ARGS);
    }
    if (vaddr & PAGE_MASK) != 0 || (paddr as usize & PAGE_MASK) != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if count == 0 {
        return Ok(());
    }
    if !aspace.is_valid_range(vaddr, count) {
        return Err(Status::OUT_OF_RANGE);
    }

    if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
        let mut cache_cm = VmICacheConsistencyManager::new();
        cache_cm.sync_addr(paddr_to_physmap(paddr) as usize, count * page::SIZE);
    }

    // SAFETY: `top_table` was null-checked above and is the root translation table
    // this aspace owns.
    unsafe {
        map_pages(
            vaddr,
            &[paddr],
            count * page::SIZE,
            mmu_flags as u32,
            ExistingEntryAction::Error,
            top_table,
            aspace,
        )
    }
}

/// Maps an array of pages.
///
/// # Safety
/// `aspace` must be a live aspace whose `Init*()` succeeded.
unsafe fn aspace_map(
    aspace: &LockedAspace<'_, '_>,
    vaddr: usize,
    phys: &[u64],
    mmu_flags: u8,
    existing_action: ExistingEntryAction,
) -> Result<(), Status> {
    let top_table = aspace.top_table();
    debug_assert!(!top_table.is_null());
    if top_table.is_null() {
        return Err(Status::BAD_STATE);
    }
    debug_assert!(aspace.is_valid_vaddr(vaddr));
    debug_assert!((vaddr & PAGE_MASK) == 0);
    let count = phys.len();
    if !aspace.is_valid_vaddr(vaddr) {
        return Err(Status::OUT_OF_RANGE);
    }
    if (vaddr & PAGE_MASK) != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if (mmu_flags & ARCH_MMU_FLAG_PERM_READ) == 0 {
        return Err(Status::INVALID_ARGS);
    }
    for &pa in phys {
        debug_assert!((pa as usize & PAGE_MASK) == 0);
        if (pa as usize & PAGE_MASK) != 0 {
            return Err(Status::INVALID_ARGS);
        }
    }
    if count == 0 {
        return Ok(());
    }
    if !aspace.is_valid_range(vaddr, count) {
        return Err(Status::OUT_OF_RANGE);
    }

    if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
        let mut cache_cm = VmICacheConsistencyManager::new();
        for &pa in phys {
            cache_cm.sync_addr(paddr_to_physmap(pa) as usize, page::SIZE);
        }
    }

    // SAFETY: `top_table` was null-checked above and is the root translation table
    // this aspace owns.
    unsafe {
        map_pages(vaddr, phys, page::SIZE, mmu_flags as u32, existing_action, top_table, aspace)
    }
}

/// Unmaps a range of virtual addresses.
///
/// TODO(https://fxbug.dev/412464435): Implement ArchUnmapOptions::Harvest for riscv.
///
/// # Safety
/// `aspace` must be a live aspace whose `Init*()` succeeded.
unsafe fn aspace_unmap(
    aspace: &LockedAspace<'_, '_>,
    vaddr: usize,
    count: usize,
    enlarge: u8,
) -> Result<(), Status> {
    let top_table = aspace.top_table();
    debug_assert!(!top_table.is_null());
    if top_table.is_null() {
        return Err(Status::BAD_STATE);
    }
    debug_assert!(aspace.is_valid_vaddr(vaddr));
    debug_assert!((vaddr & PAGE_MASK) == 0);
    if !aspace.is_valid_vaddr(vaddr) {
        return Err(Status::OUT_OF_RANGE);
    }
    if (vaddr & PAGE_MASK) != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if count == 0 {
        return Ok(());
    }
    if !aspace.is_valid_range(vaddr, count) {
        return Err(Status::OUT_OF_RANGE);
    }

    // SAFETY: `top_table` was null-checked above and is the root translation table
    // this aspace owns.
    unsafe {
        unmap_pages(
            vaddr,
            vaddr,
            count * page::SIZE,
            enlarge,
            NUM_PAGE_TABLE_LEVELS - 1,
            top_table,
            aspace,
        )
    }
}

/// Protects a range of virtual addresses.
///
/// # Safety
/// `aspace` must be a live aspace whose `Init*()` succeeded.
unsafe fn aspace_protect(
    aspace: &LockedAspace<'_, '_>,
    vaddr: usize,
    count: usize,
    mmu_flags: u8,
    _enlarge: u8,
) -> Result<(), Status> {
    let top_table = aspace.top_table();
    if top_table.is_null() {
        return Err(Status::BAD_STATE);
    }
    if !aspace.is_valid_vaddr(vaddr) {
        return Err(Status::INVALID_ARGS);
    }
    if (vaddr & PAGE_MASK) != 0 {
        return Err(Status::INVALID_ARGS);
    }
    if (mmu_flags & ARCH_MMU_FLAG_PERM_READ) == 0 {
        return Err(Status::INVALID_ARGS);
    }
    if count == 0 {
        return Ok(());
    }
    if !aspace.is_valid_range(vaddr, count) {
        return Err(Status::INVALID_ARGS);
    }

    if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
        // Pages becoming executable need their caches synced first.  The sync has
        // to run on kernel virtual addresses to avoid translation faults, so each
        // page is queried for its physical address and synced through the physmap.
        // Only pages that are not yet executable need it.  This could be folded
        // into protect_pages(), but making an existing region executable is rare,
        // so keep it simple.
        VM_MMU_PROTECT_MAKE_EXECUTE_CALLS.add(1);
        let mut cache_cm = VmICacheConsistencyManager::new();
        let mut pages_synced = 0i64;
        for idx in 0..count {
            let page_va = vaddr + idx * page::SIZE;
            if let Ok((pa, flags)) =
                query_page_table(top_table, aspace.base(), aspace.size(), page_va)
                && (flags & ARCH_MMU_FLAG_PERM_EXECUTE) == 0
            {
                cache_cm.sync_addr(paddr_to_physmap(pa) as usize, page::SIZE);
                pages_synced += 1;
            }
        }
        VM_MMU_PROTECT_MAKE_EXECUTE_PAGES.add(pages_synced);
    }

    // SAFETY: `top_table` was null-checked above and is the root translation table
    // this aspace owns.
    unsafe {
        protect_pages(
            vaddr,
            vaddr,
            count * page::SIZE,
            mmu_flags as u32,
            NUM_PAGE_TABLE_LEVELS - 1,
            top_table,
            aspace,
        )
    }
}

/// Harvests accessed state for a range.
///
/// # Safety
/// `aspace` must be a live aspace whose `Init*()` succeeded.
unsafe fn aspace_harvest_accessed(
    aspace: &LockedAspace<'_, '_>,
    vaddr: usize,
    count: usize,
    terminal: TerminalAction,
) -> Result<(), Status> {
    let top_table = aspace.top_table();
    if top_table.is_null() {
        return Err(Status::BAD_STATE);
    }
    if (vaddr & PAGE_MASK) != 0 || !aspace.is_valid_vaddr(vaddr) {
        return Err(Status::INVALID_ARGS);
    }
    if count == 0 {
        return Ok(());
    }
    if !aspace.is_valid_range(vaddr, count) {
        return Err(Status::INVALID_ARGS);
    }
    // SAFETY: `top_table` was null-checked above and is the root translation table
    // this aspace owns.
    unsafe {
        harvest_accessed(
            vaddr,
            vaddr,
            count * page::SIZE,
            terminal,
            NUM_PAGE_TABLE_LEVELS - 1,
            top_table,
            aspace,
        );
    }
    Ok(())
}

/// Marks a range as accessed.
///
/// # Safety
/// `aspace` must be a live aspace whose `Init*()` succeeded.
unsafe fn aspace_mark_accessed(
    aspace: &LockedAspace<'_, '_>,
    vaddr: usize,
    count: usize,
) -> Result<(), Status> {
    let top_table = aspace.top_table();
    if top_table.is_null() {
        return Err(Status::BAD_STATE);
    }
    if (vaddr & PAGE_MASK) != 0 || !aspace.is_valid_vaddr(vaddr) {
        return Err(Status::OUT_OF_RANGE);
    }
    if count == 0 {
        return Ok(());
    }
    if !aspace.is_valid_range(vaddr, count) {
        return Err(Status::OUT_OF_RANGE);
    }
    // SAFETY: `top_table` was null-checked above and is the root translation table
    // this aspace owns.
    unsafe {
        mark_accessed_page_table(
            vaddr,
            vaddr,
            count * page::SIZE,
            NUM_PAGE_TABLE_LEVELS - 1,
            top_table,
            aspace,
        );
    }
    aspace.aspace.accessed_since_last_check.store(true, Ordering::Relaxed);
    Ok(())
}

/// Switches this hart to `new`, or to the kernel address space when `new` is `None`,
/// and accounts for `old` no longer running here.  Assumes `old != new`.
///
/// Runs inside a chainlock transaction, so takes no mutex; everything it reads is
/// either write-once boot state or an atomic written only by `Init*()` and
/// `Destroy()`, as in upstream `ContextSwitch()`.
fn aspace_context_switch(old: Option<&Riscv64ArchVmAspace>, new: Option<&Riscv64ArchVmAspace>) {
    let use_asid = *RISCV_USE_ASID;
    match new {
        None => {
            // SAFETY: switches this hart to the kernel root page table, which is
            // fully populated for the lifetime of the kernel.
            unsafe {
                riscv64_aspace_context_switch(
                    kernel_asid(),
                    *KERNEL_TRANSLATION_TABLE_PHYS,
                    false,
                    use_asid,
                );
            }
        }
        Some(new) => {
            new.canary.assert();
            debug_assert_eq!(new.aspace_type, AspaceType::User);
            let prev = new.num_active_cpus.fetch_add(1, Ordering::Relaxed);
            debug_assert!(prev < crate::kernel::mp::SMP_MAX_CPUS as u32);
            new.accessed_since_last_check.store(true, Ordering::Relaxed);

            // A unified aspace may reach the mappings of its shared and restricted
            // halves indirectly, so mark those accessed too.  Both pointers are set
            // once by `InitUnified()` and cleared once by `Destroy()`; the aspaces
            // they point at outlive this one.
            let shared = new.shared_aspace.load(Ordering::Relaxed);
            if !shared.is_null() {
                // SAFETY: see above; non-null only for a unified aspace.
                unsafe { &*shared }.accessed_since_last_check.store(true, Ordering::Relaxed);
                let restricted = new.referenced_aspace.load(Ordering::Relaxed);
                if !restricted.is_null() {
                    // SAFETY: as above.
                    unsafe { &*restricted }
                        .accessed_since_last_check
                        .store(true, Ordering::Relaxed);
                }
            }

            // SAFETY: installs the root page table `Init*()` built for `new`, which is
            // live per this function's contract.
            unsafe {
                riscv64_aspace_context_switch(
                    new.asid.load(Ordering::Relaxed),
                    new.tt_phys.load(Ordering::Relaxed),
                    true,
                    use_asid,
                );
            }
        }
    }
    if let Some(old) = old {
        let prev = old.num_active_cpus.fetch_sub(1, Ordering::Relaxed);
        debug_assert!(prev > 0);
    }
}

/// When clearing `accessed_since_last_check` we cannot just exchange with `false`, since
/// the hardware page table walker can directly update accessed information asynchronously
/// and so new accessed information could become available without any further aspace
/// calls.  Therefore, if there are presently any CPUs executing this aspace, we must assume
/// that they could be generating new accesses.  This is equivalent to the idea that we set
/// `accessed_since_last_check` whenever we context switch to -- i.e. begin executing -- an
/// aspace.
fn accessed_since_last_check(num_active_cpus: u32, accessed: &AtomicBool, clear: bool) -> bool {
    // Read whether any CPUs are presently executing this aspace.
    let currently_active = num_active_cpus != 0;
    if clear {
        accessed.swap(currently_active, Ordering::Relaxed)
    } else {
        accessed.load(Ordering::Relaxed)
    }
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_aspace_type_from_flags(mmu_flags: u8) -> AspaceType {
    AspaceType::from_flags(mmu_flags as u32)
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_mmu_early_init() {
    riscv64_mmu_early_init();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_mmu_early_init_percpu() {
    riscv64_mmu_early_init_percpu();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_mmu_prevm_init() {
    riscv64_mmu_prevm_init();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_mmu_init() {
    riscv64_mmu_init();
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_get_bootstrap_translation_table() -> u64 {
    riscv64_get_bootstrap_translation_table()
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_icache_sync_addr(start: usize, len: usize) {
    debug_assert!(is_kernel_address(start));
    super::cache::arch_clean_cache_range(start, len);
}

#[unsafe(no_mangle)]
pub extern "C" fn rust_riscv64_icache_finish() {
    // Shootdown on all cores.  Using mp::sync_exec instead of an SBI remote fence
    // to handle race conditions with cores going offline.
    // SAFETY: `fence.i` only invalidates this hart's instruction cache, which is a
    // cache of memory that has already been written.
    crate::kernel::mp::sync_exec(crate::kernel::mp::MpIpiTarget::All, 0, || unsafe {
        core::arch::asm!("fence.i", options(nostack, preserves_flags))
    });
}

/// C++ `ArchVmAspaceInterface::page_alloc_fn_t`: the hook a test can install to
/// intercept page table allocations.
type PageAllocFn = unsafe extern "C" fn(
    u32,
    *mut *mut crate::vm::page_state::bindings::vm_page_t,
    *mut u64,
) -> i32;

/// The Rust half of a `Riscv64ArchVmAspace`.
///
/// Everything that describes the page tables is behind `mutex`, which is the lock the
/// C++ implementation used to hold across each of these operations.  Guarding the
/// fields rather than merely taking a lock is deliberate: the C++ marked them
/// `TA_GUARDED(lock_)` and the walkers `TA_REQ(lock_)`, and losing that annotation
/// with the port is what let the lock quietly disappear.  `#[guarded_by]` puts the
/// same check back, enforced by the compiler rather than by review.  The page table
/// walkers get at the guarded fields through a [`LockedAspace`], which can only be
/// built from a held guard.
///
/// `base`, `size`, `aspace_type` and the allocation hook are fixed at construction
/// and never written again, so they need no guard.  `num_active_cpus` and
/// `accessed_since_last_check` stay atomic and deliberately lock-free: the context
/// switch path updates them, and upstream reads them outside the lock too.
#[ksync::guarded]
#[pin_init::pin_data(PinnedDrop)]
pub struct Riscv64ArchVmAspace {
    pub(crate) canary: fbl::Canary<{ fbl::magic(b"VAAS") }>,

    /// Serializes every page table operation on this aspace.
    ///
    /// `NESTABLE` because every aspace shares one lock class, and the unified aspace
    /// paths touch more than one of them.  `InitUnified()` and `Destroy()` only ever
    /// hold one at a time, as upstream does with its separate guard scopes; the one
    /// place two are held together is [`LockedAspace::publish_unified_top_level`],
    /// which takes the unified aspace's lock while holding its restricted half's --
    /// the same order upstream's `AssertOrderedLock` establishes in `MapPages()` and
    /// `UnmapPageTable()`.  Lockdep classifies by class, not by instance, hence the
    /// flag.
    #[mutex(flags = lockdep::LOCK_FLAGS_NESTABLE)]
    pub(crate) mutex: ksync::KMutex,

    pub(crate) base: usize,
    pub(crate) size: usize,
    pub(crate) aspace_type: AspaceType,
    pub(crate) num_active_cpus: core::sync::atomic::AtomicU32,
    pub(crate) accessed_since_last_check: core::sync::atomic::AtomicBool,
    pub(crate) test_page_alloc_func: Option<PageAllocFn>,

    /// Read lock-free by the context switch path, which runs inside a chainlock
    /// transaction where taking a blocking mutex is illegal -- upstream `ContextSwitch()`
    /// reads `asid_` and `tt_phys_` with no lock for the same reason.  Both are written
    /// only by `Init*()` and `Destroy()`; you cannot context switch to an aspace that is
    /// being torn down.
    pub(crate) asid: core::sync::atomic::AtomicU16,

    /// Count of page table pages this aspace owns.
    ///
    /// Deliberately atomic and *not* `#[guarded_by(mutex)]`: it is bumped by
    /// `alloc_page_table()`, which the walkers call while the mutex is already held by
    /// their caller, and it is only read by debug assertions.
    pub(crate) pt_pages: core::sync::atomic::AtomicUsize,

    /// See `asid`.
    pub(crate) tt_phys: AtomicU64,

    #[guarded_by(mutex)]
    pub(crate) tt_virt: *mut Pte,

    #[guarded_by(mutex)]
    pub(crate) role: AspaceRole,

    #[guarded_by(mutex)]
    pub(crate) num_references: u32,

    /// The shared and restricted halves of a unified aspace.  Set once by
    /// `rust_riscv64_aspace_init_unified()` and cleared once by
    /// `rust_riscv64_aspace_destroy()`; like `asid`, the context switch path
    /// reads them lock-free, as upstream `ContextSwitch()` does.
    ///
    /// For a restricted aspace, `referenced_aspace` points the other way: at the
    /// unified aspace built on top of it, or null.  It is written under the
    /// restricted aspace's own mutex, so a walker holding that mutex may follow it.
    pub(crate) shared_aspace: core::sync::atomic::AtomicPtr<Riscv64ArchVmAspace>,

    /// See `shared_aspace`.
    pub(crate) referenced_aspace: core::sync::atomic::AtomicPtr<Riscv64ArchVmAspace>,
}

#[pin_init::pinned_drop]
impl pin_init::PinnedDrop for Riscv64ArchVmAspace {
    fn drop(self: core::pin::Pin<&mut Self>) {
        // Destroy() will have freed the final page table if it ran correctly, and further
        // validated that everything else was freed.
        debug_assert_eq!(self.pt_pages.load(Ordering::Relaxed), 0);
    }
}

// The C++ `Riscv64ArchVmAspace` holds this object inline in an `OpaqueStorage`
// sized by <arch/riscv64/aspace-constants.h>.
zr::static_assert_size_and_align!(
    Riscv64ArchVmAspace,
    aspace_constants::kRiscv64ArchVmAspaceStateSize,
    aspace_constants::kRiscv64ArchVmAspaceStateAlign,
);

impl Riscv64ArchVmAspace {
    /// Constructs an aspace in place.  Nothing is mapped and no ASID is taken until
    /// one of the `Init*` entry points runs.
    ///
    /// # Safety
    /// `ptr` must be valid for a write of `Self`, suitably aligned, and must not
    /// move afterwards: the aspace is pinned by its mutex.
    unsafe fn init_in_place(
        ptr: *mut Self,
        base: usize,
        size: usize,
        aspace_type: AspaceType,
        test_page_alloc_func: Option<PageAllocFn>,
    ) {
        // SAFETY: `ptr` is valid, aligned storage per this function's contract; the
        // pinned initialiser writes every field.
        unsafe {
            let _ = pin_init::PinInit::__pinned_init(
                pin_init::pin_init!(Self {
                    canary: fbl::Canary::new(),
                    mutex <- ksync::KMutex::init(),
                    aspace_type,
                    base,
                    size,
                    test_page_alloc_func,
                    num_active_cpus: core::sync::atomic::AtomicU32::new(0),
                    accessed_since_last_check: core::sync::atomic::AtomicBool::new(false),
                    asid: core::sync::atomic::AtomicU16::new(0),
                    tt_phys: AtomicU64::new(0),
                    tt_virt: core::ptr::null_mut::<Pte>().into(),
                    pt_pages: core::sync::atomic::AtomicUsize::new(0),
                    role: AspaceRole::Independent.into(),
                    num_references: 0u32.into(),
                    shared_aspace: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
                    referenced_aspace: core::sync::atomic::AtomicPtr::new(core::ptr::null_mut()),
                }),
                ptr,
            );
        }
    }

    fn is_kernel(&self) -> bool {
        self.aspace_type == AspaceType::Kernel
    }

    fn is_valid_vaddr(&self, vaddr: usize) -> bool {
        is_valid_vaddr(self.base, self.size, vaddr)
    }

    /// Whether the `count`-page run starting at `vaddr` lies entirely inside the
    /// aspace.  A run that wraps the address space is not valid.  `count` must
    /// be non-zero.
    fn is_valid_range(&self, vaddr: usize, count: usize) -> bool {
        match page_run_end(vaddr, count) {
            Some(end) => self.is_valid_vaddr(vaddr) && self.is_valid_vaddr(end),
            None => false,
        }
    }

    /// Allocates a page table page and returns its physical address.
    ///
    /// Mirrors the C++ `Riscv64ArchVmAspace::AllocPageTable()`: use the test allocation
    /// hook if one was installed at construction, otherwise go straight to the PMM.
    fn alloc_page_table(&self) -> Result<u64, Status> {
        let (page, paddr) = match self.test_page_alloc_func {
            None => match crate::vm::pmm::alloc_page(0) {
                Ok((page, paddr)) => (page, paddr.0 as u64),
                Err(_) => return Err(Status::NO_MEMORY),
            },
            Some(alloc) => {
                let mut page: *mut crate::vm::page_state::bindings::vm_page_t =
                    core::ptr::null_mut();
                let mut paddr: u64 = 0;
                // SAFETY: `page` and `paddr` are valid out-parameters.
                if unsafe { alloc(0, &mut page, &mut paddr) } != 0 {
                    return Err(Status::NO_MEMORY);
                }
                // SAFETY: the hook reported success, so `page` is a valid page.
                match unsafe { crate::vm::page::VmPagePtr::from_ffi(page) } {
                    Some(page) => (page, paddr),
                    None => return Err(Status::NO_MEMORY),
                }
            }
        };

        // SAFETY: `gPhysmapSize` is a C++ global initialized before MMU operations run.
        debug_assert!((paddr as usize) < unsafe { gPhysmapSize });

        // Upstream `AllocPageTable()` does both of these.  The count is what lets
        // teardown tell whether the address space handed every page table back.
        // SAFETY: `page` was just allocated and is not owned by anything else yet.
        unsafe {
            page.set_state(crate::vm::page_state::VmPageState(
                crate::vm::page_state::bindings::vm_page_state::MMU,
            ));
        }
        self.pt_pages.fetch_add(1, Ordering::Relaxed);
        ltracef!("allocated {paddr:#x}\n");
        Ok(paddr)
    }
}

/// An aspace whose mutex the caller holds: the page table walkers' view.
///
/// Built from the guard rather than from a pointer so that the guarded fields the
/// walkers read (`role`, `tt_virt`) are provably under the lock, and so that the
/// caller is obliged to keep holding it for the whole walk -- the walkers assume
/// serialized access to the tables, as upstream's `TA_REQ(lock_)` spelled out.
struct LockedAspace<'a, 'g> {
    aspace: &'a Riscv64ArchVmAspace,
    guard: &'a Riscv64ArchVmAspaceMutexGuard<'g>,
}

impl<'a, 'g> LockedAspace<'a, 'g> {
    fn new(aspace: &'a Riscv64ArchVmAspace, guard: &'a Riscv64ArchVmAspaceMutexGuard<'g>) -> Self {
        Self { aspace, guard }
    }

    fn base(&self) -> usize {
        self.aspace.base
    }

    fn size(&self) -> usize {
        self.aspace.size
    }

    fn is_valid_vaddr(&self, vaddr: usize) -> bool {
        self.aspace.is_valid_vaddr(vaddr)
    }

    fn is_valid_range(&self, vaddr: usize, count: usize) -> bool {
        self.aspace.is_valid_range(vaddr, count)
    }

    /// The root translation table, or null before `Init*()` / after `Destroy()`.
    fn top_table(&self) -> *mut Pte {
        *self.guard.tt_virt()
    }

    fn role(&self) -> AspaceRole {
        *self.guard.role()
    }

    fn is_kernel(&self) -> bool {
        self.aspace.is_kernel()
    }

    fn is_shared(&self) -> bool {
        self.role() == AspaceRole::Shared
    }

    fn is_restricted(&self) -> bool {
        self.role() == AspaceRole::Restricted
    }

    fn asid(&self) -> u16 {
        self.aspace.asid.load(Ordering::Relaxed)
    }

    fn alloc_page_table(&self) -> Result<u64, Status> {
        self.aspace.alloc_page_table()
    }

    /// The unified aspace built on top of this restricted one, if any.
    fn unified_aspace(&self) -> Option<&'a Riscv64ArchVmAspace> {
        if !self.is_restricted() {
            return None;
        }
        let unified = self.aspace.referenced_aspace.load(Ordering::Relaxed);
        if unified.is_null() {
            return None;
        }
        // SAFETY: a restricted aspace's `referenced_aspace` is only ever cleared under
        // the restricted aspace's mutex, which this view proves is held, and the
        // unified aspace it names is not freed until after `Destroy()` has cleared it.
        Some(unsafe { &*unified })
    }

    /// The address space identity that TLB invalidations for this aspace need.  A
    /// restricted aspace's mappings are also reachable through its unified aspace, so
    /// that ASID must be flushed as well, as upstream `FlushAsid()` / `FlushTLBEntry()`
    /// do.
    fn flush_target(&self) -> FlushTarget {
        let unified_asid = self.unified_aspace().map(|u| u.asid.load(Ordering::Relaxed));
        FlushTarget {
            is_kernel: self.is_kernel(),
            is_shared: self.is_shared(),
            asid: self.asid(),
            unified_asid: unified_asid.unwrap_or(0),
            has_unified_asid: unified_asid.is_some(),
        }
    }

    /// Mirrors a top-level page table entry of a restricted aspace into the unified
    /// aspace that references it, so the unified aspace sees the restricted half's
    /// page tables come and go.  A no-op for every other kind of aspace.
    ///
    /// Lock order: this aspace's mutex (held), then the unified aspace's.  Nothing
    /// ever takes the two the other way round -- `InitUnified()` and `Destroy()` lock
    /// them one at a time -- which is what upstream's `AssertOrderedLock` in
    /// `MapPages()` / `UnmapPageTable()` relied on.
    fn publish_unified_top_level(&self, index: usize, pte: Pte) {
        debug_assert!(index < NUM_PAGE_TABLE_ENTRIES);
        let Some(unified) = self.unified_aspace() else {
            return;
        };
        lock!(let guard = unified.lock_mutex());
        let unified_top_table = *guard.tt_virt();
        if unified_top_table.is_null() {
            return;
        }
        // SAFETY: `unified_top_table` is the unified aspace's root table, read under
        // its lock, and `index` indexes within its NUM_PAGE_TABLE_ENTRIES entries.
        unsafe {
            core::ptr::write_volatile(unified_top_table.add(index), pte);
        }
    }
}

/// Records the outcome of `init_aspace()` in the aspace and stamps its role.
fn commit_init(aspace: &Riscv64ArchVmAspace, info: &AspaceInitInfo, role: AspaceRole) {
    aspace.asid.store(info.asid, Ordering::Relaxed);
    aspace.tt_phys.store(info.tt_phys, Ordering::Relaxed);
    lock!(let mut guard = aspace.lock_mutex());
    let fields = guard.as_mut().fields_mut();
    *fields.tt_virt = info.tt_virt;
    *fields.role = role;
    ltracef!("tt_phys {:#x} tt_virt {:p}\n", info.tt_phys, info.tt_virt);
}

// C FFI exports: the `rust_riscv64_aspace_*` entry points `Riscv64ArchVmAspace`
// (mmu.cc) forwards to.  Each takes the C++ object's `rust_aspace_`.  A
// `Result<(), Status>` return is ABI-identical to `zx_status_t`.

/// Constructs the aspace in the C++ object's `rust_aspace_` storage.
///
/// # Safety
/// `ptr` must be valid for a write of `Riscv64ArchVmAspace`, suitably aligned, and
/// must stay put until `rust_riscv64_aspace_destruct` runs on it.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_construct(
    ptr: *mut Riscv64ArchVmAspace,
    base: usize,
    size: usize,
    aspace_type: AspaceType,
    paf: Option<PageAllocFn>,
) {
    // SAFETY: per this function's contract.
    unsafe { Riscv64ArchVmAspace::init_in_place(ptr, base, size, aspace_type, paf) }
}

/// Drops the aspace constructed by `rust_riscv64_aspace_construct`.
///
/// # Safety
/// `ptr` must be an aspace `rust_riscv64_aspace_construct` built that has not
/// already been passed here.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_destruct(ptr: *mut Riscv64ArchVmAspace) {
    // SAFETY: `ptr` is a live aspace per this function's contract, and it is dropped
    // exactly once.
    unsafe { core::ptr::drop_in_place(ptr) };
}

/// Tears down an address space: frees its page tables and releases its ASID.
///
/// This does *not* drop the `Riscv64ArchVmAspace` itself; the C++ destructor
/// does that via `rust_riscv64_aspace_destruct()`.
///
/// # Safety
/// `ptr` must be a live aspace built by `rust_riscv64_aspace_construct()`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_destroy(
    ptr: *mut Riscv64ArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: caller guarantees `ptr` is a live aspace.
    let aspace = unsafe { &*ptr };
    destroy_aspace(aspace)
}

/// Tears down `aspace`: a unified aspace drops its references on its shared and
/// restricted halves, any other releases its page tables and ASID.
fn destroy_aspace(aspace: &Riscv64ArchVmAspace) -> Result<(), Status> {
    aspace.canary.assert();
    ltracef!("aspace {:p}\n", aspace);
    debug_assert_ne!(aspace.aspace_type, AspaceType::Kernel);

    let (shared, restricted) = {
        lock!(let mut guard = aspace.lock_mutex());
        if *guard.role() == AspaceRole::Unified {
            // Take the references taken by `rust_riscv64_aspace_init_unified()` out of
            // this aspace now; they are dropped below, outside this lock, so the shared
            // and restricted aspaces are only ever locked one at a time here --
            // upstream `DestroyUnified()` uses separate scopes for the same reason.
            let shared = aspace.shared_aspace.swap(core::ptr::null_mut(), Ordering::Relaxed);
            let restricted =
                aspace.referenced_aspace.swap(core::ptr::null_mut(), Ordering::Relaxed);
            (shared, restricted)
        } else {
            let asid = aspace.asid.load(Ordering::Relaxed);
            let tt_phys = aspace.tt_phys.load(Ordering::Relaxed);
            // SAFETY: `tt_virt` is this aspace's own root translation table.
            let result = unsafe {
                destroy_individual_aspace(
                    aspace,
                    *guard.role(),
                    *guard.num_references(),
                    aspace.base,
                    aspace.size,
                    *guard.tt_virt(),
                    asid,
                    tt_phys,
                )
            };
            return match result {
                Ok(()) => {
                    *guard.as_mut().tt_virt_mut() = core::ptr::null_mut();
                    aspace.tt_phys.store(0, Ordering::Relaxed);
                    aspace.asid.store(MMU_RISCV64_UNUSED_ASID, Ordering::Relaxed);
                    Ok(())
                }
                Err(status) => Err(status),
            };
        }
    };

    if !shared.is_null() {
        // SAFETY: the shared aspace outlives every unified aspace referencing it.
        let shared_ref = unsafe { &*shared };
        lock!(let mut g = shared_ref.lock_mutex());
        debug_assert!(*g.num_references() > 0);
        let n = g.num_references().saturating_sub(1);
        *g.as_mut().num_references_mut() = n;
    }
    if !restricted.is_null() {
        // SAFETY: as above; a restricted aspace is referenced by exactly one unified
        // aspace, this one.
        let restricted_ref = unsafe { &*restricted };
        lock!(let mut g = restricted_ref.lock_mutex());
        debug_assert!(*g.num_references() == 1);
        debug_assert!(core::ptr::eq(
            restricted_ref.referenced_aspace.load(Ordering::Relaxed).cast_const(),
            aspace
        ));
        let n = g.num_references().saturating_sub(1);
        *g.as_mut().num_references_mut() = n;
        restricted_ref.referenced_aspace.store(core::ptr::null_mut(), Ordering::Relaxed);
    }

    {
        lock!(let mut guard = aspace.lock_mutex());
        let asid = aspace.asid.load(Ordering::Relaxed);
        let tt_phys = aspace.tt_phys.load(Ordering::Relaxed);
        destroy_unified_aspace(aspace, asid, tt_phys);
        *guard.as_mut().tt_virt_mut() = core::ptr::null_mut();
        aspace.tt_phys.store(0, Ordering::Relaxed);
        aspace.asid.store(MMU_RISCV64_UNUSED_ASID, Ordering::Relaxed);
    }
    Ok(())
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
/// `paddrs` must point at `count` physical addresses.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_map(
    ptr: *mut Riscv64ArchVmAspace,
    vaddr: usize,
    paddrs: *const u64,
    count: usize,
    mmu_flags: u8,
    existing_action: ExistingEntryAction,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    ltracef!("vaddr {vaddr:#x} count {count} flags {mmu_flags:#x}\n");
    let phys: &[u64] = if count == 0 || paddrs.is_null() {
        &[]
    } else {
        // SAFETY: `paddrs` points at `count` contiguous entries per the contract.
        unsafe { core::slice::from_raw_parts(paddrs, count) }
    };
    lock!(let guard = aspace.lock_mutex());
    let locked = LockedAspace::new(aspace, &guard);
    // SAFETY: `locked` proves the lock is held for the whole walk.
    unsafe { aspace_map(&locked, vaddr, phys, mmu_flags, existing_action) }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_map_contiguous(
    ptr: *mut Riscv64ArchVmAspace,
    vaddr: usize,
    paddr: u64,
    count: usize,
    mmu_flags: u8,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    ltracef!("vaddr {vaddr:#x} paddr {paddr:#x} count {count} flags {mmu_flags:#x}\n");
    lock!(let guard = aspace.lock_mutex());
    let locked = LockedAspace::new(aspace, &guard);
    // SAFETY: `locked` proves the lock is held for the whole walk.
    unsafe { aspace_map_contiguous(&locked, vaddr, paddr, count, mmu_flags) }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_unmap(
    ptr: *mut Riscv64ArchVmAspace,
    vaddr: usize,
    count: usize,
    enlarge: u8,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    ltracef!("vaddr {vaddr:#x} count {count}\n");
    lock!(let guard = aspace.lock_mutex());
    let locked = LockedAspace::new(aspace, &guard);
    // SAFETY: `locked` proves the lock is held for the whole walk.
    unsafe { aspace_unmap(&locked, vaddr, count, enlarge) }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_protect(
    ptr: *mut Riscv64ArchVmAspace,
    vaddr: usize,
    count: usize,
    mmu_flags: u8,
    enlarge: u8,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    ltracef!("vaddr {vaddr:#x} count {count} flags {mmu_flags:#x}\n");
    lock!(let guard = aspace.lock_mutex());
    let locked = LockedAspace::new(aspace, &guard);
    // SAFETY: `locked` proves the lock is held for the whole walk.
    unsafe { aspace_protect(&locked, vaddr, count, mmu_flags, enlarge) }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_accessed_since_last_check(
    ptr: *mut Riscv64ArchVmAspace,
    clear: bool,
) -> bool {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    accessed_since_last_check(
        aspace.num_active_cpus.load(Ordering::Relaxed),
        &aspace.accessed_since_last_check,
        clear,
    )
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_harvest_accessed(
    ptr: *mut Riscv64ArchVmAspace,
    vaddr: usize,
    count: usize,
    terminal: TerminalAction,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    ltracef!("vaddr {vaddr:#x} count {count}\n");
    lock!(let guard = aspace.lock_mutex());
    let locked = LockedAspace::new(aspace, &guard);
    // SAFETY: `locked` proves the lock is held for the whole walk.
    unsafe { aspace_harvest_accessed(&locked, vaddr, count, terminal) }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_mark_accessed(
    ptr: *mut Riscv64ArchVmAspace,
    vaddr: usize,
    count: usize,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    ltracef!("vaddr {vaddr:#x} count {count}\n");
    lock!(let guard = aspace.lock_mutex());
    let locked = LockedAspace::new(aspace, &guard);
    // SAFETY: `locked` proves the lock is held for the whole walk.
    unsafe { aspace_mark_accessed(&locked, vaddr, count) }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_arch_table_phys(
    ptr: *const Riscv64ArchVmAspace,
) -> u64 {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.tt_phys.load(Ordering::Relaxed)
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
/// `out_paddr` and `out_mmu_flags` must be valid for a `u64` and a `u8` write;
/// they are only written when the lookup succeeds.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_query(
    ptr: *mut Riscv64ArchVmAspace,
    vaddr: usize,
    out_paddr: *mut u64,
    // `arch_mmu_flags_t` is `uint8_t`; writing a wider type here would scribble
    // over whatever follows the caller's variable.
    out_mmu_flags: *mut u8,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    // Held across the walk: `query_page_table` reads the tables.
    lock!(let guard = aspace.lock_mutex());
    aspace.canary.assert();
    ltracef!("aspace {:p}, vaddr {vaddr:#x}\n", ptr);
    let tt_virt = *guard.tt_virt();
    match query_page_table(tt_virt, aspace.base, aspace.size, vaddr) {
        Ok((paddr, flags)) => {
            if !out_paddr.is_null() {
                // SAFETY: `out_paddr` was checked non-null immediately above, so the caller
                // asked for this value and supplied somewhere to put it.
                unsafe {
                    *out_paddr = paddr;
                }
            }
            if !out_mmu_flags.is_null() {
                // SAFETY: `out_mmu_flags` was checked non-null immediately above, so the caller
                // asked for this value and supplied somewhere to put it.
                unsafe {
                    *out_mmu_flags = flags;
                }
            }
            Ok(())
        }
        Err(status) => Err(status),
    }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` that has not been
/// destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_pick_spot(
    ptr: *mut Riscv64ArchVmAspace,
    base: usize,
    _end: usize,
    _align: usize,
    _size: usize,
    _mmu_flags: u8,
) -> usize {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` that is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    // riscv64 places no architectural constraint on where a mapping may go, so
    // the bottom of the gap the caller found is always acceptable.  The caller
    // (VmAddressRegion::AllocSpotLockedRegion) treats anything outside the gap as
    // an allocation failure and panics, so this must return a usable address.
    (base + page::SIZE - 1) & !PAGE_MASK
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` whose `Init*()`
/// succeeded and which has not been destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_asid(ptr: *const Riscv64ArchVmAspace) -> u16 {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` whose `Init*()` succeeded and which is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.asid.load(Ordering::Relaxed)
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` that has not been
/// destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_init(
    ptr: *mut Riscv64ArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` that is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    ltracef!(
        "aspace {:p}, base {:#x}, size {:#x}, type {}\n",
        ptr,
        aspace.base,
        aspace.size,
        aspace.aspace_type.name()
    );
    init_aspace(aspace, false).map(|info| commit_init(aspace, &info, AspaceRole::Independent))
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` that has not been
/// destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_init_shared(
    ptr: *mut Riscv64ArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` that is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    let info = init_aspace(aspace, false)?;
    commit_init(aspace, &info, AspaceRole::Shared);
    lock!(let guard = aspace.lock_mutex());
    let locked = LockedAspace::new(aspace, &guard);
    // SAFETY: `locked` proves the lock is held while the top-level table is filled in.
    unsafe { init_shared_aspace_page_tables(&locked) }
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` that has not been
/// destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_init_restricted(
    ptr: *mut Riscv64ArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` that is not yet destroyed.
    let aspace = unsafe { &*ptr };
    aspace.canary.assert();
    init_aspace(aspace, false).map(|info| commit_init(aspace, &info, AspaceRole::Restricted))
}

/// # Safety
/// `ptr` must be the `rust_aspace_` of a `Riscv64ArchVmAspace` that has not been
/// destroyed.  `shared` and `restricted` must be live aspaces whose `InitShared()` /
/// `InitRestricted()` succeeded, and must be the shared and restricted aspaces this
/// unified aspace is being built from.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_init_unified(
    ptr: *mut Riscv64ArchVmAspace,
    shared: *mut Riscv64ArchVmAspace,
    restricted: *mut Riscv64ArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, `ptr` is the `rust_aspace_` storage of a
    // `Riscv64ArchVmAspace` that is not yet destroyed, and C++ `InitUnified()`
    // passes two live aspaces that outlive it.
    unsafe { init_unified_aspace(ptr, shared, restricted) }
}

/// Initializes a unified aspace over `shared` and `restricted`: validates that the
/// restricted half is empty, copies the shared half's top-level entries, and takes
/// a reference on each.
///
/// # Safety
/// `ptr` must be a live, uninitialized aspace; `shared` and `restricted` must be
/// live aspaces that outlive it.
unsafe fn init_unified_aspace(
    ptr: *mut Riscv64ArchVmAspace,
    shared: *mut Riscv64ArchVmAspace,
    restricted: *mut Riscv64ArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: per this function's contract, all three are live aspaces.
    let (aspace, shared_ref, restricted_ref) = unsafe { (&*ptr, &*shared, &*restricted) };
    aspace.canary.assert();
    debug_assert_eq!(aspace.size, 0);
    debug_assert_eq!(aspace.base, 0);
    let info = init_aspace(aspace, true)?;
    commit_init(aspace, &info, AspaceRole::Unified);

    let top_level = NUM_PAGE_TABLE_LEVELS - 1;
    let restricted_start = vaddr_to_index(restricted_ref.base, top_level);
    let restricted_end = vaddr_to_index(restricted_ref.base + restricted_ref.size - 1, top_level);
    let shared_start = vaddr_to_index(shared_ref.base, top_level);
    let shared_end = vaddr_to_index(shared_ref.base + shared_ref.size - 1, top_level);
    debug_assert!(restricted_end < shared_start);
    // Upstream `InitUnified()` reaches the error cases here and below only through
    // `DEBUG_ASSERT`s -- they are programming errors, not conditions a caller can
    // hit -- so the root table and ASID that `init_aspace()` just took are
    // deliberately not unwound.
    if restricted_end >= shared_start {
        return Err(Status::INVALID_ARGS);
    }

    // Validate that the restricted aspace is empty and set its metadata.  The
    // check and the reference share one lock scope so that no top-level entry
    // can be installed in between that `publish_unified_top_level()` would miss.
    {
        lock!(let mut g = restricted_ref.lock_mutex());
        let tt = *g.tt_virt();
        let has_ref = !restricted_ref.referenced_aspace.load(Ordering::Relaxed).is_null();
        debug_assert!(!tt.is_null());
        debug_assert_eq!(*g.role(), AspaceRole::Restricted);
        debug_assert_eq!(*g.num_references(), 0);
        debug_assert!(!has_ref);
        if tt.is_null()
            || *g.role() != AspaceRole::Restricted
            || *g.num_references() != 0
            || has_ref
        {
            return Err(Status::BAD_STATE);
        }
        for i in restricted_start..=restricted_end {
            // SAFETY: `i` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so the
            // read stays inside the 512-entry table `tt` points at.
            let pte = unsafe { core::ptr::read_volatile(tt.add(i)) };
            debug_assert_eq!(pte, 0);
            if pte != 0 {
                return Err(Status::BAD_STATE);
            }
        }
        let n = *g.num_references() + 1;
        *g.as_mut().num_references_mut() = n;
        restricted_ref.referenced_aspace.store(ptr, Ordering::Relaxed);
    }

    // Copy all mappings from the shared aspace and set its metadata.
    {
        lock!(let mut g = shared_ref.lock_mutex());
        let tt = *g.tt_virt();
        debug_assert!(!tt.is_null());
        debug_assert_eq!(*g.role(), AspaceRole::Shared);
        if tt.is_null() || *g.role() != AspaceRole::Shared {
            return Err(Status::BAD_STATE);
        }
        for i in shared_start..=shared_end {
            // SAFETY: `i` is a page-table index below NUM_PAGE_TABLE_ENTRIES, so both
            // accesses stay inside the 512-entry tables `tt` and `info.tt_virt` point at.
            unsafe {
                let pte = core::ptr::read_volatile(tt.add(i));
                core::ptr::write_volatile(info.tt_virt.add(i), pte);
            }
        }
        let n = *g.num_references() + 1;
        *g.as_mut().num_references_mut() = n;
    }

    // A shared aspace may back many unified aspaces; a restricted one backs
    // exactly one.  The context switch path reads these two lock-free.
    aspace.shared_aspace.store(shared, Ordering::Relaxed);
    aspace.referenced_aspace.store(restricted, Ordering::Relaxed);
    Ok(())
}

/// # Safety
/// `new` may be null, meaning "switch to the kernel aspace". When non-null, both
/// `old` and `new` must satisfy the aspace contract above.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_riscv64_aspace_context_switch(
    old: *mut Riscv64ArchVmAspace,
    new: *mut Riscv64ArchVmAspace,
) {
    // SAFETY: both pointers are either null or live aspaces C++ still owns.
    let (old, new) = unsafe { (old.as_ref(), new.as_ref()) };
    aspace_context_switch(old, new);
}

const _: () = assert!(core::mem::size_of::<AspaceInitInfo>() == 24);
const _: () = assert!(core::mem::size_of::<SfenceVmaArgs>() == 32);

#[cfg(ktest)]
/// Unit tests for RISC-V 64 MMU encodings and TLB flush coalescing.
#[unittest::suite(name = "riscv64_mmu")]
mod tests {
    use unittest::{assert_eq, assert_false, assert_true};

    /// Test virtual address to page table index calculation across levels.
    #[test]
    fn test_vaddr_to_index() {
        let va = 0x0000_003f_ffff_e000usize;
        assert_eq!(vaddr_to_index(va, 0), 510);
        assert_eq!(vaddr_to_index(va, 1), 511);
        assert_eq!(vaddr_to_index(va, 2), 255);

        let va_kernel = 0xffff_ffc0_0000_0000usize;
        assert_eq!(vaddr_to_index(va_kernel, 2), 256);
    }

    /// Test physical address encoding and decoding in PTEs.
    #[test]
    fn test_pte_paddr_encoding() {
        let pa = 0x8000_0000u64;
        let pte = paddr_to_pte(pa);
        assert_eq!(pte_paddr(pte), pa);

        let non_leaf = mmu_non_leaf_pte(pa, true);
        assert_true!(pte_is_valid(non_leaf));
        assert_false!(pte_is_leaf(non_leaf));
        assert_eq!(pte_paddr(non_leaf), pa);
        assert_eq!(non_leaf & RISCV64_PTE_G, RISCV64_PTE_G);
    }

    /// Test conversion between MMU flags and PTE attributes.
    #[test]
    fn test_mmu_flags_conversion() {
        let flags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_USER;
        let pte = mmu_flags_to_pte_attr(flags as u32, false);
        assert_true!(pte_is_valid(pte));
        assert_true!(pte_is_leaf(pte));
        assert_eq!(pte & RISCV64_PTE_R, RISCV64_PTE_R);
        assert_eq!(pte & RISCV64_PTE_W, RISCV64_PTE_W);
        assert_eq!(pte & RISCV64_PTE_X, 0);
        assert_eq!(pte & RISCV64_PTE_U, RISCV64_PTE_U);
        assert_eq!(pte & RISCV64_PTE_G, 0);

        let roundtrip_flags = mmu_flags_from_pte(pte);
        assert_eq!(
            roundtrip_flags & ARCH_MMU_FLAG_PERM_RWX_MASK,
            flags & ARCH_MMU_FLAG_PERM_RWX_MASK
        );
        assert_eq!(roundtrip_flags & ARCH_MMU_FLAG_PERM_USER, ARCH_MMU_FLAG_PERM_USER);
    }

    /// Test validation of user address space base and size.
    #[test]
    fn test_user_base_size_valid() {
        // Valid user space ranges (within [0, 256 GiB)).
        assert_true!(is_user_base_size_valid(0x1000, 0x1000));
        assert_true!(is_user_base_size_valid(0x0000_0000_1000_0000, 0x0000_0000_2000_0000));

        // Invalid ranges
        assert_false!(is_user_base_size_valid(0, 0)); // Size 0
        assert_false!(is_user_base_size_valid(0x1001, 0x1000)); // Unaligned base
        assert_false!(is_user_base_size_valid(0x1000, 0x1001)); // Unaligned size
        assert_false!(is_user_base_size_valid(0xffff_ffc0_0000_0000, 0x1000)); // Kernel address
        assert_false!(is_user_base_size_valid(0x0000_003f_ffff_f000, 0x2000)); // Overflow into kernel space
    }

    /// Test the overflow-checked end address of a run of pages.
    #[test]
    fn test_page_run_end() {
        assert_true!(page_run_end(0x1000, 1) == Some(0x1fff));
        assert_true!(page_run_end(0x1000, 2) == Some(0x2fff));
        assert_true!(page_run_end(usize::MAX - page::SIZE + 1, 1) == Some(usize::MAX));
        // The run wraps the top of the address space.
        assert_true!(page_run_end(usize::MAX - page::SIZE + 1, 2).is_none());
        // The page count alone overflows.
        assert_true!(page_run_end(0, usize::MAX).is_none());
        assert_true!(page_run_end(0, (usize::MAX >> page::SHIFT) + 1).is_none());
    }

    /// Test SATP bitfield encoding and decoding.
    #[test]
    fn test_satp_encoding() {
        let satp = Satp::from_fields(SATP_MODE_SV39, 42, 0x8000_0000);
        assert_eq!(satp.mode(), SATP_MODE_SV39);
        assert_eq!(satp.asid(), 42);
        assert_eq!(satp.root_address(), 0x8000_0000);
    }

    /// Test ConsistencyManagerTracker queueing, run coalescing, and full flush trigger.
    #[test]
    fn test_consistency_manager_tracker() {
        let mut cm = ConsistencyManagerTracker::new();
        assert_eq!(cm.num_pending_tlb_runs, 0);
        assert_false!(cm.full_flush);

        // Queue page 0x1000 terminal
        let flush_needed = cm.flush_entry(0x1000, Flush::Terminal, false);
        assert_false!(flush_needed);
        assert_eq!(cm.num_pending_tlb_runs, 1);
        assert_eq!(cm.pending_tlbs[0].va, 0x1000);
        assert_eq!(cm.pending_tlbs[0].count, 1);

        // Queue adjacent page 0x2000 terminal (should coalesce)
        let flush_needed = cm.flush_entry(0x2000, Flush::Terminal, false);
        assert_false!(flush_needed);
        assert_eq!(cm.num_pending_tlb_runs, 1);
        assert_eq!(cm.pending_tlbs[0].va, 0x1000);
        assert_eq!(cm.pending_tlbs[0].count, 2);

        // Re-queue the page the run starts at.  Only a repeat of the run's first
        // page is recognised, not of an arbitrary page within it -- that is enough
        // for the common case of flushing the same page several times in a row.
        let flush_needed = cm.flush_entry(0x1000, Flush::Terminal, false);
        assert_false!(flush_needed);
        assert_eq!(cm.num_pending_tlb_runs, 1);
        assert_eq!(cm.pending_tlbs[0].count, 2);

        // Queue non-terminal flush (should trigger full_flush immediately)
        let flush_needed = cm.flush_entry(0x5000, Flush::NonTerminal, false);
        assert_false!(flush_needed);
        assert_true!(cm.full_flush);

        // Reset
        cm.reset();
        assert_eq!(cm.num_pending_tlb_runs, 0);
        assert_false!(cm.full_flush);

        // Fill up to max runs (8 runs)
        for i in 0..MAX_PENDING_TLB_RUNS {
            let va = (i * 2 + 1) * page::SIZE; // Non-contiguous pages
            let flush_needed = cm.flush_entry(va, Flush::Terminal, false);
            assert_false!(flush_needed);
        }
        assert_eq!(cm.num_pending_tlb_runs, MAX_PENDING_TLB_RUNS);
        assert_false!(cm.full_flush);

        // 9th entry in user space triggers full_flush
        let flush_needed = cm.flush_entry(0x50000, Flush::Terminal, false);
        assert_false!(flush_needed);
        assert_true!(cm.full_flush);
    }
}
