// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2014 Google Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM64 Memory Management Unit (MMU), page tables, TLB consistency, and icache consistency.
#![allow(clippy::too_many_arguments)]
#![allow(clippy::only_used_in_recursion)]

use super::asid_allocator::{asid_alloc, asid_allocator_init, asid_free};
use super::cache::{
    InvalidateInstructionCacheRange, arch_clean_cache_range, arm64_clean_cache_range_pou,
    invalidate_global_instruction_cache,
};
use super::vm::is_kernel_address;
use crate::counters;
use crate::kernel::mp::SMP_MAX_CPUS;
use crate::kernel::types::{PAddr, VAddr};
use crate::mmu_rcu::{AutoSimpleGenerationalReader, SimpleGenerational};
use crate::page_cache::{PageCache, PageList};
use crate::platform_rs::timer::{current_mono_time, zx_msec, zx_sec};
use crate::vm::arch_vm_aspace::{
    ARCH_ASPACE_FLAG_GUEST, ARCH_ASPACE_FLAG_KERNEL, ARCH_MMU_FLAG_CACHE_MASK,
    ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_NS, ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ,
    ARCH_MMU_FLAG_PERM_RWX_MASK, ARCH_MMU_FLAG_PERM_USER, ARCH_MMU_FLAG_PERM_WRITE,
    ARCH_MMU_FLAG_UNCACHED, ARCH_MMU_FLAG_UNCACHED_DEVICE, ARCH_MMU_FLAG_WRITE_COMBINING,
    ARCH_UNMAP_OPTION_ENLARGE, ARCH_UNMAP_OPTION_HARVEST, ARCH_UNMAP_OPTION_NONE, ArchMmuFlags,
    ArchUnmapOptions, ExistingEntryAction, NonTerminalAction, TerminalAction,
    arch_mmu_flags_uncached,
};
use crate::vm::page::{VmPage, VmPageDoublyLinkedList, VmPagePtr};
use crate::vm::page_state::VmPageState;
use crate::vm::page_state::bindings::vm_page_state;
use crate::vm::physmap::{is_physmap_phys_addr, paddr_to_physmap, physmap_to_paddr};
use crate::vm::pmm;
use arch_arm64_aspace_bindings as bindings;
use arm64_feature_bindings::arm64_asid_width;
/// ARM64 address space role and type enums shared with FFI shims.
pub(super) use bindings::{ArmAspaceRole, ArmAspaceType};
use core::pin::Pin;
use core::sync::atomic::{
    AtomicBool, AtomicPtr, AtomicU8, AtomicU16, AtomicU32, AtomicU64, AtomicUsize, Ordering,
};
use debug::{dprintf, ltracef, tracef};
use init::{LK_INIT_LEVEL_KERNEL, LK_INIT_LEVEL_THREADING, LK_INIT_LEVEL_VM, LkInitLevel};
use ksync::{KMutex, RawCriticalMutex, lock};
use lazy_init::LazyInit;
use libarch::arm64::{TTBR0_EL1, TTBR1_EL1, isb};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};
use zx_status::Status;
use zx_types::ZX_ARM64_FEATURE_ADDRESS_TAGGING_TBI;

#[cfg(sanitize = "address")]
unsafe extern "C" {
    fn asan_map_shadow_for(start: usize, size: usize);
}

const LOCAL_TRACE: u32 = 0;
const TRACE_CONTEXT_SWITCH: bool = false;

/// ktraces just local to this file
const LOCAL_KTRACE_ENABLE: bool = false;

macro_rules! local_ktrace {
    ($label:literal $(, $key:literal => $val:expr)*) => {
        if LOCAL_KTRACE_ENABLE {
            crate::ktrace_rs::cpu_instant!("kernel:probe", $label $(, $key => $val)*);
        }
    };
}

type Pte = AtomicU64;
type FirstLeafMappingResult = Result<(usize, usize, u64), (Status, usize, usize, u64)>;

/// Optional custom page allocator callback supplied for testing.
pub(super) type PageAllocFn =
    unsafe extern "C" fn(alloc_flags: u32, p: *mut *mut VmPage, pa: *mut u64) -> Result<(), Status>;

const MMU_KERNEL_SIZE_SHIFT: u32 = bindings::kMmuKernelSizeShift;
const MMU_KERNEL_TOP_SHIFT: u32 = bindings::kMmuKernelTopShift;
const MMU_KERNEL_PAGE_SIZE_SHIFT: u32 = bindings::MMU_KERNEL_PAGE_SIZE_SHIFT;
const MMU_KERNEL_PAGE_TABLE_ENTRIES: u64 = bindings::MMU_KERNEL_PAGE_TABLE_ENTRIES as u64;

const MMU_USER_SIZE_SHIFT: u32 = bindings::MMU_USER_SIZE_SHIFT;
const MMU_USER_TOP_SHIFT: u32 = bindings::kMmuUserTopShift;
const MMU_USER_PAGE_SIZE_SHIFT: u32 = bindings::MMU_USER_PAGE_SIZE_SHIFT;

const MMU_GUEST_SIZE_SHIFT: u32 = bindings::MMU_GUEST_SIZE_SHIFT;
const MMU_GUEST_TOP_SHIFT: u32 = bindings::kMmuGuestTopShift;
const MMU_GUEST_PAGE_SIZE_SHIFT: u32 = bindings::MMU_GUEST_PAGE_SIZE_SHIFT;

const MMU_PTE_DESCRIPTOR_BLOCK_MAX_SHIFT: u32 = bindings::MMU_PTE_DESCRIPTOR_BLOCK_MAX_SHIFT;

const MMU_TCR_TBI0: u64 = bindings::kMmuTcrTbi0;
const MMU_TCR_FLAGS_KERNEL: u64 = bindings::kMmuTcrFlagsKernel;
const MMU_TCR_FLAGS_USER: u64 = bindings::kMmuTcrFlagsUser;
const MMU_TCR_FLAGS_USER_RESTRICTED: u64 = bindings::kMmuTcrFlagsUserRestricted;

const MMU_PTE_VALID: u64 = bindings::kMmuPteValid;
const MMU_PTE_DESCRIPTOR_INVALID: u64 = bindings::kMmuPteDescriptorInvalid;
const MMU_PTE_DESCRIPTOR_MASK: u64 = bindings::kMmuPteDescriptorMask;
const MMU_PTE_L012_DESCRIPTOR_BLOCK: u64 = bindings::kMmuPteL012DescriptorBlock;
const MMU_PTE_L012_DESCRIPTOR_TABLE: u64 = bindings::kMmuPteL012DescriptorTable;
const MMU_PTE_L3_DESCRIPTOR_PAGE: u64 = bindings::kMmuPteL3DescriptorPage;
const MMU_PTE_OUTPUT_ADDR_MASK: u64 = bindings::kMmuPteOutputAddrMask;

const MMU_PTE_ATTR_RES_SOFTWARE: u64 = bindings::kMmuPteAttrResSoftware;
const MMU_PTE_ATTR_UXN: u64 = bindings::kMmuPteAttrUxn;
const MMU_PTE_ATTR_PXN: u64 = bindings::kMmuPteAttrPxn;
const MMU_PTE_ATTR_NON_GLOBAL: u64 = bindings::kMmuPteAttrNonGlobal;
const MMU_PTE_ATTR_AF: u64 = bindings::kMmuPteAttrAf;
const MMU_PTE_ATTR_SH_INNER_SHAREABLE: u64 = bindings::kMmuPteAttrShInnerShareable;
const MMU_PTE_ATTR_AP_P_RW_U_NA: u64 = bindings::kMmuPteAttrApPRwUNa;
const MMU_PTE_ATTR_AP_P_RW_U_RW: u64 = bindings::kMmuPteAttrApPRwURw;
const MMU_PTE_ATTR_AP_P_RO_U_NA: u64 = bindings::kMmuPteAttrApPRoUNa;
const MMU_PTE_ATTR_AP_P_RO_U_RO: u64 = bindings::kMmuPteAttrApPRoURo;
const MMU_PTE_ATTR_AP_MASK: u64 = bindings::kMmuPteAttrApMask;
const MMU_PTE_ATTR_NON_SECURE: u64 = bindings::kMmuPteAttrNonSecure;
const MMU_PTE_ATTR_ATTR_INDEX_MASK: u64 = bindings::kMmuPteAttrAttrIndexMask;
const MMU_PTE_PERMISSION_MASK: u64 = bindings::kMmuPtePermissionMask;

const MMU_PTE_ATTR_STRONGLY_ORDERED: u64 = bindings::kMmuPteAttrStronglyOrdered;
const MMU_PTE_ATTR_DEVICE: u64 = bindings::kMmuPteAttrDevice;
const MMU_PTE_ATTR_NORMAL_MEMORY: u64 = bindings::kMmuPteAttrNormalMemory;
const MMU_PTE_ATTR_NORMAL_UNCACHED: u64 = bindings::kMmuPteAttrNormalUncached;

const MMU_S2_PTE_ATTR_XN: u64 = bindings::kMmuS2PteAttrXn;
const MMU_S2_PTE_ATTR_S2AP_RO: u64 = bindings::kMmuS2PteAttrS2ApRo;
const MMU_S2_PTE_ATTR_S2AP_RW: u64 = bindings::kMmuS2PteAttrS2ApRw;
const MMU_S2_PTE_ATTR_ATTR_INDEX_MASK: u64 = bindings::kMmuS2PteAttrAttrIndexMask;
const MMU_S2_PTE_ATTR_NORMAL_MEMORY: u64 = bindings::kMmuS2PteAttrNormalMemory;
const MMU_S2_PTE_ATTR_NORMAL_UNCACHED: u64 = bindings::kMmuS2PteAttrNormalUncached;
const MMU_S2_PTE_ATTR_STRONGLY_ORDERED: u64 = bindings::kMmuS2PteAttrStronglyOrdered;
const MMU_S2_PTE_ATTR_DEVICE: u64 = bindings::kMmuS2PteAttrDevice;

const TLBI_VADDR_MASK: u64 = bindings::kMmuTlbiVaddrMask;
const MMU_ARM64_UNUSED_ASID: u16 = bindings::MMU_ARM64_UNUSED_ASID;
const MMU_ARM64_GLOBAL_ASID: u16 = bindings::MMU_ARM64_GLOBAL_ASID;
const MMU_ARM64_FIRST_USER_ASID: u16 = bindings::MMU_ARM64_FIRST_USER_ASID;

/// Use one of the ignored bits for a software simulated accessed flag for non-terminal entries.
/// TODO: Once the hardware setting of the terminal AF is supported usage of this for non-terminal
/// AF will have to become optional as we rely on the software terminal fault to set the
/// non-terminal bits.
const MMU_PTE_ATTR_RES_SOFTWARE_AF: u64 = 1u64 << 55;
// Ensure we picked a bit that is actually part of the software controlled bits.
zr::static_assert!(
    (MMU_PTE_ATTR_RES_SOFTWARE & MMU_PTE_ATTR_RES_SOFTWARE_AF) == MMU_PTE_ATTR_RES_SOFTWARE_AF
);

/// Use one of the ignored bits to do store an odd parity flag. This is checked whenever a pte is
/// read and its purpose is to help detect any stray memory corruption that impacts the page
/// tables.
const MMU_PTE_RES_PARITY: u64 = 1u64 << 56;
zr::static_assert!((MMU_PTE_ATTR_RES_SOFTWARE & MMU_PTE_RES_PARITY) == MMU_PTE_RES_PARITY);

zr::static_assert!(((bindings::kHandoffVirtualAddress as i64) >> MMU_KERNEL_SIZE_SHIFT) == -1);
zr::static_assert!((bindings::KERNEL_ASPACE_BASE >> MMU_KERNEL_SIZE_SHIFT) == -1);
zr::static_assert!(MMU_KERNEL_SIZE_SHIFT <= 48);
zr::static_assert!(MMU_KERNEL_SIZE_SHIFT >= 25);

// `root_kernel_page_table_phys` and `root_lower_page_table_phys` are written once on the boot CPU
// during early MMU initialization before secondary CPUs start, so `Ordering::Relaxed` is sufficient
// for subsequent reads.
/// Physical address of the kernel (upper `TTBR1_EL1`) root page table.
#[unsafe(no_mangle)]
pub static root_kernel_page_table_phys: AtomicU64 = AtomicU64::new(0);
/// Physical address of the initial lower (`TTBR0_EL1`) root page table.
#[unsafe(no_mangle)]
pub static root_lower_page_table_phys: AtomicU64 = AtomicU64::new(0);

/// If lock is contended, limit harvesting to 32 entries per iteration with the
/// arch aspace lock held to avoid delays in accessed faults in the same aspace
/// running in parallel.
///
/// This limit is derived from the following observations:
/// 1. Worst case runtime to harvest a terminal PTE on a low-end A53 is ~780ns.
/// 2. Real workloads can result in harvesting thousands of terminal PTEs in a
///    single aspace.
/// 3. An access fault handler will spin up to 150us on the aspace adaptive
///    mutex before blocking.
/// 4. Unnecessarily blocking is costly when the system is heavily loaded,
///    especially during accessed faults, which tend to occur multiple times in
///    quick succession within and across threads in the same process.
///
/// To achieve optimal contention between access harvesting and access faults,
/// it is important to avoid exhausting the 150us mutex spin phase by holding
/// the aspace mutex for too long. The selected entry limit results in a worst
/// case harvest time of about 1/6 of the mutex spin phase.
///
///   Ti = worst case runtime per top-level harvest iteration.
///   Te = worst case runtime per terminal entry harvest.
///   L  = max entries per top-level harvest iteration.
///
///   Ti = Te * L = 780ns * 32 = 24.96us
///
const HARVEST_ENTRIES_BETWEEN_UNLOCKS: usize = 32;

// `FEAT_ASID_ENABLED` and `ALLOW_BBM` are independent configuration flags initialized during early
// boot without publishing dependent memory state, so `Ordering::Relaxed` is sufficient.
/// Whether ASID use is enabled.
static FEAT_ASID_ENABLED: AtomicBool = AtomicBool::new(false);

/// Whether or not we allow break-before-make. Used in very early boot.
static ALLOW_BBM: AtomicBool = AtomicBool::new(false);

counters::define_kcounter!(CM_FLUSH_ALL, "mmu.consistency_manager.flush_all", Sum);
counters::define_kcounter!(
    CM_FLUSH_ALL_REPLACING,
    "mmu.consistency_manager.flush_all_replacing",
    Sum
);
counters::define_kcounter!(
    CM_SINGLE_TLB_INVALIDATES,
    "mmu.consistency_manager.single_tlb_invalidate",
    Sum
);
counters::define_kcounter!(CM_FLUSH, "mmu.consistency_manager.flush", Sum);

counters::define_kcounter!(
    VM_MMU_PROTECT_MAKE_EXECUTE_CALLS,
    "vm.mmu.protect.make_execute_calls",
    Sum
);
counters::define_kcounter!(
    VM_MMU_PROTECT_MAKE_EXECUTE_PAGES,
    "vm.mmu.protect.make_execute_pages",
    Sum
);
counters::define_kcounter!(VM_MMU_PAGE_TABLE_ALLOC, "vm.mmu.pt.alloc", Sum);
counters::define_kcounter!(VM_MMU_PAGE_TABLE_FREE, "vm.mmu.pt.free", Sum);
counters::define_kcounter!(VM_MMU_PAGE_TABLE_RECLAIM, "vm.mmu.pt.reclaim", Sum);
counters::define_kcounter!(VM_MMU_SET_AF_FAIL, "vm.mmu.set_af_fail", Sum);

static PAGE_CACHE: LazyInit<PageCache> = LazyInit::uninit();
static PAGE_CACHE_INITIALIZED: AtomicBool = AtomicBool::new(false);

fn cache_alloc_page() -> Result<VmPagePtr, Status> {
    if !PAGE_CACHE_INITIALIZED.load(Ordering::Acquire) {
        return pmm::alloc_page(pmm::ALLOC_FLAG_ANY).map(|(page, _)| page);
    }

    stack_pin_init!(let page_list = PageList::new());
    PAGE_CACHE.alloc(1, page_list.as_mut())?;

    let page = page_list.as_mut().list_mut().pop_front();
    debug_assert!(page.is_some());
    debug_assert!(page_list.is_empty());
    Ok(VmPagePtr::new(page.unwrap()))
}

/// # Safety
/// Every page in `list` must be exclusively owned, unlinked from page tables, synchronized against
/// all hardware TLBs and RCU readers, and freed only once.
unsafe fn cache_free_pages(list: Pin<&mut PageList>) {
    if !PAGE_CACHE_INITIALIZED.load(Ordering::Acquire) {
        // SAFETY: Caller guarantees every page in `list` is exclusively owned and freed once.
        unsafe { pmm::free_list(list.list_mut()) };
    } else {
        PAGE_CACHE.free(list);
    }
}

/// # Safety
/// `p` must be an exclusively owned allocated page-table page, unlinked from all intrusive lists
/// and page tables, synchronized against all hardware TLBs and RCU readers, and freed only once.
unsafe fn cache_free_page(p: VmPagePtr) {
    if !PAGE_CACHE_INITIALIZED.load(Ordering::Acquire) {
        // SAFETY: Caller guarantees `p` is an exclusively owned PMM page being freed once.
        unsafe { pmm::free_page(p) };
    } else {
        stack_pin_init!(let list = PageList::new());
        // SAFETY: Caller guarantees `p` is unlinked from all other lists and exclusively owned.
        unsafe { list.as_mut().list_mut().push_back_raw(p.as_non_null()) };

        PAGE_CACHE.free(list);
    }
}

fn initialize_page_cache(level: LkInitLevel) {
    static INIT_ONCE: AtomicBool = AtomicBool::new(false);
    assert!(!INIT_ONCE.swap(true, Ordering::AcqRel));
    assert!(level < LK_INIT_LEVEL_THREADING);

    const RESERVE_PAGES: usize = 8;
    let cache = PageCache::new(RESERVE_PAGES).expect("PageCache::new failed");

    // SAFETY: `INIT_ONCE` ensures single initialization on the boot CPU prior to
    // `LK_INIT_LEVEL_THREADING`, and callers gate access on `PAGE_CACHE_INITIALIZED` with
    // Acquire/Release ordering.
    unsafe { PAGE_CACHE.init(cache) };
    PAGE_CACHE_INITIALIZED.store(true, Ordering::Release);
}

// Initialize the cache after the percpu data structures are initialized.
init::lk_init_hook!(arm64_mmu_page_cache_init, initialize_page_cache, LK_INIT_LEVEL_KERNEL);

fn enable_bbm(_level: LkInitLevel) {
    dprintf!(INFO, "ARM: enabling break-before-make\n");
    ALLOW_BBM.store(true, Ordering::Relaxed);
}

// Enable break-before-make when splitting large pages after the VM has been initialized
// which is where a bunch of pieces of the physmap and kernel are unmapped or permissions
// lowered.
init::lk_init_hook!(arm64_mmu_enable_bbm, enable_bbm, LK_INIT_LEVEL_VM);

#[inline(always)]
fn dsb_ishst() {
    // SAFETY: Barrier only; touches no memory Rust reasons about.
    unsafe {
        core::arch::asm!("dsb ishst", options(nostack, preserves_flags));
    }
}

#[inline(always)]
fn dsb_ish() {
    // SAFETY: Barrier only; touches no memory Rust reasons about.
    unsafe {
        core::arch::asm!("dsb ish", options(nostack, preserves_flags));
    }
}

macro_rules! arm64_tlbi {
    ($op:ident, $val:expr) => {{
        // SAFETY: TLB maintenance only; touches no memory Rust reasons about, and callers
        // synchronize page-table reclamation separately before freeing unmapped pages.
        unsafe {
            core::arch::asm!(
                concat!("tlbi ", stringify!($op), ", {val}"),
                val = in(reg) ($val),
                options(nostack, preserves_flags)
            );
        }
        isb();
    }};
}

macro_rules! arm64_tlbi_noaddr {
    ($op:ident) => {{
        // SAFETY: TLB maintenance only; touches no memory Rust reasons about.
        unsafe {
            core::arch::asm!(concat!("tlbi ", stringify!($op)), options(nostack, preserves_flags));
        }
        isb();
    }};
}

macro_rules! arm64_tlbi_asid {
    ($op:ident, $asid:expr) => {{
        // SAFETY: TLB maintenance only; touches no memory Rust reasons about.
        unsafe {
            core::arch::asm!(
                concat!("tlbi ", stringify!($op), ", {val}"),
                val = in(reg) (($asid as u64) << 48),
                options(nostack, preserves_flags)
            );
        }
        isb();
    }};
}

/// Writes an architectural 64-bit system register (`ttbr0_el1` or `tcr_el1`).
///
/// # Safety
/// The caller must run on the context-switch path with migration suppressed and pass either an
/// initialized user `ArmArchVmAspace`'s `ttbr0_el1`/`tcr_el1` values (affecting only EL0 user
/// mappings) or the kernel-only zero `ttbr0_el1` with user walks disabled in `tcr_el1`, so no
/// kernel virtual mapping Rust reasons about is altered.
macro_rules! arm_wsr64 {
    ($reg:literal, $val:expr) => {{
        core::arch::asm!(
            concat!("msr ", $reg, ", {val}"),
            val = in(reg) ($val),
            options(nostack, preserves_flags)
        );
    }};
}

macro_rules! arm_rsr64 {
    ($reg:literal) => {{
        let val: u64;
        // SAFETY: Side-effect-free read of an EL1 architectural status register (`par_el1`);
        // touches no memory Rust reasons about.
        unsafe {
            core::arch::asm!(
                concat!("mrs {val}, ", $reg),
                val = out(reg) val,
                options(nostack, preserves_flags)
            );
        }
        val
    }};
}

#[inline(always)]
fn paddr_to_pte_ptr(pa: u64) -> *mut Pte {
    core::ptr::with_exposed_provenance_mut(paddr_to_physmap(PAddr(pa as usize)).0)
}

#[inline(always)]
fn arm64_vttbr(vmid: u16, baddr: u64) -> u64 {
    ((vmid as u64) << 48) | baddr
}

#[inline(always)]
const fn is_rounded(val: usize, align: usize) -> bool {
    debug_assert!(align.is_power_of_two());
    (val & (align - 1)) == 0
}

/// # Safety
/// Caller must own `page` or hold the address space lock protecting its MMU state union.
#[inline(always)]
unsafe fn get_page_num_mappings(page: VmPagePtr) -> u32 {
    // SAFETY: Caller owns or holds the lock protecting the MMU page's `mmu.num_mappings` field.
    unsafe { (&raw const (*page.as_ref().state_union.get()).mmu.num_mappings).read_unaligned() }
}

/// # Safety
/// Caller must own `page` or hold the address space lock protecting its MMU state union.
#[inline(always)]
unsafe fn set_page_num_mappings(page: VmPagePtr, count: u32) {
    // SAFETY: Caller owns or holds the lock protecting the MMU page's `mmu.num_mappings` field.
    unsafe {
        (&raw mut (*page.as_ref().state_union.get()).mmu.num_mappings).write_unaligned(count);
    }
}

/// Calculates the parity of the given pte. Return value of true indicates it has odd parity, which
/// is the goal.
#[inline(always)]
const fn compute_parity(pte: u64) -> bool {
    pte.count_ones().is_multiple_of(2)
}

#[inline(always)]
const fn add_parity(mut pte: u64) -> u64 {
    if !compute_parity(pte) {
        pte ^= MMU_PTE_RES_PARITY;
    }
    pte
}

#[inline(always)]
fn check_parity(pte: u64) {
    assert!(compute_parity(pte), "PTE parity error: {pte:#x}");
}

#[inline(always)]
fn load_pte(pte: &Pte, order: Ordering) -> u64 {
    let val = pte.load(order);
    check_parity(val);
    val
}

#[inline(always)]
fn compare_exchange_pte(pte: &Pte, expected: &mut u64, desired: u64, order: Ordering) -> bool {
    *expected = add_parity(*expected);
    let desired = add_parity(desired);
    // `Ordering::Relaxed` is sufficient on failure because callers only inspect the loaded PTE
    // value for parity checking and do not rely on a failed CAS to synchronize dependent memory.
    if let Err(actual) = pte.compare_exchange(*expected, desired, order, Ordering::Relaxed) {
        *expected = actual;
        // If the exchange failed, expected will have been updated and so we should check its
        // parity.
        check_parity(*expected);
        return false;
    }
    true
}

/// Convert user level mmu flags to flags that go in L1 descriptors.
fn mmu_flags_to_s1_pte_attr(flags: ArchMmuFlags) -> u64 {
    let mut attr: u64 = MMU_PTE_ATTR_AF;

    match flags & ARCH_MMU_FLAG_CACHE_MASK {
        ARCH_MMU_FLAG_CACHED => {
            attr |= MMU_PTE_ATTR_NORMAL_MEMORY | MMU_PTE_ATTR_SH_INNER_SHAREABLE;
        }
        ARCH_MMU_FLAG_WRITE_COMBINING => {
            attr |= MMU_PTE_ATTR_NORMAL_UNCACHED | MMU_PTE_ATTR_SH_INNER_SHAREABLE;
        }
        ARCH_MMU_FLAG_UNCACHED => {
            attr |= MMU_PTE_ATTR_STRONGLY_ORDERED;
        }
        ARCH_MMU_FLAG_UNCACHED_DEVICE => {
            attr |= MMU_PTE_ATTR_DEVICE;
        }
        _ => panic!("unexpected flags value {flags:#x}"),
    }

    match flags & (ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_WRITE) {
        0 => attr |= MMU_PTE_ATTR_AP_P_RO_U_NA,
        ARCH_MMU_FLAG_PERM_WRITE => attr |= MMU_PTE_ATTR_AP_P_RW_U_NA,
        ARCH_MMU_FLAG_PERM_USER => attr |= MMU_PTE_ATTR_AP_P_RO_U_RO,
        x if x == (ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_WRITE) => {
            attr |= MMU_PTE_ATTR_AP_P_RW_U_RW;
        }
        _ => unreachable!(),
    }

    if (flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
        if (flags & ARCH_MMU_FLAG_PERM_USER) != 0 {
            // User executable page, marked privileged execute never.
            attr |= MMU_PTE_ATTR_PXN;
        } else {
            // Privileged executable page, marked user execute never.
            attr |= MMU_PTE_ATTR_UXN;
        }
    } else {
        // All non executable pages are marked both privileged and user execute never.
        attr |= MMU_PTE_ATTR_UXN | MMU_PTE_ATTR_PXN;
    }

    if (flags & ARCH_MMU_FLAG_NS) != 0 {
        attr |= MMU_PTE_ATTR_NON_SECURE;
    }

    attr
}

fn s1_pte_attr_to_mmu_flags(pte: u64) -> ArchMmuFlags {
    let mut mmu_flags: ArchMmuFlags = 0;
    match pte & MMU_PTE_ATTR_ATTR_INDEX_MASK {
        MMU_PTE_ATTR_STRONGLY_ORDERED => mmu_flags |= ARCH_MMU_FLAG_UNCACHED,
        MMU_PTE_ATTR_DEVICE => mmu_flags |= ARCH_MMU_FLAG_UNCACHED_DEVICE,
        MMU_PTE_ATTR_NORMAL_UNCACHED => mmu_flags |= ARCH_MMU_FLAG_WRITE_COMBINING,
        MMU_PTE_ATTR_NORMAL_MEMORY => mmu_flags |= ARCH_MMU_FLAG_CACHED,
        _ => panic!("unexpected pte value {pte:x}"),
    }

    mmu_flags |= ARCH_MMU_FLAG_PERM_READ;
    match pte & MMU_PTE_ATTR_AP_MASK {
        MMU_PTE_ATTR_AP_P_RW_U_NA => mmu_flags |= ARCH_MMU_FLAG_PERM_WRITE,
        MMU_PTE_ATTR_AP_P_RW_U_RW => {
            mmu_flags |= ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_WRITE;
        }
        MMU_PTE_ATTR_AP_P_RO_U_NA => {}
        MMU_PTE_ATTR_AP_P_RO_U_RO => mmu_flags |= ARCH_MMU_FLAG_PERM_USER,
        _ => unreachable!(),
    }

    // Based on whether or not this is a user page, check UXN or PXN bit to determine
    // if it's an executable page.
    if (mmu_flags & ARCH_MMU_FLAG_PERM_USER) != 0 {
        if (pte & MMU_PTE_ATTR_UXN) == 0 {
            mmu_flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
        }
    } else if (pte & MMU_PTE_ATTR_PXN) == 0 {
        // Privileged page, check the PXN bit.
        mmu_flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
    }

    // TODO: https://fxbug.dev/42169684
    // Add additional asserts here that the translation table entries are correctly formed
    // with regards to UXN and PXN bits and possibly other unhandled and/or ambiguous bits.

    if (pte & MMU_PTE_ATTR_NON_SECURE) != 0 {
        mmu_flags |= ARCH_MMU_FLAG_NS;
    }

    mmu_flags
}

fn mmu_flags_to_s2_pte_attr(flags: ArchMmuFlags) -> u64 {
    let mut attr: u64 = MMU_PTE_ATTR_AF;

    match flags & ARCH_MMU_FLAG_CACHE_MASK {
        ARCH_MMU_FLAG_CACHED => {
            attr |= MMU_S2_PTE_ATTR_NORMAL_MEMORY | MMU_PTE_ATTR_SH_INNER_SHAREABLE;
        }
        ARCH_MMU_FLAG_WRITE_COMBINING => {
            attr |= MMU_S2_PTE_ATTR_NORMAL_UNCACHED | MMU_PTE_ATTR_SH_INNER_SHAREABLE;
        }
        ARCH_MMU_FLAG_UNCACHED => {
            attr |= MMU_S2_PTE_ATTR_STRONGLY_ORDERED;
        }
        ARCH_MMU_FLAG_UNCACHED_DEVICE => {
            attr |= MMU_S2_PTE_ATTR_DEVICE;
        }
        _ => panic!("unexpected flags value {flags:#x}"),
    }

    if (flags & ARCH_MMU_FLAG_PERM_WRITE) != 0 {
        attr |= MMU_S2_PTE_ATTR_S2AP_RW;
    } else {
        attr |= MMU_S2_PTE_ATTR_S2AP_RO;
    }
    if (flags & ARCH_MMU_FLAG_PERM_EXECUTE) == 0 {
        attr |= MMU_S2_PTE_ATTR_XN;
    }

    attr
}

fn s2_pte_attr_to_mmu_flags(pte: u64) -> ArchMmuFlags {
    let mut mmu_flags: ArchMmuFlags = 0;

    match pte & MMU_S2_PTE_ATTR_ATTR_INDEX_MASK {
        MMU_S2_PTE_ATTR_STRONGLY_ORDERED => mmu_flags |= ARCH_MMU_FLAG_UNCACHED,
        MMU_S2_PTE_ATTR_DEVICE => mmu_flags |= ARCH_MMU_FLAG_UNCACHED_DEVICE,
        MMU_S2_PTE_ATTR_NORMAL_UNCACHED => mmu_flags |= ARCH_MMU_FLAG_WRITE_COMBINING,
        MMU_S2_PTE_ATTR_NORMAL_MEMORY => mmu_flags |= ARCH_MMU_FLAG_CACHED,
        _ => panic!("unexpected pte value {pte:x}"),
    }

    mmu_flags |= ARCH_MMU_FLAG_PERM_READ;
    match pte & MMU_PTE_ATTR_AP_MASK {
        MMU_S2_PTE_ATTR_S2AP_RO => {}
        MMU_S2_PTE_ATTR_S2AP_RW => mmu_flags |= ARCH_MMU_FLAG_PERM_WRITE,
        _ => panic!("unexpected pte value {pte:x}"),
    }

    if (pte & MMU_S2_PTE_ATTR_XN) == 0 {
        mmu_flags |= ARCH_MMU_FLAG_PERM_EXECUTE;
    }

    mmu_flags
}

#[inline(always)]
const fn is_pte_valid(pte: u64) -> bool {
    (pte & MMU_PTE_DESCRIPTOR_MASK) != MMU_PTE_DESCRIPTOR_INVALID
}

#[inline(always)]
fn update_pte(pte: &Pte, newval: u64, order: Ordering) {
    pte.store(add_parity(newval), order);
}

/// Returns a slice over `page_table` whose lifetime is tied to an active RCU read-side guard.
///
/// # Safety
/// `page_table` must be non-null and point to a valid page table allocation of
/// `1 << (page_size_shift - 3)` PTEs kept live by `_reader`.
#[inline(always)]
unsafe fn rcu_page_table_slice<'r>(
    _reader: &'r AutoSimpleGenerationalReader<'_>,
    page_table: *const Pte,
    page_size_shift: u32,
) -> &'r [Pte] {
    assert!(!page_table.is_null());
    let count = 1usize << (page_size_shift - 3);
    // SAFETY: `page_table` is non-null and caller guarantees `count` valid PTEs live for `'r`.
    unsafe { core::slice::from_raw_parts(page_table, count) }
}

/// Converts architectural MMU flags into the corresponding ARM64 address space type.
pub(super) const fn aspace_type_from_flags(mmu_flags: ArchMmuFlags) -> ArmAspaceType {
    // Kernel/Guest flags are mutually exclusive. Ensure at most 1 is set.
    let is_kernel = (mmu_flags & ARCH_ASPACE_FLAG_KERNEL) != 0;
    let is_guest = (mmu_flags & ARCH_ASPACE_FLAG_GUEST) != 0;
    debug_assert!((is_kernel as u8) + (is_guest as u8) <= 1);
    if is_kernel {
        return ArmAspaceType::kKernel;
    }
    if is_guest {
        return ArmAspaceType::kGuest;
    }
    ArmAspaceType::kUser
}

const fn arm_aspace_type_name(aspace_type: ArmAspaceType) -> &'static str {
    match aspace_type {
        ArmAspaceType::kKernel => "kernel",
        ArmAspaceType::kUser => "user",
        ArmAspaceType::kGuest => "guest",
    }
}

/// Returns true if the given physical page address is mapped in the kernel
/// physmap. Non-RAM physical pages (such as reserved firmware runtime regions)
/// are excluded from the physmap even when they fall below gPhysmapSize.
fn is_physmap_page_mapped(pa: u64) -> bool {
    if !is_physmap_phys_addr(PAddr(pa as usize)) {
        return false;
    }
    let mut unused = PAddr(0);
    arm64_mmu_translate(
        paddr_to_physmap(PAddr(pa as usize)),
        &mut unused,
        /*user=*/ false,
        /*write=*/ false,
    )
    .is_ok()
}

/// Helper that tracks a virtual address range for performing MMU operations. This can be used
/// directly, or by as part of a `MappingCursor`.
struct VirtualAddressCursor {
    start_vaddr: usize,
    vaddr: usize,
    vaddr_rel_offset: usize,
    size: usize,
}

impl VirtualAddressCursor {
    const fn new(vaddr: usize, size: usize) -> Self {
        Self { start_vaddr: vaddr, vaddr, vaddr_rel_offset: 0, size }
    }

    /// Sets offset used for `vaddr_rel` and returns true if the cursor lies within that offset and
    /// some specified maximum. Should only be called before the cursor has started to be used.
    fn set_vaddr_relative_offset(&mut self, rel_offset: usize, rel_max: usize) -> bool {
        debug_assert_eq!(self.start_vaddr, self.vaddr);
        let vaddr_rel = self.start_vaddr.wrapping_sub(rel_offset);

        if self.size > rel_max || vaddr_rel > rel_max - self.size {
            return false;
        }
        self.vaddr_rel_offset = rel_offset;
        true
    }

    /// Update the cursor to skip over a not-present page table entry.
    fn skip_entry(&mut self, ps: usize) {
        // Cannot just increase by given size as both the current or final vaddr may not be aligned
        // to this given size. These cases only happen as the very first or very last entry we will
        // examine respectively, but still must be handled here.

        // Calculate the amount the cursor should skip to get to the next entry at
        // this page table level.
        let next_entry_offset = ps - (self.vaddr & (ps - 1));
        // If our endpoint was in the middle of this range, clamp the
        // amount we remove from the cursor
        let consume = if self.size > next_entry_offset { next_entry_offset } else { self.size };

        debug_assert!(self.size >= consume);
        self.size -= consume;
        self.vaddr = self.vaddr.wrapping_add(consume);
    }

    fn consume(&mut self, ps: usize) {
        debug_assert!(self.size >= ps);
        self.vaddr = self.vaddr.wrapping_add(ps);
        self.size -= ps;
    }

    /// Returns a new cursor to the, possibly empty, virtual range that has already been processed
    /// by this cursor. The returned cursor will always be a subset of the original cursors range.
    const fn processed_range(&self) -> Self {
        let mut ret = Self::new(self.start_vaddr, self.vaddr.wrapping_sub(self.start_vaddr));
        // As our new cursor is a subrange we know the relative offset will always be valid.
        ret.vaddr_rel_offset = self.vaddr_rel_offset;
        ret
    }

    const fn vaddr(&self) -> usize {
        self.vaddr
    }

    const fn vaddr_rel(&self) -> usize {
        self.vaddr.wrapping_sub(self.vaddr_rel_offset)
    }

    const fn size(&self) -> usize {
        self.size
    }
}

/// Helper class for MMU implementations to track physical address ranges when installing mappings.
/// If just processing a virtual address range, such as for unmapping, then the
/// `VirtualAddressCursor` can be used instead.
struct MappingCursor<'a> {
    paddrs: &'a [u64],
    paddr_idx: usize,
    /// We have no need to actually track the total number of elements in the paddrs array, as this
    /// should be a simple size/paddr_size. To guard against code mistakes though, we separately
    /// track this just in debug mode.
    #[cfg(debug_assertions)]
    paddr_count: usize,
    paddr_consumed: usize,
    page_size: usize,
    vaddr_cursor: VirtualAddressCursor,
}

impl<'a> MappingCursor<'a> {
    const fn new(paddrs: &'a [u64], page_size: usize, vaddr: usize) -> Self {
        Self {
            paddrs,
            paddr_idx: 0,
            #[cfg(debug_assertions)]
            paddr_count: paddrs.len(),
            paddr_consumed: 0,
            page_size,
            vaddr_cursor: VirtualAddressCursor::new(vaddr, page_size.saturating_mul(paddrs.len())),
        }
    }

    /// See `VirtualAddressCursor::set_vaddr_relative_offset`.
    fn set_vaddr_relative_offset(&mut self, rel_offset: usize, rel_max: usize) -> bool {
        self.vaddr_cursor.set_vaddr_relative_offset(rel_offset, rel_max)
    }

    fn consume(&mut self, ps: usize) {
        self.paddr_consumed += ps;
        debug_assert!(self.paddr_consumed <= self.page_size);
        if self.paddr_consumed == self.page_size {
            self.paddr_idx += 1;
            self.paddr_consumed = 0;
            #[cfg(debug_assertions)]
            {
                debug_assert!(self.paddr_count > 0);
                self.paddr_count -= 1;
            }
        }
        self.vaddr_cursor.consume(ps);
    }

    fn paddr(&self) -> u64 {
        debug_assert!(self.paddr_consumed < self.page_size);
        self.paddrs[self.paddr_idx] + (self.paddr_consumed as u64)
    }

    const fn page_remaining(&self) -> usize {
        self.page_size - self.paddr_consumed
    }

    /// Returns a new cursor to the, possibly empty, virtual range that has already been processed
    /// by this cursor. The returned cursor will always be a subset of the original cursors range
    /// and does not include the paddrs.
    const fn processed_range(&self) -> VirtualAddressCursor {
        self.vaddr_cursor.processed_range()
    }

    const fn vaddr(&self) -> usize {
        self.vaddr_cursor.vaddr()
    }

    const fn vaddr_rel(&self) -> usize {
        self.vaddr_cursor.vaddr_rel()
    }

    const fn size(&self) -> usize {
        self.vaddr_cursor.size()
    }
}

/// Maximum number of TLB entries we will queue before switching to ASID invalidation.
const MAX_PENDING_TLBS: usize = 16;

/// Pending TLBs to flush are stored as 63 bits, with the bottom bit stolen to store the terminal
/// flag. 63 bits is more than enough as these entries are page aligned at the minimum.
#[derive(Copy, Clone, Default)]
#[repr(transparent)]
struct PendingTlbs {
    /// address\[63:1\], terminal\[0\]
    va_terminal: u64,
}

impl PendingTlbs {
    const fn new(va: u64, terminal: bool) -> Self {
        Self { va_terminal: va | (terminal as u64) }
    }

    const fn terminal(self) -> bool {
        (self.va_terminal & 1) != 0
    }

    const fn va(self) -> usize {
        (self.va_terminal & !1u64) as usize
    }
}

zr::static_assert!(core::mem::size_of::<PendingTlbs>() == 8);

/// A consistency manager that tracks TLB updates, walker syncs and free pages in an effort to
/// minimize DSBs (by delaying and coalescing TLB invalidations) and switching to full ASID
/// invalidations if too many TLB invalidations are requested.
/// The aspace lock *must* be held over the full operation of the ConsistencyManager, from
/// construction to deletion. The lock must be held continuously to deletion, and specifically till
/// the actual TLB invalidations occur, due to strategy employed here of only invalidating actual
/// vaddrs with changing entries, and not all vaddrs an operation applies to. Otherwise the
/// following scenario is possible
///  1. Thread 1 performs an Unmap and removes PTE entries, but drops the lock prior to
///     invalidation.
///  2. Thread 2 performs an Unmap, no PTE entries are removed, no invalidations occur
///  3. Thread 2 now believes the resources (pages) for the region are no longer accessible, and
///     returns them to the pmm.
///  4. Thread 3 attempts to access this region and is now able to read/write to returned pages as
///     invalidations have not occurred.
///
/// This scenario is possible as the mappings here are not the source of truth of resource
/// management, but a cache of information from other parts of the system. If thread 2 wanted to
/// guarantee that the pages were free it could issue its own TLB invalidations for the vaddr
/// range, even though it found no entries. However this is not the strategy employed here at the
/// moment.
#[pin_data(PinnedDrop)]
struct ConsistencyManager<'a, 'g> {
    /// The locked aspace we are invalidating TLBs for.
    aspace: &'a LockedAspace<'a, 'g>,

    /// VmPages to release to the PMM after the TLB invalidation occurs.
    #[pin]
    to_free: PageList,

    /// Pending ISB
    isb_pending: bool,

    /// The main list of pending TLBs.
    num_pending_tlbs: usize,
    pending_tlbs: [PendingTlbs; MAX_PENDING_TLBS],
}

impl<'a, 'g> ConsistencyManager<'a, 'g> {
    fn new(aspace: &'a LockedAspace<'a, 'g>) -> impl PinInit<Self> {
        pin_init!(Self {
            aspace,
            to_free <- PageList::new(),
            isb_pending: false,
            num_pending_tlbs: 0,
            pending_tlbs: [PendingTlbs::default(); MAX_PENDING_TLBS],
        })
    }

    fn map_entry(self: Pin<&mut Self>, _va: usize, terminal: bool) {
        // We do not need to sync the walker, despite writing a new entry, as this is a
        // non-terminal entry and so is irrelevant to the walker anyway.
        if !terminal {
            return;
        }

        // If we're mapping in the kernel aspace we may access the page shortly. DSB to make sure
        // the page table walker sees it and ISB to keep the cpu from prefetching through this
        // point. We do not need to do this for user pages since there will be a synchronization
        // event before returning back to user space, or in the case of performing a user_copy
        // after this mapping to the newly mapped page at worst there will be an extraneous page
        // fault.
        let this = self.project();
        if this.aspace.aspace.aspace_type == ArmAspaceType::kKernel {
            dsb_ishst();
            *this.isb_pending = true;
        }
    }

    /// Queue a TLB entry for flushing. This may get turned into a complete ASID flush, or even a
    /// complete TLB (all ASID) flush if the associated aspace is a shared one.
    fn flush_entry(mut self: Pin<&mut Self>, va: usize, terminal: bool) {
        // Check we have queued too many entries already.
        if self.num_pending_tlbs >= MAX_PENDING_TLBS {
            // Most of the time we will now prefer to invalidate the entire ASID/VMID; the
            // exception is if this is the kernel aspace, which uses global mappings rather than a
            // dedicated ASID.
            if self.aspace.aspace.aspace_type != ArmAspaceType::kKernel {
                // Keep counting entries so that we can track how many TLB invalidates we saved by
                // grouping.
                *self.project().num_pending_tlbs += 1;
                return;
            }
            // Flush what pages we've cached up until now and reset counter to zero.
            self.as_mut().flush();
        }

        // va must be page aligned so we can safely throw away the bottom bit.
        debug_assert!(page::is_page_rounded(va));
        debug_assert!(self.aspace.aspace.is_valid_vaddr(va));

        let this = self.project();
        let idx = *this.num_pending_tlbs;
        this.pending_tlbs[idx] = PendingTlbs::new(va as u64, terminal);
        *this.num_pending_tlbs = idx + 1;
    }

    /// Performs any pending synchronization of TLBs and page table walkers. Includes the DSB to
    /// ensure TLB flushes have completed prior to returning to user.
    fn flush(self: Pin<&mut Self>) {
        CM_FLUSH.add(1);
        let this = self.project();

        // Flush any pending ISBs.
        if *this.isb_pending {
            isb();
            *this.isb_pending = false;
        }

        if *this.num_pending_tlbs == 0 {
            return;
        }
        // Need a DSB to synchronize any page table updates prior to flushing the TLBs.
        dsb_ishst();

        // Check if we should just be performing a full ASID invalidation.
        // If the associate aspace is shared, this will be upgraded to a full TLB invalidation
        // across all ASIDs.
        if *this.num_pending_tlbs > MAX_PENDING_TLBS {
            CM_FLUSH_ALL.add(1);
            CM_FLUSH_ALL_REPLACING.add(*this.num_pending_tlbs as i64);
            // If we're a shared aspace, we should be invalidating across all ASIDs.
            if this.aspace.aspace.is_shared() {
                this.aspace.flush_all_asids();
            } else {
                this.aspace.flush_asid();
            }
        } else {
            for entry in &this.pending_tlbs[..*this.num_pending_tlbs] {
                let va = entry.va();
                debug_assert!(this.aspace.aspace.is_valid_vaddr(va));
                this.aspace.flush_tlb_entry(va, entry.terminal());
            }
            CM_SINGLE_TLB_INVALIDATES.add(*this.num_pending_tlbs as i64);
        }

        // DSB to ensure TLB flushes happen prior to returning to user.
        dsb_ish();

        // Local flushes that the kernel may observe prior to Context Synchronization Event
        // should go ahead and get an ISB to force it.
        if this.aspace.aspace.aspace_type == ArmAspaceType::kKernel {
            isb();
        }

        *this.num_pending_tlbs = 0;
    }

    /// Queue a page for freeing that is dependent on TLB flushing. This is for pages that were
    /// previously installed as page tables and should not be reused until the non-terminal TLB
    /// flush has occurred.
    fn free_page(self: Pin<&mut Self>, page: VmPagePtr) {
        // SAFETY: `page` is an unlinked page table page allocated for this aspace and not stored
        // in any other intrusive container.
        unsafe { self.project().to_free.list_mut().push_back_raw(page.as_non_null()) };
    }
}

#[pinned_drop]
impl PinnedDrop for ConsistencyManager<'_, '_> {
    fn drop(mut self: Pin<&mut Self>) {
        self.as_mut().flush();

        if !self.to_free.is_empty() {
            // Any page tables in the to_free list have been disconnected from tt_virt and are no
            // longer discoverable. Synchronize to ensure any previous readers have finished before
            // freeing the pages.

            // TODO(https://fxbug.dev/502004224): Remove or revisit this logging once the
            // referenced issue is better understood.
            let start = current_mono_time();

            self.aspace.aspace.rcu.synchronize();

            let duration = current_mono_time() - start;
            if duration.0 > zx_sec(1) {
                dprintf!(
                    ALWAYS,
                    "[MMU_STALL_WARN] Aspace {:p}: rcu.synchronize() took {} ms \
                     (spinning too long!)\n",
                    self.aspace.aspace,
                    duration.0 / zx_msec(1)
                );
            }

            // SAFETY: `self.flush()` completed all hardware TLB shootdowns and
            // `rcu.synchronize()` waited for concurrent readers, so pages in `self.to_free` are
            // unlinked and exclusively owned here.
            unsafe { cache_free_pages(self.project().to_free) };
        }
    }
}

#[derive(Copy, Clone, PartialEq, Eq)]
enum Reclaim {
    No,
    Yes,
}

/// Used by callers of `unmap_page_table` to indicate whether there might be empty page tables in
/// the tree that need to be checked for. Empty page tables are normally not allowed, but might be
/// there due to cleaning up a failed attempt at mapping.
#[derive(Copy, Clone, PartialEq, Eq)]
enum CheckForEmptyPt {
    No,
    Yes,
}

struct AspaceLockedState {
    /// The number of times entries in the top level page are referenced by other page tables.
    /// Unified page tables increment and decrement this value on their associated shared and
    /// restricted page tables, so we must hold `lock` when doing so.
    num_references: u32,

    /// Whether or not changes to this instance are allowed.
    updates_enabled: bool,
}

/// ARM64 architectural virtual memory address space state.
///
/// Metadata fields stored as atomics (`role`, `asid`, `top_size_shift`, `top_index_shift`,
/// `page_size_shift`, `vaddr_base`, `tt_phys`, `tt_virt`, `tt_page`, `shared_aspace`,
/// `restricted_aspace`, `accessed_since_last_check`, `num_active_cpus`, and `pt_pages`) are
/// initialized during `init*` (or updated under `lock` / on context switch) and read as independent
/// scalar values without publishing non-atomic memory through those fields, so `Ordering::Relaxed`
/// is sufficient on their loads and stores.
#[ksync::guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub(super) struct ArmArchVmAspace {
    /// Canary magic (`"VAAS"`) validated on entry to address space operations.
    canary: fbl::Canary<{ fbl::magic(b"VAAS") }>,

    /// Type of address space.
    aspace_type: ArmAspaceType,

    /// The role this aspace plays in unified aspaces, if any. This should only be set by the
    /// `init*` functions and should not be modified anywhere else.
    role: AtomicU8,

    /// Architectural ASID or VMID assigned to this address space.
    asid: AtomicU16,

    /// Log2 of aspace size.
    top_size_shift: AtomicU32,
    /// Log2 top level shift.
    top_index_shift: AtomicU32,
    /// Log2 of page size.
    page_size_shift: AtomicU32,
    /// Offset that should be applied to address to compute PTE indices.
    vaddr_base: AtomicUsize,

    /// Pointer to the translation table.
    ///
    /// An RCU-like system is used for page table management. Unlike an actual RCU system, the page
    /// tables are not copied when modified, but rather updated live in parallel with readers. The
    /// life cycle management and read-side critical sections are, however, the same and so we can
    /// still use an rcu primitive in the implementation. The protocols are as follows:
    ///
    /// # Readers
    ///
    /// In order for a reader to actually avoid taking the main `lock`, the external caller must
    /// provide a guarantee on object liveness, i.e. that `destroy()` is not going to be called.
    ///
    /// The `rcu` read lock allows the reader to know that any page table it finds from the
    /// `tt_virt` root will not be freed, and will be a 'valid' page table. Valid means that it can
    /// be interpreted as a page table with entries, although it may no longer be installed and
    /// findable by the hardware walker. To achieve this readers must:
    ///  * Assume that the value of any entry in the page table may change due to concurrent
    ///    map/unmap operations.
    ///  * Use load acquire orders when reading next level page table pointers to synchronize with
    ///    writers.
    ///
    /// For the moment the only reader is the `mark_accessed` method, which will attempt to set the
    /// AF flag on entries.
    ///
    /// # Updaters
    ///
    /// To coordinate with parallel readers an update must:
    ///  * Use at least relaxed atomic operations when manipulating page table entries.
    ///  * Use atomic release operations when installing new page tables.
    ///  * Until `synchronize()` is called, treat any page table as if visible to the hardware
    ///    walker and so keep it as a valid page table.
    tt_phys: AtomicU64,
    tt_virt: AtomicPtr<Pte>,

    /// Page backing the root translation table.
    tt_page: AtomicPtr<VmPage>,

    /// Base virtual address of this address space (`[base, base + size)`).
    base: usize,
    /// Byte length of this address space (`[base, base + size)`).
    size: usize,

    /// Pointers to the shared and restricted aspaces referenced by this aspace if it is unified.
    /// These fields are set by `init_unified` and should not be modified anywhere else except
    /// `destroy_unified`.
    shared_aspace: AtomicPtr<ArmArchVmAspace>,
    restricted_aspace: AtomicPtr<ArmArchVmAspace>,

    /// Page allocate function, if set will be used instead of the default allocator.
    test_page_alloc_func: Option<PageAllocFn>,

    /// Whether or not this has been accessed since `accessed_since_last_check` was called. This is
    /// updated by `mark_accessed`, and anywhere a mapping is installed with the AF flag set.
    accessed_since_last_check: AtomicBool,

    /// Number of CPUs this aspace is currently active on.
    num_active_cpus: AtomicU32,

    /// Upper bound of the number of pages allocated to back the translation table.
    pt_pages: AtomicUsize,

    rcu: SimpleGenerational,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,

    #[guarded_by(lock)]
    state: AspaceLockedState,
}

#[pinned_drop]
impl PinnedDrop for ArmArchVmAspace {
    fn drop(self: Pin<&mut Self>) {
        // destroy() will have freed the final page table if it ran correctly, and further
        // validated that everything else was freed.
        debug_assert_eq!(self.pt_pages.load(Ordering::Relaxed), 0);
    }
}

struct LockedAspace<'a, 'g> {
    aspace: &'a ArmArchVmAspace,
    guard: &'a ArmArchVmAspaceLockGuard<'g>,
}

impl<'a, 'g> LockedAspace<'a, 'g> {
    fn new(aspace: &'a ArmArchVmAspace, guard: &'a ArmArchVmAspaceLockGuard<'g>) -> Self {
        Self { aspace, guard }
    }

    /// # Safety
    /// `page_table` must point to a valid page table allocation owned or referenced under
    /// `self.aspace.lock`.
    #[inline(always)]
    unsafe fn page_table_slice(&self, page_table: *const Pte) -> &[Pte] {
        assert!(!page_table.is_null());
        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        let count = 1usize << (page_size_shift - 3);
        // SAFETY: `page_table` is non-null and kept live for the borrow lifetime of `&self` by
        // `self.aspace.lock`.
        unsafe { core::slice::from_raw_parts(page_table, count) }
    }

    /// # Safety
    /// `page_table` must point to a valid page table allocation owned under `self.aspace.lock`.
    unsafe fn first_used_page_table_entry(&self, page_table: *const Pte) -> Option<usize> {
        // SAFETY: Caller guarantees `page_table` is valid under `self.aspace.lock`.
        let slice = unsafe { self.page_table_slice(page_table) };
        for (i, entry) in slice.iter().enumerate() {
            let pte = load_pte(entry, Ordering::Relaxed);
            if pte != MMU_PTE_DESCRIPTOR_INVALID {
                // Although the descriptor isn't exactly the INVALID value, it might have been
                // corrupted and also not a valid entry. Some forms of corruption are
                // indistinguishable from valid entries, so this is really just checking for
                // scenarios where the low type bits got set to INVALID, but the rest of the entry
                // did not.
                //
                // TODO(https://fxbug.dev/42159319): Once https://fxbug.dev/42159319 is resolved
                // this can be removed.
                assert!(
                    is_pte_valid(pte),
                    "page_table at {page_table:p} has malformed invalid entry {pte:#x} at {i}"
                );
                return Some(i);
            }
        }
        None
    }

    fn alloc_page_table(&self) -> Result<VmPagePtr, Status> {
        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        ltracef!("page_size_shift {page_size_shift}\n");

        // currently we only support allocating a single page
        debug_assert_eq!(page_size_shift, page::SHIFT as u32);

        let test_alloc = |paf: PageAllocFn| -> Result<VmPagePtr, Status> {
            let mut page_ptr: *mut VmPage = core::ptr::null_mut();
            let mut paddr: u64 = 0;
            // SAFETY: `paf` is a valid caller-supplied allocator callback and stack out-pointers
            // are valid.
            unsafe { paf(0, &mut page_ptr, &mut paddr) }?;
            // SAFETY: `paf` returned `ZX_OK`, providing a valid non-null `vm_page_t*`.
            unsafe { VmPagePtr::from_raw(page_ptr) }.ok_or(Status::NO_MEMORY)
        };

        // Allocate a page from the pmm via function pointer passed to us in Init().
        // The default is cache_alloc_page so test and explicitly call it to avoid any unnecessary
        // virtual functions.
        let result = match self.aspace.test_page_alloc_func {
            None => cache_alloc_page(),
            Some(paf) => test_alloc(paf),
        };
        if let Ok(page) = result {
            // SAFETY: We just allocated `page` and own its state and `mmu.num_mappings` field.
            unsafe {
                page.set_state(VmPageState(vm_page_state::MMU));
                set_page_num_mappings(page, 0);
            }
            self.aspace.pt_pages.fetch_add(1, Ordering::Relaxed);
            VM_MMU_PAGE_TABLE_ALLOC.add(1);

            local_ktrace!("page table alloc");

            // SAFETY: `page` is valid and owned here.
            ltracef!("allocated {:#x}\n", unsafe { page.paddr() }.0);
        }
        result
    }

    fn free_page_table(
        &self,
        vaddr: *mut Pte,
        page: VmPagePtr,
        cm: Pin<&mut ConsistencyManager<'_, '_>>,
        reclaim: Reclaim,
    ) {
        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        // SAFETY: `page` is a valid MMU page table page owned under `self.aspace.lock`.
        ltracef!(
            "vaddr {vaddr:p} paddr {:#x} page_size_shift {page_size_shift}\n",
            unsafe { page.paddr() }.0
        );

        // currently we only support freeing a single page
        debug_assert_eq!(page_size_shift, page::SHIFT as u32);

        local_ktrace!("page table free");

        debug_assert_eq!(page.state(), VmPageState(vm_page_state::MMU));
        // SAFETY: `self.aspace.lock` protects `page`'s MMU mapping count.
        debug_assert_eq!(unsafe { get_page_num_mappings(page) }, 0);
        cm.free_page(page);

        self.aspace.pt_pages.fetch_sub(1, Ordering::Relaxed);
        VM_MMU_PAGE_TABLE_FREE.add(1);
        if reclaim == Reclaim::Yes {
            VM_MMU_PAGE_TABLE_RECLAIM.add(1);
        }
    }

    /// Splits a descriptor block into a set of next-level-down page blocks/pages.
    ///
    /// `vaddr` is the virtual address of the start of the block being split. `index_shift` is
    /// the index shift of the page table entry of the descriptor block being split.
    /// `page_table` is the page table that contains the descriptor block being split,
    /// and `pt_index` is the index into that table.
    ///
    /// # Safety
    /// `page_table` must point to a valid page table under `self.aspace.lock`, and `pt_index` must
    /// index a block descriptor entry within `page_table`.
    unsafe fn split_large_page(
        &self,
        vaddr: usize,
        index_shift: u32,
        pt_index: usize,
        page_table: *mut Pte,
        mut cm: Pin<&mut ConsistencyManager<'_, '_>>,
    ) -> Result<(), Status> {
        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        debug_assert!(index_shift > page_size_shift);

        // SAFETY: Caller guarantees `page_table` is valid and `pt_index` is in bounds.
        let table = unsafe { self.page_table_slice(page_table) };
        let pte = load_pte(&table[pt_index], Ordering::Relaxed);
        debug_assert_eq!(pte & MMU_PTE_DESCRIPTOR_MASK, MMU_PTE_L012_DESCRIPTOR_BLOCK);

        let page = match self.alloc_page_table() {
            Ok(page) => page,
            Err(status) => {
                tracef!("failed to allocate page table\n");
                return Err(status);
            }
        };

        let next_shift = index_shift - (page_size_shift - 3);

        // SAFETY: `page` is freshly allocated and mapped in physmap; no other reader can see it.
        let page_paddr = unsafe { page.paddr() }.0 as u64;
        let new_page_table_ptr: *mut u64 =
            core::ptr::with_exposed_provenance_mut(paddr_to_physmap(PAddr(page_paddr as usize)).0);
        let new_desc_type = if next_shift == page_size_shift {
            MMU_PTE_L3_DESCRIPTOR_PAGE
        } else {
            MMU_PTE_L012_DESCRIPTOR_BLOCK
        };
        let attrs = (pte & !(MMU_PTE_OUTPUT_ADDR_MASK | MMU_PTE_DESCRIPTOR_MASK)) | new_desc_type;

        let next_size = 1u64 << next_shift;
        let mut mapped_paddr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
        for i in 0..(MMU_KERNEL_PAGE_TABLE_ENTRIES as usize) {
            // directly write to the pte, no need to update since this is
            // a completely new table
            // SAFETY: `new_page_table_ptr` points to a privately owned newly allocated page.
            unsafe { *new_page_table_ptr.add(i) = add_parity(mapped_paddr | attrs) };
            mapped_paddr += next_size;
        }
        // SAFETY: `page` is owned here under `self.aspace.lock`.
        unsafe { set_page_num_mappings(page, MMU_KERNEL_PAGE_TABLE_ENTRIES as u32) };

        if ALLOW_BBM.load(Ordering::Relaxed) {
            // As we are changing the block size of a translation we must do a break-before-make in
            // accordance with ARM requirements to avoid TLB and other inconsistency.
            update_pte(&table[pt_index], MMU_PTE_DESCRIPTOR_INVALID, Ordering::Relaxed);
            cm.as_mut().flush_entry(vaddr, true);
            // Must force the flush to happen now before installing the new entry. This will also
            // ensure the page table entries we wrote will be visible before we install it.
            cm.as_mut().flush();
        } else {
            dsb_ishst();
        }

        let new_pte = page_paddr | MMU_PTE_L012_DESCRIPTOR_TABLE;
        // Release ordering ensures the initialized entries in `new_page_table_ptr` are visible to
        // lockless RCU readers that load this table descriptor with `Ordering::Acquire` in
        // `mark_accessed_page_table`.
        update_pte(&table[pt_index], new_pte, Ordering::Release);
        ltracef!(
            "pte {page_table:p}[{pt_index:#x}] = {:#x}\n",
            load_pte(&table[pt_index], Ordering::Relaxed)
        );

        // no need to update the page table count here since we're replacing a block entry with a
        // table entry.

        cm.flush_entry(vaddr, false);

        Ok(())
    }

    fn flush_tlb_entry_for_all_asids(&self, vaddr: usize, terminal: bool) {
        let va_arg = ((vaddr as u64) >> 12) & TLBI_VADDR_MASK;
        if terminal {
            arm64_tlbi!(vaale1is, va_arg);
        } else {
            arm64_tlbi!(vaae1is, va_arg);
        }
    }

    /// use the appropriate TLB flush instruction to globally flush the modified entry
    /// terminal is set when flushing at the final level of the page table.
    fn flush_tlb_entry(&self, vaddr: usize, terminal: bool) {
        let asid = self.aspace.asid.load(Ordering::Relaxed);
        match self.aspace.aspace_type {
            ArmAspaceType::kUser => {
                if self.aspace.is_shared() {
                    // If this is a shared aspace, we need to flush this address for all ASIDs.
                    self.flush_tlb_entry_for_all_asids(vaddr, terminal);
                } else {
                    // Otherwise, flush this address for the specific ASID.
                    let arg = (((vaddr as u64) >> 12) & TLBI_VADDR_MASK) | ((asid as u64) << 48);
                    if terminal {
                        arm64_tlbi!(vale1is, arg);
                    } else {
                        arm64_tlbi!(vae1is, arg);
                    }
                }
            }
            ArmAspaceType::kKernel => {
                debug_assert_eq!(asid, MMU_ARM64_GLOBAL_ASID);
                self.flush_tlb_entry_for_all_asids(vaddr, terminal);
            }
            ArmAspaceType::kGuest => {
                let vttbr = arm64_vttbr(asid, self.aspace.tt_phys.load(Ordering::Relaxed));
                // SAFETY: `vttbr` encodes the valid VMID and stage-2 table address for this guest.
                let status = unsafe { bindings::arm64_el2_tlbi_ipa(vttbr, vaddr, terminal) };
                debug_assert_eq!(status, zx_status::sys::ZX_OK);
            }
        }
    }

    fn flush_all_asids(&self) {
        debug_assert_eq!(self.aspace.aspace_type, ArmAspaceType::kUser);
        debug_assert!(self.aspace.is_shared());
        arm64_tlbi_noaddr!(vmalle1is);
    }

    fn flush_asid(&self) {
        let asid = self.aspace.asid.load(Ordering::Relaxed);
        match self.aspace.aspace_type {
            ArmAspaceType::kUser => {
                debug_assert_ne!(asid, MMU_ARM64_GLOBAL_ASID);
                arm64_tlbi_asid!(aside1is, asid);
            }
            ArmAspaceType::kKernel => {
                // The alle1is instruction that invalidates TLBs for all ASIDs is only available
                // in EL2, and not EL1.
                panic!("FlushAsid not available for kernel address space");
            }
            ArmAspaceType::kGuest => {
                let vttbr = arm64_vttbr(asid, self.aspace.tt_phys.load(Ordering::Relaxed));
                // SAFETY: `vttbr` encodes this guest aspace's VMID and root Stage-2 table physical
                // address.
                let status = unsafe { bindings::arm64_el2_tlbi_vmid(vttbr) };
                debug_assert_eq!(status, zx_status::sys::ZX_OK);
            }
        }
    }

    /// Unmaps the range of `cursor` from `page_table` and returns both a status, as well as how
    /// many mappings were removed in `page_table`. If the removed mapping count is non-zero,
    /// regardless of the error value, the num_mappings field in the page must be updated by the
    /// caller.
    ///
    /// # Safety
    /// `page_table` must point to a valid page table allocation protected by `self.aspace.lock`.
    unsafe fn unmap_page_table(
        &self,
        cursor: &mut VirtualAddressCursor,
        unmap_options: ArchUnmapOptions,
        pt_check: CheckForEmptyPt,
        index_shift: u32,
        page_table: *mut Pte,
        mut cm: Pin<&mut ConsistencyManager<'_, '_>>,
        reclaim: Reclaim,
    ) -> (Result<(), Status>, u32) {
        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        let top_index_shift = self.aspace.top_index_shift.load(Ordering::Relaxed);
        let block_size = 1usize << index_shift;
        let num_entries = 1usize << (page_size_shift - 3);
        let index_mask = num_entries - 1;
        let mut index = (cursor.vaddr_rel() >> index_shift) & index_mask;
        let mut unmapped: u32 = 0;

        // SAFETY: Caller guarantees `page_table` points to a valid page table of `num_entries`.
        let table = unsafe { self.page_table_slice(page_table) };

        while index != num_entries && cursor.size() != 0 {
            let mut pte = load_pte(&table[index], Ordering::Relaxed);

            if (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_DESCRIPTOR_INVALID {
                cursor.skip_entry(block_size);
                index += 1;
                continue;
            }

            // Check if this is a large page and we need to split it.
            if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_BLOCK
                && (!is_rounded(cursor.vaddr_rel(), block_size) || cursor.size() < block_size)
            {
                // Splitting a large page may perform break-before-make, and during that window we
                // will have temporarily unmapped beyond our range, so ensure we are permitted to
                // do that.
                if !ALLOW_BBM.load(Ordering::Relaxed)
                    && (unmap_options & ARCH_UNMAP_OPTION_ENLARGE) == 0
                {
                    return (Err(Status::NOT_SUPPORTED), unmapped);
                }
                // SAFETY: `page_table` and `index` name a valid block descriptor under `lock`.
                if let Err(s) = unsafe {
                    self.split_large_page(
                        cursor.vaddr(),
                        index_shift,
                        index,
                        page_table,
                        cm.as_mut(),
                    )
                } {
                    // If split fails, just unmap the whole thing, and let a
                    // subsequent page fault clean it up.
                    if unmap_options == ARCH_UNMAP_OPTION_NONE {
                        return (Err(s), unmapped);
                    }
                    // We must unmap here, and not in the normal block below, so that we can use
                    // skip_entry instead of consume on the cursor. This is necessary since the
                    // range we must unmap is, by definition, larger than our target cursor, and it
                    // would be an error to call consume.
                    update_pte(&table[index], MMU_PTE_DESCRIPTOR_INVALID, Ordering::Relaxed);
                    unmapped += 1;

                    cm.as_mut().flush_entry(cursor.vaddr(), true);
                    cursor.skip_entry(block_size);
                    index += 1;
                    continue;
                }
                pte = load_pte(&table[index], Ordering::Relaxed);
            }

            if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_TABLE
            {
                let page_table_paddr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                let next_page_table = paddr_to_pte_ptr(page_table_paddr);

                // Recurse a level but remember where we are unmapping from in case we need to do a
                // second pass to remove a PT.
                let unmap_vaddr = cursor.vaddr();
                // SAFETY: `next_page_table` is a valid lower-level page table protected by `lock`.
                let (status, lower_unmapped) = unsafe {
                    self.unmap_page_table(
                        cursor,
                        unmap_options,
                        pt_check,
                        index_shift - (page_size_shift - 3),
                        next_page_table,
                        cm.as_mut(),
                        reclaim,
                    )
                };
                let mut unmap_lower = false;
                // Regardless of success or failure we must update the mapping count. Since this
                // involves looking up the VmPage we take this opportunity to check if it's empty
                // and needs unmapping.
                let mut lower_page: Option<VmPagePtr> = None;
                if lower_unmapped > 0 || pt_check == CheckForEmptyPt::Yes {
                    let lp = pmm::paddr_to_vm_page(PAddr(page_table_paddr as usize))
                        .expect("page table paddr must have VmPage");
                    // SAFETY: `self.aspace.lock` protects `lp`'s MMU mapping count.
                    unsafe {
                        let cur = get_page_num_mappings(lp);
                        debug_assert!(cur >= lower_unmapped);
                        let new_count = cur - lower_unmapped;
                        set_page_num_mappings(lp, new_count);
                        unmap_lower = new_count == 0;
                    }
                    lower_page = Some(lp);
                }
                if let Err(err) = status {
                    return (Err(err), unmapped);
                }

                // If the unmap made the lower level empty we want to free the page table, unless
                // in the top page of an aspace with a prepopulated top page.
                if unmap_lower && !(self.aspace.is_shared() && index_shift == top_index_shift) {
                    ltracef!(
                        "pte {:p}[{index:#x}] = 0 (was page table phys {page_table_paddr:#x})\n",
                        page_table
                    );
                    update_pte(&table[index], MMU_PTE_DESCRIPTOR_INVALID, Ordering::Relaxed);
                    unmapped += 1;

                    // We can safely defer TLB flushing as the consistency manager will not return
                    // the backing page to the PMM until after the tlb is flushed.
                    cm.as_mut().flush_entry(unmap_vaddr, false);
                    self.free_page_table(
                        next_page_table,
                        lower_page.unwrap(),
                        cm.as_mut(),
                        reclaim,
                    );
                }
            } else {
                // Empty entries were already handled and skipped at the top of the loop
                debug_assert!(is_pte_valid(pte));
                ltracef!(
                    "pte {page_table:p}[{index:#x}] = 0 (was phys {:#x})\n",
                    pte & MMU_PTE_OUTPUT_ADDR_MASK
                );
                update_pte(&table[index], MMU_PTE_DESCRIPTOR_INVALID, Ordering::Relaxed);

                if (unmap_options & ARCH_UNMAP_OPTION_HARVEST) != 0 {
                    let pte_addr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                    let paddr = pte_addr + ((cursor.vaddr_rel() & (block_size - 1)) as u64);
                    if let Some(page) = pmm::paddr_to_vm_page(PAddr(paddr as usize))
                        && (pte & MMU_PTE_ATTR_AF) != 0
                    {
                        // SAFETY: `page` is a valid `VmPagePtr` returned by `paddr_to_vm_page`.
                        unsafe { pmm::page_queues().mark_accessed(page) };
                    }
                }

                unmapped += 1;
                cm.as_mut().flush_entry(cursor.vaddr(), true);
                cursor.consume(block_size);
            }

            index += 1;
        }

        (Ok(()), unmapped)
    }

    /// Returns both a status, as well as how many new mappings were installed in `page_table`. If
    /// the new mapping count is non-zero, regardless of the error value, the num_mappings field in
    /// the page must be updated by the caller.
    ///
    /// # Safety
    /// `page_table` must point to a valid page table allocation protected by `self.aspace.lock`.
    unsafe fn map_page_table(
        &self,
        attrs: u64,
        ro: bool,
        index_shift: u32,
        page_table: *mut Pte,
        existing_action: ExistingEntryAction,
        cursor: &mut MappingCursor<'_>,
        mut cm: Pin<&mut ConsistencyManager<'_, '_>>,
    ) -> (Result<(), Status>, u32) {
        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        let block_size = 1usize << index_shift;
        let num_entries = 1usize << (page_size_shift - 3);
        let index_mask = num_entries - 1;
        let mut index = (cursor.vaddr_rel() >> index_shift) & index_mask;
        let mut mapped: u32 = 0;

        // SAFETY: Caller guarantees `page_table` points to a valid page table of `num_entries`.
        let table = unsafe { self.page_table_slice(page_table) };

        while index != num_entries && cursor.size() != 0 {
            let mut pte = load_pte(&table[index], Ordering::Relaxed);

            // if we're at an unaligned address, not trying to map a block, and not at the terminal
            // level, recurse one more level of the page table tree
            let level_valigned = is_rounded(cursor.vaddr_rel(), block_size);
            let level_paligned = is_rounded(cursor.paddr() as usize, block_size);
            if !level_valigned
                || !level_paligned
                || cursor.page_remaining() < block_size
                || index_shift > MMU_PTE_DESCRIPTOR_BLOCK_MAX_SHIFT
            {
                // Lookup the next level page table, allocating if required.
                let (page_table_paddr, next_page_table): (u64, *mut Pte) =
                    match pte & MMU_PTE_DESCRIPTOR_MASK {
                        MMU_PTE_DESCRIPTOR_INVALID => {
                            let page = match self.alloc_page_table() {
                                Ok(page) => page,
                                Err(status) => {
                                    tracef!("failed to allocate page table\n");
                                    // The mapping wasn't fully updated, yet work here may need to
                                    // be undone if lower-level page tables were allocated.
                                    // Consuming a single page makes cleanup treat this range as
                                    // modified and check the page table for cleanup.
                                    cursor.consume(page::SIZE);
                                    return (Err(status), mapped);
                                }
                            };
                            // SAFETY: `page` was just allocated for this page table.
                            let page_table_paddr = unsafe { page.paddr() }.0 as u64;

                            let pt_vaddr = paddr_to_pte_ptr(page_table_paddr);

                            ltracef!(
                                "allocated page table, vaddr {pt_vaddr:p}, paddr {:#x}\n",
                                page_table_paddr
                            );
                            // Odd parity means that the 0 pte entry has correct parity, so assert
                            // that here, as it allows us to just zero the page.
                            zr::static_assert!(add_parity(0) == 0);
                            // SAFETY: `pt_vaddr` is page-aligned in the physmap of size
                            // `page::SIZE`.
                            unsafe { arch_zero_page(pt_vaddr.cast()) };

                            // ensure that the zeroing is observable from hardware page table
                            // walkers, as we need to do this prior to writing the pte we cannot
                            // defer it using the consistency manager.
                            dsb_ishst();

                            // When new pages are mapped they have their AF set, assuming they are
                            // mapped due to being accessed, avoiding an accessed fault. Since new
                            // terminal mappings start with AF set, we also start non-terminal
                            // mappings with AF set.
                            pte = page_table_paddr
                                | MMU_PTE_L012_DESCRIPTOR_TABLE
                                | MMU_PTE_ATTR_RES_SOFTWARE_AF;
                            // To ensure any rcu readers see the zeroed page table use a
                            // store-release here, paired with the load-acquire in the reader path.
                            // This is technically redundant since the dsb above also synchronized
                            // with readers, so the explicit release here is added for clarity.
                            update_pte(&table[index], pte, Ordering::Release);
                            mapped += 1;

                            // Tell the consistency manager that we've mapped an inner node.
                            cm.as_mut().map_entry(cursor.vaddr(), false);

                            ltracef!("pte {page_table:p}[{index}] = {pte:#x}\n");
                            (page_table_paddr, pt_vaddr)
                        }
                        MMU_PTE_L012_DESCRIPTOR_TABLE => {
                            // Similar to creating a page table, if we map a page lower down in
                            // this hierarchy it will start off as accessed, so set the accessed
                            // flag on the way down.
                            pte |= MMU_PTE_ATTR_RES_SOFTWARE_AF;
                            update_pte(&table[index], pte, Ordering::Relaxed);
                            let page_table_paddr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                            ltracef!("found page table {page_table_paddr:#x}\n");
                            (page_table_paddr, paddr_to_pte_ptr(page_table_paddr))
                        }
                        MMU_PTE_L012_DESCRIPTOR_BLOCK => {
                            return (Err(Status::ALREADY_EXISTS), mapped);
                        }
                        _ => panic!("unexpected pte value {pte:x}"),
                    };
                debug_assert!(!next_page_table.is_null());

                // SAFETY: `next_page_table` points to a valid child page table under `lock`.
                let (ret, lower_mapped) = unsafe {
                    self.map_page_table(
                        attrs,
                        ro,
                        index_shift - (page_size_shift - 3),
                        next_page_table,
                        existing_action,
                        cursor,
                        cm.as_mut(),
                    )
                };
                // Regardless of success or failure we must update the mapping counts.
                if lower_mapped > 0 {
                    let lower_page = pmm::paddr_to_vm_page(PAddr(page_table_paddr as usize));
                    debug_assert!(lower_page.is_some());
                    let lp = lower_page.unwrap();
                    // SAFETY: `self.aspace.lock` protects `lp`'s MMU mapping count.
                    unsafe {
                        set_page_num_mappings(lp, get_page_num_mappings(lp) + lower_mapped);
                    }
                }
                if let Err(err) = ret {
                    return (Err(err), mapped);
                }
            } else {
                let mut new_pte = cursor.paddr() | attrs;
                if index_shift > page_size_shift {
                    new_pte |= MMU_PTE_L012_DESCRIPTOR_BLOCK;
                } else {
                    new_pte |= MMU_PTE_L3_DESCRIPTOR_PAGE;
                }

                let valid = is_pte_valid(pte);
                if valid && existing_action == ExistingEntryAction::Error {
                    return (Err(Status::ALREADY_EXISTS), mapped);
                } else if valid && existing_action == ExistingEntryAction::Skip {
                    // Empty case to simplify the other branches.
                } else if valid
                    && existing_action == ExistingEntryAction::Upgrade
                    && (pte & MMU_PTE_OUTPUT_ADDR_MASK) == cursor.paddr()
                {
                    // Doing an upgrade of an existing entry where the output address is not
                    // changing. This is just a protect, which we can skip if either nothing is
                    // actually changing, or if we would potentially be reducing permissions.
                    if !ro && new_pte != pte {
                        update_pte(&table[index], new_pte, Ordering::Relaxed);
                        cm.as_mut().flush_entry(cursor.vaddr(), true);
                    }
                } else {
                    if !valid {
                        // As we are going to transition an entry from INVALID->VALID we must count
                        // this additional mapping. All other cases change VALID->VALID.
                        mapped += 1;
                    }
                    // Either no current entry, or we need to upgrade the existing one, potentially
                    // performing a break-before-make.
                    if valid && !ro {
                        // If the output address were not changing we would have hit the protect
                        // case above, so if the new entry is not read only then we must perform
                        // break-before-make before installing it. Failing to do this could result
                        // in writes being temporarily lost due to the different output addresses
                        // and so we must ignore the ALLOW_BBM flag.
                        update_pte(&table[index], MMU_PTE_DESCRIPTOR_INVALID, Ordering::Relaxed);
                        cm.as_mut().flush_entry(cursor.vaddr(), true);
                        // Must force the flush to happen now before installing the new entry. This
                        // will also ensure the page table entries we wrote will be visible before
                        // we install it.
                        cm.as_mut().flush();
                    }
                    ltracef!(
                        "pte {page_table:p}[{index}] = {pte:#x} (paddr {:#x})\n",
                        cursor.paddr()
                    );
                    update_pte(&table[index], new_pte, Ordering::Relaxed);

                    // Tell the consistency manager we've mapped a new page.
                    cm.as_mut().map_entry(cursor.vaddr(), true);
                }
                cursor.consume(block_size);
            }

            index += 1;
        }

        (Ok(()), mapped)
    }

    /// # Safety
    /// `page_table` must point to a valid page table allocation protected by `self.aspace.lock`.
    unsafe fn protect_page_table(
        &self,
        vaddr_in: usize,
        vaddr_rel_in: usize,
        size_in: usize,
        attrs: u64,
        enlarge: ArchUnmapOptions,
        index_shift: u32,
        page_table: *mut Pte,
        mut cm: Pin<&mut ConsistencyManager<'_, '_>>,
    ) -> Result<(), Status> {
        let mut vaddr = vaddr_in;
        let mut vaddr_rel = vaddr_rel_in;
        let mut size = size_in;

        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        let block_size = 1usize << index_shift;
        let block_mask = block_size - 1;

        ltracef!(
            "vaddr {vaddr:#x}, vaddr_rel {vaddr_rel:#x}, size {size:#x}, attrs {attrs:#x}, \
             index shift {index_shift}, page_size_shift {page_size_shift}, \
             page_table {page_table:p}\n"
        );

        // vaddr_rel and size must be page aligned
        debug_assert_eq!((vaddr_rel | size) & ((1usize << page_size_shift) - 1), 0);

        // SAFETY: Caller guarantees `page_table` points to a valid page table under `lock`.
        let table = unsafe { self.page_table_slice(page_table) };

        while size != 0 {
            let vaddr_rem = vaddr_rel & block_mask;
            let chunk_size = core::cmp::min(size, block_size - vaddr_rem);
            let index = vaddr_rel >> index_shift;

            let mut pte = load_pte(&table[index], Ordering::Relaxed);

            // If the input range partially covers a large page, split the page.
            if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_BLOCK
                && chunk_size != block_size
            {
                // Splitting a large page may perform break-before-make, and during that window we
                // will have temporarily unmapped beyond our range, so make sure that is permitted.
                if !ALLOW_BBM.load(Ordering::Relaxed) && (enlarge & ARCH_UNMAP_OPTION_ENLARGE) == 0
                {
                    return Err(Status::NOT_SUPPORTED);
                }
                // SAFETY: `page_table` and `index` name a valid block descriptor under `lock`.
                unsafe {
                    self.split_large_page(vaddr, index_shift, index, page_table, cm.as_mut())?;
                }
                pte = load_pte(&table[index], Ordering::Relaxed);
            }

            if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_TABLE
            {
                let page_table_paddr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                let next_page_table = paddr_to_pte_ptr(page_table_paddr);

                // Recurse a level.
                // SAFETY: `next_page_table` points to a valid child page table under `lock`.
                unsafe {
                    self.protect_page_table(
                        vaddr,
                        vaddr_rem,
                        chunk_size,
                        attrs,
                        enlarge,
                        index_shift - (page_size_shift - 3),
                        next_page_table,
                        cm.as_mut(),
                    )?;
                }
            } else if is_pte_valid(pte) {
                let new_pte = (pte & !MMU_PTE_PERMISSION_MASK) | attrs;
                ltracef!("pte {page_table:p}[{index:#x}] = {new_pte:#x} was {pte:#x}\n");
                // Skip updating the page table entry if the new value is the same as before.
                if new_pte != pte {
                    update_pte(&table[index], new_pte, Ordering::Relaxed);
                    cm.as_mut().flush_entry(vaddr, true);
                }
            } else {
                ltracef!("page table entry does not exist, index {index:#x}, {pte:#x}\n");
            }
            vaddr = vaddr.wrapping_add(chunk_size);
            vaddr_rel += chunk_size;
            size -= chunk_size;
        }
        Ok(())
    }

    /// # Safety
    /// `page_table` must point to a valid page table allocation protected by `self.aspace.lock`.
    unsafe fn harvest_accessed_page_table(
        &self,
        entry_limit: &mut usize,
        mut vaddr: usize,
        vaddr_rel_in: usize,
        mut size: usize,
        index_shift: u32,
        non_terminal_action: NonTerminalAction,
        terminal_action: TerminalAction,
        page_table: *mut Pte,
        mut cm: Pin<&mut ConsistencyManager<'_, '_>>,
    ) -> usize {
        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        let top_index_shift = self.aspace.top_index_shift.load(Ordering::Relaxed);
        let top_size_shift = self.aspace.top_size_shift.load(Ordering::Relaxed);
        let vaddr_base = self.aspace.vaddr_base.load(Ordering::Relaxed);
        let block_size = 1usize << index_shift;
        let block_mask = block_size - 1;

        // We need to retain top level page table entries and their accessed information in shared
        // aspaces. Since the top level page tables are pre-allocated and the corresponding top
        // level entries copied into unified aspaces, these page tables can never be unmapped,
        // because the copied entries cannot change.
        //
        // Because of this, we cannot clear the AF bit at the top level too, otherwise we lose
        // information about whether we should recurse further down to potentially reclaim lower
        // level page tables in the next harvest. (Unmapping would have taken care of recursion if
        // we could unmap.) The alternative to this would be to always recurse down irrespective of
        // whether the AF bit is set. Experiments show that this is wasteful as majority of the top
        // level page tables remain unaccessed. Instead we choose to have the AF bit sticky at the
        // top level in shared aspaces.
        let retain_shared_entries = index_shift == top_index_shift && self.aspace.is_shared();

        let mut vaddr_rel = vaddr_rel_in;

        // vaddr_rel and size must be page aligned
        debug_assert_eq!((vaddr_rel | size) & ((1usize << page_size_shift) - 1), 0);

        let mut harvested_size = 0usize;

        let table_page = pmm::paddr_to_vm_page(physmap_to_paddr(VAddr(page_table.addr())))
            .expect("page table physmap address must have VmPage");
        // SAFETY: Caller guarantees `page_table` points to a valid page table under `lock`.
        let table = unsafe { self.page_table_slice(page_table) };

        while size > 0 && *entry_limit > 0 {
            let _trace = crate::ktrace_rs::begin_scope_cond!(
                LOCAL_KTRACE_ENABLE,
                "kernel:vm",
                "page_table_loop"
            );

            let vaddr_rem = vaddr_rel & block_mask;
            let index = vaddr_rel >> index_shift;

            let mut chunk_size = core::cmp::min(size, block_size - vaddr_rem);

            let mut pte = load_pte(&table[index], Ordering::Relaxed);

            if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_BLOCK
                && chunk_size != block_size
            {
                // Ignore large pages, we do not support harvesting accessed bits from them. Having
                // this empty if block simplifies the overall logic.
            } else if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_TABLE
            {
                let page_table_paddr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                let pt_pa = PAddr(page_table_paddr as usize);
                debug_assert!(
                    page_table_paddr != 0 && is_physmap_phys_addr(pt_pa),
                    "invalid pte {pte:#x} found in page_table {page_table:p} \
                     at index {index}. updates_enabled {}",
                    self.guard.state().updates_enabled
                );
                let next_page_table = paddr_to_pte_ptr(page_table_paddr);

                // Start with the assumption that we will unmap if we can.
                let mut do_unmap = non_terminal_action == NonTerminalAction::FreeUnaccessed;
                // Check for our emulated non-terminal AF so we can potentially skip the recursion.
                // TODO: make this optional when hardware AF is supported (see todo on
                // MMU_PTE_ATTR_RES_SOFTWARE_AF for details)
                let should_recurse = (pte & MMU_PTE_ATTR_RES_SOFTWARE_AF) != 0;
                let mut lower_page: Option<VmPagePtr> = None;
                if should_recurse {
                    // SAFETY: `next_page_table` is a valid child page table under `lock`.
                    chunk_size = unsafe {
                        self.harvest_accessed_page_table(
                            entry_limit,
                            vaddr,
                            vaddr_rem,
                            chunk_size,
                            index_shift - (page_size_shift - 3),
                            non_terminal_action,
                            terminal_action,
                            next_page_table,
                            cm.as_mut(),
                        )
                    };
                    // This was accessed so we don't necessarily want to unmap it, unless our
                    // recursive call caused the page table to be empty, in which case we are
                    // obligated to.
                    let lp =
                        pmm::paddr_to_vm_page(pt_pa).expect("page table paddr must have VmPage");

                    // SAFETY: `self.aspace.lock` protects `lp`'s MMU mapping count.
                    do_unmap = unsafe { get_page_num_mappings(lp) } == 0;
                    lower_page = Some(lp);

                    // We cannot clear the AF bit for top level page table entries in shared
                    // aspaces.
                    if !retain_shared_entries {
                        // If we processed till the end of sub page table, and we are not retaining
                        // page tables, then we can clear the AF as we know we will not have to
                        // process entries from this one again.
                        if !do_unmap
                            && ((vaddr_rel + chunk_size) >> index_shift) != index
                            && non_terminal_action != NonTerminalAction::Retain
                        {
                            pte &= !MMU_PTE_ATTR_RES_SOFTWARE_AF;
                            update_pte(&table[index], pte, Ordering::Relaxed);
                        }
                    }
                }
                // We can't unmap any top level page table entries in an address space with a
                // prepopulated top level page.
                if retain_shared_entries {
                    do_unmap = false;
                }
                if do_unmap {
                    // Unmapping an exact block, which should not need enlarging and hence should
                    // never be able to fail.
                    let mut unmap_cursor = VirtualAddressCursor::new(vaddr, chunk_size);
                    let ok = unmap_cursor
                        .set_vaddr_relative_offset(vaddr_base, 1usize << top_size_shift);
                    // This should never fail as the cursor we are building is a subset of the
                    // range we have already processed, which by definition must have been valid.
                    debug_assert!(ok);

                    // SAFETY: `next_page_table` points to a valid child page table under `lock`.
                    let (result, lower_unmapped) = unsafe {
                        self.unmap_page_table(
                            &mut unmap_cursor,
                            ARCH_UNMAP_OPTION_NONE,
                            CheckForEmptyPt::No,
                            index_shift - (page_size_shift - 3),
                            next_page_table,
                            cm.as_mut(),
                            Reclaim::Yes,
                        )
                    };
                    assert!(result.is_ok());
                    let lp = lower_page.unwrap_or_else(|| {
                        pmm::paddr_to_vm_page(pt_pa).expect("page table paddr must have VmPage")
                    });
                    // SAFETY: `self.aspace.lock` protects both `lp` and `table_page` counts.
                    unsafe {
                        let cur = get_page_num_mappings(lp);
                        debug_assert_eq!(cur, lower_unmapped);
                        set_page_num_mappings(lp, cur - lower_unmapped);
                        update_pte(&table[index], MMU_PTE_DESCRIPTOR_INVALID, Ordering::Relaxed);
                        set_page_num_mappings(table_page, get_page_num_mappings(table_page) - 1);
                    }

                    // We can safely defer TLB flushing as the consistency manager will not return
                    // the backing page to the PMM until after the tlb is flushed.
                    cm.as_mut().flush_entry(vaddr, false);
                    self.free_page_table(next_page_table, lp, cm.as_mut(), Reclaim::Yes);
                }
            } else if is_pte_valid(pte) && (pte & MMU_PTE_ATTR_AF) != 0 {
                let pte_addr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                let paddr = pte_addr + (vaddr_rem as u64);

                // Mappings for physical VMOs do not have pages associated with them and so there's
                // no state to update on an access.
                if let Some(page) = pmm::paddr_to_vm_page(PAddr(paddr as usize)) {
                    // SAFETY: `page` is a valid `VmPagePtr` returned by `paddr_to_vm_page`.
                    unsafe { pmm::page_queues().mark_accessed(page) };

                    if terminal_action == TerminalAction::UpdateAgeAndHarvest {
                        // Modifying the access flag does not require break-before-make for
                        // correctness and as we do not support hardware access flag setting at the
                        // moment we do not have to deal with potential concurrent modifications.
                        pte &= !MMU_PTE_ATTR_AF;
                        ltracef!("pte {page_table:p}[{index:#x}] = {pte:#x}\n");
                        update_pte(&table[index], pte, Ordering::Relaxed);

                        cm.as_mut().flush_entry(vaddr, true);
                    }
                }
            }
            vaddr = vaddr.wrapping_add(chunk_size);
            vaddr_rel += chunk_size;
            size -= chunk_size;

            harvested_size += chunk_size;

            // Each iteration of this loop examines a PTE at the current level. The
            // total number of PTEs examined is limited to avoid holding the aspace lock
            // for too long. However, the remaining limit balance is updated at the end
            // of the loop to ensure that harvesting makes progress, even if the initial
            // limit is too small to reach a terminal PTE.
            if *entry_limit > 1 {
                *entry_limit -= 1;
            } else if !self.aspace.lock.raw_mutex().is_contested() {
                // The entry_limit is either about to be, or already is, 0, but since the lock is
                // not contended we can reset the counter and perform another block of work before
                // checking again.
                *entry_limit = HARVEST_ENTRIES_BETWEEN_UNLOCKS;
            } else {
                // This either changes entry_limit from 1->0, or is a no-op if it was already 0. As
                // the lock is contested this ensures we break out to the parent scope where the
                // lock can be dropped.
                *entry_limit = 0;
            }
        }

        harvested_size
    }

    fn protect_pages(
        &self,
        vaddr: usize,
        size: usize,
        attrs: u64,
        enlarge: ArchUnmapOptions,
        vaddr_base: usize,
        cm: Pin<&mut ConsistencyManager<'_, '_>>,
    ) -> Result<(), Status> {
        let vaddr_rel = vaddr.wrapping_sub(vaddr_base);
        let vaddr_rel_max = 1usize << self.aspace.top_size_shift.load(Ordering::Relaxed);
        let asid = self.aspace.asid.load(Ordering::Relaxed);

        ltracef!("vaddr {vaddr:#x}, size {size:#x}, attrs {attrs:#x}, asid {asid:#x}\n");

        if size > vaddr_rel_max || vaddr_rel > vaddr_rel_max - size {
            tracef!(
                "vaddr {vaddr:#x}, size {size:#x} out of range vaddr {vaddr_base:#x}, \
                 size {vaddr_rel_max:#x}\n"
            );
            return Err(Status::INVALID_ARGS);
        }

        local_ktrace!("mmu protect", "vaddr" => vaddr as u64, "size" => size as u64);

        let top_index_shift = self.aspace.top_index_shift.load(Ordering::Relaxed);
        let tt_virt = self.aspace.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());
        // SAFETY: `tt_virt` is non-null and protected by `self.aspace.lock`.
        unsafe {
            self.protect_page_table(
                vaddr,
                vaddr_rel,
                size,
                attrs,
                enlarge,
                top_index_shift,
                tt_virt,
                cm,
            )
        }
    }

    fn query_locked(&self, vaddr: usize) -> Result<(u64, ArchMmuFlags), Status> {
        self.aspace.canary.assert();
        ltracef!("aspace {:p}, vaddr {vaddr:#x}\n", self.aspace);

        let tt_virt = self.aspace.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());

        debug_assert!(self.aspace.is_valid_vaddr(vaddr));
        if !self.aspace.is_valid_vaddr(vaddr) {
            return Err(Status::OUT_OF_RANGE);
        }

        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        let mut page_table = tt_virt;
        let mut index_shift = self.aspace.top_index_shift.load(Ordering::Relaxed);
        let mut vaddr_rem = vaddr.wrapping_sub(self.aspace.vaddr_base.load(Ordering::Relaxed));
        loop {
            let index = vaddr_rem >> index_shift;
            vaddr_rem -= index << index_shift;
            // SAFETY: `page_table` points to a valid page table under `self.aspace.lock`.
            let table = unsafe { self.page_table_slice(page_table) };
            let pte = load_pte(&table[index], Ordering::Relaxed);
            let descriptor_type = pte & MMU_PTE_DESCRIPTOR_MASK;
            let pte_addr = pte & MMU_PTE_OUTPUT_ADDR_MASK;

            ltracef!(
                "va {vaddr:#x}, index {index}, index_shift {index_shift}, \
                 rem {vaddr_rem:#x}, pte {pte:#x}\n"
            );

            if (pte & MMU_PTE_VALID) == 0 {
                assert_eq!(pte, 0, "invalid pte should be zero {pte:#x}\n");
                return Err(Status::NOT_FOUND);
            }

            let expected_leaf = if index_shift > page_size_shift {
                MMU_PTE_L012_DESCRIPTOR_BLOCK
            } else {
                MMU_PTE_L3_DESCRIPTOR_PAGE
            };
            if descriptor_type == expected_leaf {
                let paddr = pte_addr + (vaddr_rem as u64);
                let mmu_flags = self.aspace.mmu_flags_from_pte(pte);
                ltracef!("va {vaddr:#x}, paddr {paddr:#x}, flags {mmu_flags:#x}\n");
                return Ok((paddr, mmu_flags));
            }

            assert!(
                index_shift > page_size_shift && descriptor_type == MMU_PTE_L012_DESCRIPTOR_TABLE,
                "index_shift {index_shift}, page_size_shift {page_size_shift}, \
                 descriptor_type {descriptor_type:#x}"
            );

            page_table = paddr_to_pte_ptr(pte_addr);
            index_shift -= page_size_shift - 3;
        }
    }

    /// Panic if the page table is not empty.
    ///
    /// The caller must be holding `lock`.
    fn assert_empty_locked(&self) {
        let tt_virt = self.aspace.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());
        let top_index_shift = self.aspace.top_index_shift.load(Ordering::Relaxed);
        // Check to see if the top level page table is empty. If not the user didn't
        // properly unmap everything before destroying the aspace.
        // SAFETY: `tt_virt` is non-null and protected by `self.aspace.lock`.
        let index = unsafe { self.first_used_page_table_entry(tt_virt) };
        // Restricted aspaces share their top level page with their unified aspace, which maintains
        // shared mappings after base + size. Ignore those mappings when checking emptiness.
        let end_index = (self.aspace.base + (self.aspace.size - 1)) >> top_index_shift;
        if let Some(index) = index
            && index <= end_index
        {
            // Walk the page table to find the first leaf mapping holding this page table alive.
            //
            // TODO(https://fxbug.dev/42159319): Once https://fxbug.dev/42159319 is resolved this
            // call, and the entire called method, can be removed.
            let (status_raw, pt_addr, va, pte) = match self.debug_find_first_leaf_mapping() {
                Ok((pt, va, p)) => (zx_status::sys::ZX_OK, pt, va, p),
                Err((st, pt, va, p)) => (st.into_raw(), pt, va, p),
            };
            // SAFETY: `tt_virt` is non-null and `index` is within the page table bounds.
            let top_entry =
                load_pte(&unsafe { self.page_table_slice(tt_virt) }[index], Ordering::Relaxed);
            panic!(
                "top level page table still in use! aspace {:p} pt_pages {} tt_virt {:p} \
                 index {} entry {:x}. Leaf query status {} pt_addr {} vaddr {} entry {:x}\n",
                self.aspace,
                self.aspace.pt_pages.load(Ordering::Relaxed),
                tt_virt,
                index,
                top_entry,
                status_raw,
                pt_addr,
                va,
                pte
            );
        }

        let pt_pages = self.aspace.pt_pages.load(Ordering::Relaxed);
        if pt_pages != 1 {
            panic!(
                "allocated page table count is wrong, aspace {:p} count {} (should be 1)\n",
                self.aspace, pt_pages
            );
        }
    }

    /// Walks the aspace and finds the first leaf mapping that it can, return the virtual address
    /// (in the kernel physmap) of the containing page table, the virtual address of the entry, and
    /// the raw pte value.
    ///
    /// TODO(https://fxbug.dev/42159319): Once https://fxbug.dev/42159319 is resolved this method
    /// can be removed.
    fn debug_find_first_leaf_mapping(&self) -> FirstLeafMappingResult {
        self.aspace.canary.assert();

        let tt_virt = self.aspace.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());

        let page_size_shift = self.aspace.page_size_shift.load(Ordering::Relaxed);
        let count = 1usize << (page_size_shift - 3);
        let mut page_table = tt_virt;
        let mut index_shift = self.aspace.top_index_shift.load(Ordering::Relaxed);
        let mut vaddr: usize = 0;
        loop {
            // SAFETY: `page_table` points to a valid page table in physmap protected by
            // `self.aspace.lock`.
            let table = unsafe { self.page_table_slice(page_table) };
            let mut found: Option<(usize, u64)> = None;
            // Walk the page table until we find an entry.
            for (index, entry) in table.iter().enumerate().take(count) {
                let pte = load_pte(entry, Ordering::Relaxed);
                if pte != MMU_PTE_DESCRIPTOR_INVALID {
                    found = Some((index, pte));
                    break;
                }
            }
            let Some((index, pte)) = found else {
                return Err((Status::NOT_FOUND, 0, 0, 0));
            };
            // Update the virtual address for the index at the current level.
            vaddr += index << index_shift;

            let descriptor_type = pte & MMU_PTE_DESCRIPTOR_MASK;
            let pte_addr = pte & MMU_PTE_OUTPUT_ADDR_MASK;

            // If we have found a leaf mapping, return it.
            let expected_leaf = if index_shift > page_size_shift {
                MMU_PTE_L012_DESCRIPTOR_BLOCK
            } else {
                MMU_PTE_L3_DESCRIPTOR_PAGE
            };
            if descriptor_type == expected_leaf {
                return Ok((page_table.addr(), vaddr, pte));
            }

            // Assume this entry could be corrupted and validate the next table address is valid,
            // and return graceful errors on invalid descriptor types.
            if !is_physmap_phys_addr(PAddr(pte_addr as usize))
                || index_shift <= page_size_shift
                || descriptor_type != MMU_PTE_L012_DESCRIPTOR_TABLE
            {
                return Err((Status::BAD_STATE, page_table.addr(), vaddr, pte));
            }

            page_table = paddr_to_pte_ptr(pte_addr);
            index_shift -= page_size_shift - 3;
        }
    }
}

impl ArmArchVmAspace {
    fn new(
        base: usize,
        size: usize,
        aspace_type: ArmAspaceType,
        paf: Option<PageAllocFn>,
    ) -> impl PinInit<Self> {
        pin_init!(Self {
            canary: fbl::Canary::new(),
            aspace_type,
            role: AtomicU8::new(ArmAspaceRole::kIndependent as u8),
            asid: AtomicU16::new(MMU_ARM64_UNUSED_ASID),
            top_size_shift: AtomicU32::new(0),
            top_index_shift: AtomicU32::new(0),
            page_size_shift: AtomicU32::new(0),
            vaddr_base: AtomicUsize::new(0),
            tt_phys: AtomicU64::new(0),
            tt_virt: AtomicPtr::new(core::ptr::null_mut()),
            tt_page: AtomicPtr::new(core::ptr::null_mut()),
            base,
            size,
            shared_aspace: AtomicPtr::new(core::ptr::null_mut()),
            restricted_aspace: AtomicPtr::new(core::ptr::null_mut()),
            test_page_alloc_func: paf,
            accessed_since_last_check: AtomicBool::new(false),
            num_active_cpus: AtomicU32::new(0),
            pt_pages: AtomicUsize::new(0),
            rcu: SimpleGenerational::new(),
            lock <- KMutex::init(),
            state: AspaceLockedState {
                num_references: 0,
                updates_enabled: true,
            }
            .into(),
        })
    }

    /// Initializes an `ArmArchVmAspace` in place at `ptr`.
    ///
    /// # Safety
    /// `ptr` must be valid, properly aligned storage for `Self` that remains pinned until dropped.
    pub(super) unsafe fn init_in_place(
        ptr: *mut Self,
        base: usize,
        size: usize,
        aspace_type: ArmAspaceType,
        paf: Option<PageAllocFn>,
    ) {
        // SAFETY: Caller guarantees `ptr` points to valid, aligned, pinned storage for `Self`.
        let Ok(()) = unsafe { Self::new(base, size, aspace_type, paf).__pinned_init(ptr) };
    }

    #[inline(always)]
    fn tt_page_ptr(&self) -> Option<VmPagePtr> {
        // SAFETY: Non-null values in `self.tt_page` always come from a valid `VmPagePtr`.
        unsafe { VmPagePtr::from_raw(self.tt_page.load(Ordering::Relaxed)) }
    }

    #[inline(always)]
    fn role_val(&self) -> u8 {
        self.role.load(Ordering::Relaxed)
    }

    /// Return if an aspace is shared.
    #[inline(always)]
    fn is_shared(&self) -> bool {
        self.role_val() == (ArmAspaceRole::kShared as u8)
    }

    /// Return if an aspace is restricted.
    #[inline(always)]
    fn is_restricted(&self) -> bool {
        self.role_val() == (ArmAspaceRole::kRestricted as u8)
    }

    /// Return if an aspace is unified.
    #[inline(always)]
    fn is_unified(&self) -> bool {
        self.role_val() == (ArmAspaceRole::kUnified as u8)
    }

    #[inline(always)]
    fn is_valid_vaddr(&self, vaddr: usize) -> bool {
        self.size > 0 && vaddr >= self.base && vaddr <= self.base + (self.size - 1)
    }

    /// Returns the value to set the TCR register to during a context switch.
    #[inline(always)]
    fn tcr(&self) -> u64 {
        if self.is_restricted() {
            return MMU_TCR_FLAGS_USER_RESTRICTED;
        }
        MMU_TCR_FLAGS_USER
    }

    fn mmu_flags_from_pte(&self, pte: u64) -> ArchMmuFlags {
        match self.aspace_type {
            ArmAspaceType::kUser | ArmAspaceType::kKernel => s1_pte_attr_to_mmu_flags(pte),
            ArmAspaceType::kGuest => s2_pte_attr_to_mmu_flags(pte),
        }
    }

    fn mmu_params_from_flags(&self, mmu_flags: ArchMmuFlags) -> u64 {
        match self.aspace_type {
            ArmAspaceType::kUser => {
                // User pages are marked non global
                mmu_flags_to_s1_pte_attr(mmu_flags) | MMU_PTE_ATTR_NON_GLOBAL
            }
            ArmAspaceType::kKernel => mmu_flags_to_s1_pte_attr(mmu_flags),
            ArmAspaceType::kGuest => mmu_flags_to_s2_pte_attr(mmu_flags),
        }
    }

    /// # Safety
    /// Caller must hold `reader` keeping `page_table` and any reachable child page tables live for
    /// the duration of the walk.
    unsafe fn mark_accessed_page_table(
        &self,
        reader: &AutoSimpleGenerationalReader<'_>,
        mut vaddr: usize,
        vaddr_rel_in: usize,
        mut size: usize,
        index_shift: u32,
        page_table: *const Pte,
    ) {
        let page_size_shift = self.page_size_shift.load(Ordering::Relaxed);
        let block_size = 1usize << index_shift;
        let block_mask = block_size - 1;

        let mut vaddr_rel = vaddr_rel_in;

        // vaddr_rel and size must be page aligned
        debug_assert_eq!((vaddr_rel | size) & ((1usize << page_size_shift) - 1), 0);

        // SAFETY: Caller holds `reader` ensuring `page_table` stays valid across the walk.
        let table = unsafe { rcu_page_table_slice(reader, page_table, page_size_shift) };

        while size != 0 {
            let vaddr_rem = vaddr_rel & block_mask;
            let chunk_size = core::cmp::min(size, block_size - vaddr_rem);
            let index = vaddr_rel >> index_shift;

            // In the case where this ends up being a reference to another table we load-acquire to
            // ensure the contents of the table reflect the order it became visible to us. This
            // matches the release performed when mapping in new page tables.
            let mut orig_pte = load_pte(&table[index], Ordering::Acquire);
            let mut pte = orig_pte;

            if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_BLOCK
                && chunk_size != block_size
            {
                // Ignore large pages as we don't support modifying their access flags. Having this
                // empty if block simplifies the overall logic.
            } else if index_shift > page_size_shift
                && (pte & MMU_PTE_DESCRIPTOR_MASK) == MMU_PTE_L012_DESCRIPTOR_TABLE
            {
                // Set the software bit we use to represent that this page table has been accessed.
                pte |= MMU_PTE_ATTR_RES_SOFTWARE_AF;

                // The result of the compare exchange is ignored as in the unlikely event we have
                // raced with some other page table operation then we will just take another
                // accessed fault. `Ordering::Relaxed` is sufficient because setting the software AF
                // bit does not publish any dependent memory writes.
                if !compare_exchange_pte(&table[index], &mut orig_pte, pte, Ordering::Relaxed) {
                    VM_MMU_SET_AF_FAIL.add(1);
                }
                let page_table_paddr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                let next_page_table: *const Pte = core::ptr::with_exposed_provenance(
                    paddr_to_physmap(PAddr(page_table_paddr as usize)).0,
                );
                // SAFETY: Load-acquire synchronized with table installation; `reader` keeps it
                // live.
                unsafe {
                    self.mark_accessed_page_table(
                        reader,
                        vaddr,
                        vaddr_rem,
                        chunk_size,
                        index_shift - (page_size_shift - 3),
                        next_page_table,
                    );
                }
            } else if is_pte_valid(pte) && (pte & MMU_PTE_ATTR_AF) == 0 {
                pte |= MMU_PTE_ATTR_AF;

                // The result of the compare exchange is ignored as in the unlikely event we have
                // raced with some other page table operation then we will just take another
                // accessed fault. `Ordering::Relaxed` is sufficient because setting the hardware AF
                // bit does not publish any dependent memory writes.
                if !compare_exchange_pte(&table[index], &mut orig_pte, pte, Ordering::Relaxed) {
                    VM_MMU_SET_AF_FAIL.add(1);
                }
            }
            vaddr = vaddr.wrapping_add(chunk_size);
            vaddr_rel += chunk_size;
            size -= chunk_size;
        }
    }

    /// Returns the physical address of the root translation table.
    pub(super) fn arch_table_phys(&self) -> u64 {
        self.tt_phys.load(Ordering::Relaxed)
    }

    /// Returns the ASID or VMID currently assigned to this address space.
    pub(super) fn arch_asid(&self) -> u16 {
        self.asid.load(Ordering::Relaxed)
    }

    /// Sets the ASID or VMID assigned to this address space.
    pub(super) fn arch_set_asid(&self, asid: u16) {
        self.asid.store(asid, Ordering::Relaxed);
    }

    /// Returns a page-aligned candidate virtual address starting at `base`.
    pub(super) fn pick_spot(
        &self,
        base: usize,
        _end: usize,
        _align: usize,
        _size: usize,
        _mmu_flags: ArchMmuFlags,
    ) -> usize {
        self.canary.assert();
        page::round_up(base)
    }

    /// Queries the physical address and architectural MMU flags mapped at `vaddr`.
    pub(super) fn query(&self, vaddr: usize) -> Result<(u64, ArchMmuFlags), Status> {
        lock!(let al = self.lock_lock());
        let locked = LockedAspace::new(self, &al);
        locked.query_locked(vaddr)
    }

    /// Maps `count` contiguous physical pages starting at `paddr` to `vaddr`.
    ///
    /// # Safety
    /// `self` must be an initialized `ArmArchVmAspace`, and mapping
    /// `[paddr, paddr + count * page::SIZE)` at `vaddr` with `mmu_flags` must preserve memory
    /// safety and valid physical page ownership without aliasing active kernel mappings.
    pub(super) unsafe fn map_contiguous(
        &self,
        vaddr: usize,
        paddr: u64,
        count: usize,
        mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        self.canary.assert();
        ltracef!("vaddr {vaddr:#x} paddr {paddr:#x} count {count} flags {mmu_flags:#x}\n");

        assert!(!self.tt_virt.load(Ordering::Relaxed).is_null());

        debug_assert!(self.is_valid_vaddr(vaddr));
        if !self.is_valid_vaddr(vaddr) {
            return Err(Status::OUT_OF_RANGE);
        }

        if (mmu_flags & ARCH_MMU_FLAG_PERM_READ) == 0 {
            return Err(Status::INVALID_ARGS);
        }
        if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 && arch_mmu_flags_uncached(mmu_flags) {
            return Err(Status::INVALID_ARGS);
        }

        // paddr and vaddr must be aligned.
        debug_assert!(page::is_page_rounded(vaddr));
        debug_assert!(page::is_page_rounded(paddr as usize));
        if !page::is_page_rounded(vaddr) || !page::is_page_rounded(paddr as usize) {
            return Err(Status::INVALID_ARGS);
        }

        if count == 0 {
            return Ok(());
        }

        let map_size = count.saturating_mul(page::SIZE);

        {
            lock!(let a = self.lock_lock());
            assert!(a.state().updates_enabled);
            if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0
                || self.aspace_type == ArmAspaceType::kGuest
            {
                // The icache gets synced both for executable mappings, which is the expected case,
                // as well as for any physmap-backed guest mapping. For guest mappings we
                // additionally need to clean the cache fully to PoC (not just PoU as required for
                // icache consistency) as guests, who can disable their Stage-1 caches at will,
                // could otherwise see stale data that hasn't been written back to memory yet.
                let mut cache_cm = ArmVmICacheConsistencyManager::default();
                if self.aspace_type == ArmAspaceType::kGuest {
                    cache_cm.force_clean_to_poc();
                }
                for idx in 0..count {
                    let page_paddr = paddr.wrapping_add(idx.wrapping_mul(page::SIZE) as u64);
                    if is_physmap_page_mapped(page_paddr) {
                        let kva = paddr_to_physmap(PAddr(page_paddr as usize)).0;
                        // SAFETY: `is_physmap_page_mapped` verified `[kva, kva + page::SIZE)` is
                        // mapped in the kernel physmap.
                        unsafe { cache_cm.sync_addr(kva, page::SIZE) };
                    }
                }
            }
            let attrs = self.mmu_params_from_flags(mmu_flags);

            let locked = LockedAspace::new(self, &a);
            stack_pin_init!(let cm = ConsistencyManager::new(&locked));
            let paddrs = [paddr];
            let mut cursor = MappingCursor::new(&paddrs, map_size, vaddr);
            let vaddr_base = self.vaddr_base.load(Ordering::Relaxed);
            let top_size_shift = self.top_size_shift.load(Ordering::Relaxed);
            if !cursor.set_vaddr_relative_offset(vaddr_base, 1usize << top_size_shift) {
                return Err(Status::OUT_OF_RANGE);
            }
            let ro = (mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) == ARCH_MMU_FLAG_PERM_READ;
            let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
            let tt_virt = self.tt_virt.load(Ordering::Relaxed);
            assert!(!tt_virt.is_null());
            // SAFETY: `tt_virt` is non-null and protected by `self.lock`.
            let (status, lower_mapped) = unsafe {
                locked.map_page_table(
                    attrs,
                    ro,
                    top_index_shift,
                    tt_virt,
                    ExistingEntryAction::Error,
                    &mut cursor,
                    cm.as_mut(),
                )
            };
            let tt_page = self.tt_page_ptr().expect("tt_page must be initialized");
            // SAFETY: `self.lock` protects `tt_page`'s MMU mapping count.
            unsafe {
                set_page_num_mappings(tt_page, get_page_num_mappings(tt_page) + lower_mapped);
            }
            if !self.accessed_since_last_check.load(Ordering::Relaxed) {
                self.accessed_since_last_check.store(true, Ordering::Relaxed);
            }
            if let Err(err) = status {
                let mut unmap_cursor = cursor.processed_range();
                if unmap_cursor.size() > 0 {
                    // SAFETY: `tt_virt` is non-null and protected by `self.lock`.
                    let (unmap_status, unmapped) = unsafe {
                        locked.unmap_page_table(
                            &mut unmap_cursor,
                            ARCH_UNMAP_OPTION_NONE,
                            CheckForEmptyPt::Yes,
                            top_index_shift,
                            tt_virt,
                            cm.as_mut(),
                            Reclaim::No,
                        )
                    };
                    debug_assert!(unmap_status.is_ok());
                    // SAFETY: `self.lock` protects `tt_page`'s MMU mapping count.
                    unsafe {
                        set_page_num_mappings(tt_page, get_page_num_mappings(tt_page) - unmapped);
                    }
                }
                return Err(err);
            }
            debug_assert_eq!(cursor.size(), 0);
        }

        #[cfg(sanitize = "address")]
        if self.aspace_type == ArmAspaceType::kKernel {
            // SAFETY: `vaddr..vaddr + map_size` was just mapped into the kernel address space.
            unsafe { asan_map_shadow_for(vaddr, map_size) };
        }

        Ok(())
    }

    /// Maps physical pages in `phys` starting at `vaddr`.
    ///
    /// # Safety
    /// `self` must be an initialized `ArmArchVmAspace`, and mapping `phys` at `vaddr` with
    /// `mmu_flags` and `existing_action` must preserve memory safety and valid physical page
    /// ownership without invalidating or aliasing active kernel mappings.
    pub(super) unsafe fn map(
        &self,
        vaddr: usize,
        phys: &[u64],
        mmu_flags: ArchMmuFlags,
        existing_action: ExistingEntryAction,
    ) -> Result<(), Status> {
        let count = phys.len();
        self.canary.assert();
        ltracef!("vaddr {vaddr:#x} count {count} flags {mmu_flags:#x}\n");

        assert!(!self.tt_virt.load(Ordering::Relaxed).is_null());

        debug_assert!(self.is_valid_vaddr(vaddr));
        if !self.is_valid_vaddr(vaddr) {
            return Err(Status::OUT_OF_RANGE);
        }
        for &pa in phys {
            debug_assert!(page::is_page_rounded(pa as usize));
            if !page::is_page_rounded(pa as usize) {
                return Err(Status::INVALID_ARGS);
            }
        }

        if (mmu_flags & ARCH_MMU_FLAG_PERM_READ) == 0 {
            return Err(Status::INVALID_ARGS);
        }
        if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 && arch_mmu_flags_uncached(mmu_flags) {
            return Err(Status::INVALID_ARGS);
        }

        // vaddr must be aligned.
        debug_assert!(page::is_page_rounded(vaddr));
        if !page::is_page_rounded(vaddr) {
            return Err(Status::INVALID_ARGS);
        }

        if count == 0 {
            return Ok(());
        }

        {
            lock!(let a = self.lock_lock());
            assert!(a.state().updates_enabled);
            if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0
                || self.aspace_type == ArmAspaceType::kGuest
            {
                let mut cache_cm = ArmVmICacheConsistencyManager::default();
                if self.aspace_type == ArmAspaceType::kGuest {
                    // See comment in map_contiguous for why we do this for guest mappings.
                    cache_cm.force_clean_to_poc();
                }
                for &pa in phys {
                    // Ignore non-physmap pages, such as passed-through device MMIO ranges or
                    // reserved firmware runtime regions.
                    if !is_physmap_page_mapped(pa) {
                        continue;
                    }
                    let kva = paddr_to_physmap(PAddr(pa as usize)).0;
                    // SAFETY: `is_physmap_page_mapped` verified `[kva, kva + page::SIZE)` is
                    // mapped in the kernel physmap.
                    unsafe { cache_cm.sync_addr(kva, page::SIZE) };
                }
            }
            let attrs = self.mmu_params_from_flags(mmu_flags);

            let locked = LockedAspace::new(self, &a);
            stack_pin_init!(let cm = ConsistencyManager::new(&locked));
            let mut cursor = MappingCursor::new(phys, page::SIZE, vaddr);
            let vaddr_base = self.vaddr_base.load(Ordering::Relaxed);
            let top_size_shift = self.top_size_shift.load(Ordering::Relaxed);
            if !cursor.set_vaddr_relative_offset(vaddr_base, 1usize << top_size_shift) {
                return Err(Status::OUT_OF_RANGE);
            }
            let ro = (mmu_flags & ARCH_MMU_FLAG_PERM_RWX_MASK) == ARCH_MMU_FLAG_PERM_READ;
            let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
            let tt_virt = self.tt_virt.load(Ordering::Relaxed);
            assert!(!tt_virt.is_null());
            // SAFETY: `tt_virt` is non-null and protected by `self.lock`.
            let (status, lower_mapped) = unsafe {
                locked.map_page_table(
                    attrs,
                    ro,
                    top_index_shift,
                    tt_virt,
                    existing_action,
                    &mut cursor,
                    cm.as_mut(),
                )
            };
            let tt_page = self.tt_page_ptr().expect("tt_page must be initialized");
            // SAFETY: `self.lock` protects `tt_page`'s MMU mapping count.
            unsafe {
                set_page_num_mappings(tt_page, get_page_num_mappings(tt_page) + lower_mapped);
            }
            if !self.accessed_since_last_check.load(Ordering::Relaxed) {
                self.accessed_since_last_check.store(true, Ordering::Relaxed);
            }
            if let Err(err) = status {
                let mut unmap_cursor = cursor.processed_range();
                if unmap_cursor.size() > 0 {
                    // SAFETY: `tt_virt` is non-null and protected by `self.lock`.
                    let (unmap_status, unmapped) = unsafe {
                        locked.unmap_page_table(
                            &mut unmap_cursor,
                            ARCH_UNMAP_OPTION_NONE,
                            CheckForEmptyPt::Yes,
                            top_index_shift,
                            tt_virt,
                            cm.as_mut(),
                            Reclaim::No,
                        )
                    };
                    debug_assert!(unmap_status.is_ok());
                    // SAFETY: `self.lock` protects `tt_page`'s MMU mapping count.
                    unsafe {
                        set_page_num_mappings(tt_page, get_page_num_mappings(tt_page) - unmapped);
                    }
                }
                return Err(err);
            }
            debug_assert_eq!(cursor.size(), 0);
        }

        #[cfg(sanitize = "address")]
        if self.aspace_type == ArmAspaceType::kKernel {
            // SAFETY: The kernel range starting at `vaddr` was just mapped into kernel space.
            unsafe { asan_map_shadow_for(vaddr, count.saturating_mul(page::SIZE)) };
        }

        Ok(())
    }

    /// Unmaps `count` pages starting at `vaddr`.
    ///
    /// # Safety
    /// `self` must be an initialized `ArmArchVmAspace`, and unmapping
    /// `[vaddr, vaddr + count * page::SIZE)` with `unmap_options` must preserve memory safety and
    /// valid physical page ownership without invalidating active kernel virtual mappings.
    pub(super) unsafe fn unmap(
        &self,
        vaddr: usize,
        count: usize,
        unmap_options: ArchUnmapOptions,
    ) -> Result<(), Status> {
        self.canary.assert();
        ltracef!("vaddr {vaddr:#x} count {count}\n");

        assert!(!self.tt_virt.load(Ordering::Relaxed).is_null());

        debug_assert!(self.is_valid_vaddr(vaddr));

        if !self.is_valid_vaddr(vaddr) {
            return Err(Status::OUT_OF_RANGE);
        }

        debug_assert!(page::is_page_rounded(vaddr));
        if !page::is_page_rounded(vaddr) {
            return Err(Status::INVALID_ARGS);
        }

        lock!(let a = self.lock_lock());

        assert!(a.state().updates_enabled);
        let locked = LockedAspace::new(self, &a);
        stack_pin_init!(let cm = ConsistencyManager::new(&locked));
        let mut cursor = VirtualAddressCursor::new(vaddr, count.saturating_mul(page::SIZE));
        let vaddr_base = self.vaddr_base.load(Ordering::Relaxed);
        let top_size_shift = self.top_size_shift.load(Ordering::Relaxed);
        if !cursor.set_vaddr_relative_offset(vaddr_base, 1usize << top_size_shift) {
            return Err(Status::OUT_OF_RANGE);
        }
        let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
        let tt_virt = self.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());
        // SAFETY: `tt_virt` is non-null and protected by `self.lock`.
        let (ret, lower_unmapped) = unsafe {
            locked.unmap_page_table(
                &mut cursor,
                unmap_options,
                CheckForEmptyPt::No,
                top_index_shift,
                tt_virt,
                cm.as_mut(),
                Reclaim::No,
            )
        };
        let tt_page = self.tt_page_ptr().expect("tt_page must be initialized");
        // SAFETY: `self.lock` protects `tt_page`'s MMU mapping count.
        unsafe {
            set_page_num_mappings(tt_page, get_page_num_mappings(tt_page) - lower_unmapped);
        }

        debug_assert!(cursor.size() == 0 || ret.is_err());

        ret
    }

    /// Updates protection attributes on `count` pages starting at `vaddr`.
    ///
    /// # Safety
    /// `self` must be an initialized `ArmArchVmAspace`, and updating permissions on
    /// `[vaddr, vaddr + count * page::SIZE)` with `mmu_flags` and `enlarge` must preserve memory
    /// safety and valid page ownership without invalidating active kernel virtual mappings.
    pub(super) unsafe fn protect(
        &self,
        vaddr: usize,
        count: usize,
        mmu_flags: ArchMmuFlags,
        enlarge: ArchUnmapOptions,
    ) -> Result<(), Status> {
        self.canary.assert();

        if !self.is_valid_vaddr(vaddr) {
            return Err(Status::INVALID_ARGS);
        }

        if !page::is_page_rounded(vaddr) {
            return Err(Status::INVALID_ARGS);
        }

        if (mmu_flags & ARCH_MMU_FLAG_PERM_READ) == 0 {
            return Err(Status::INVALID_ARGS);
        }
        if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 && arch_mmu_flags_uncached(mmu_flags) {
            return Err(Status::INVALID_ARGS);
        }

        // The stage 2 data and instructions aborts do not contain sufficient information for us to
        // resolve permission faults, and these kinds of faults generate a hard error. As such we
        // cannot safely perform protections and instead upgrade any protect to a complete unmap,
        // therefore causing a regular translation fault that we can handle to repopulate the
        // correct mapping.
        if self.aspace_type == ArmAspaceType::kGuest {
            // SAFETY: Caller upholds safety invariants for updating `[vaddr, vaddr + count *
            // page::SIZE)` in this initialized guest stage-2 address space.
            return unsafe { self.unmap(vaddr, count, ARCH_UNMAP_OPTION_ENLARGE) };
        }

        lock!(let a = self.lock_lock());
        assert!(a.state().updates_enabled);
        let locked = LockedAspace::new(self, &a);
        if (mmu_flags & ARCH_MMU_FLAG_PERM_EXECUTE) != 0 {
            // If mappings are going to become executable then we first need to sync their caches.
            // Unfortunately this needs to be done on kernel virtual addresses to avoid taking
            // translation faults, and so we need to first query for the physical address to then
            // get the kernel virtual address in the physmap.
            // This sync could be more deeply integrated into protect_pages, but making existing
            // regions executable is very uncommon operation and so we keep it simple.
            VM_MMU_PROTECT_MAKE_EXECUTE_CALLS.add(1);
            let mut cache_cm = ArmVmICacheConsistencyManager::default();
            let mut pages_synced: i64 = 0;
            for idx in 0..count {
                let page_va = vaddr.wrapping_add(idx.wrapping_mul(page::SIZE));
                if let Ok((paddr, flags)) = locked.query_locked(page_va)
                    && (flags & ARCH_MMU_FLAG_PERM_EXECUTE) == 0
                    && is_physmap_page_mapped(paddr)
                {
                    let kva = paddr_to_physmap(PAddr(paddr as usize)).0;
                    // SAFETY: `is_physmap_page_mapped` verified `[kva, kva + page::SIZE)` is
                    // mapped in the kernel physmap.
                    unsafe { cache_cm.sync_addr(kva, page::SIZE) };
                    pages_synced += 1;
                }
            }
            VM_MMU_PROTECT_MAKE_EXECUTE_PAGES.add(pages_synced);
        }

        let attrs = self.mmu_params_from_flags(mmu_flags);
        let vaddr_base = self.vaddr_base.load(Ordering::Relaxed);

        stack_pin_init!(let cm = ConsistencyManager::new(&locked));
        locked.protect_pages(
            vaddr,
            count.saturating_mul(page::SIZE),
            attrs,
            enlarge,
            vaddr_base,
            cm,
        )
    }

    /// Harvests accessed bits across `count` pages starting at `vaddr`.
    ///
    /// # Safety
    /// `self` must be an initialized non-unified `ArmArchVmAspace`, and harvesting or unmapping
    /// mappings in `[vaddr, vaddr + count * page::SIZE)` with `non_terminal_action` and
    /// `terminal_action` must preserve memory safety and valid physical page ownership without
    /// invalidating active kernel virtual mappings.
    pub(super) unsafe fn harvest_accessed(
        &self,
        vaddr: usize,
        count: usize,
        non_terminal_action: NonTerminalAction,
        terminal_action: TerminalAction,
    ) -> Result<(), Status> {
        crate::vm::vm::vm_ktrace_duration!(
            2,
            "ArmArchVmAspace::HarvestAccessed",
            "vaddr" => vaddr as u64,
            "count" => count as u64
        );
        self.canary.assert();

        if !page::is_page_rounded(vaddr) || !self.is_valid_vaddr(vaddr) {
            return Err(Status::INVALID_ARGS);
        }

        // Harvesting is done on the restricted and shared aspaces that form the unified aspace,
        // never directly on the unified aspace.
        debug_assert!(!self.is_unified());

        // Avoid preemption while "involuntarily" holding the arch aspace lock during
        // access harvesting. The harvest loop below is O(n), however, the amount of
        // work performed with the lock held and preemption disabled is limited. Other
        // O(n) operations under this lock are opt-in by the user (e.g. map, protect)
        // and are performed with preemption enabled.

        let vaddr_base = self.vaddr_base.load(Ordering::Relaxed);
        let vaddr_rel = vaddr.wrapping_sub(vaddr_base);
        let vaddr_rel_max = 1usize << self.top_size_shift.load(Ordering::Relaxed);
        let size = count.saturating_mul(page::SIZE);

        if size > vaddr_rel_max || vaddr_rel > vaddr_rel_max - size {
            tracef!(
                "vaddr {vaddr:#x}, size {size:#x} out of range vaddr {vaddr_base:#x}, \
                 size {vaddr_rel_max:#x}\n"
            );
            return Err(Status::INVALID_ARGS);
        }

        local_ktrace!("mmu harvest accessed", "vaddr" => vaddr as u64, "size" => size as u64);

        let mut remaining_size = size;
        let mut current_vaddr = vaddr;
        let mut current_vaddr_rel = vaddr_rel;

        while remaining_size != 0 {
            // Release and re-acquire the lock to let contending threads have a chance
            // to acquire the arch aspace lock between iterations.
            lock!(let guard = self.lock_lock());
            let _trace = crate::ktrace_rs::begin_scope_cond!(
                LOCAL_KTRACE_ENABLE,
                "kernel:vm",
                "harvest_loop",
                "remaining_size" => remaining_size as u64
            );
            let mut entry_limit = HARVEST_ENTRIES_BETWEEN_UNLOCKS;
            let locked = LockedAspace::new(self, &guard);
            stack_pin_init!(let cm = ConsistencyManager::new(&locked));
            let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
            let tt_virt = self.tt_virt.load(Ordering::Relaxed);
            assert!(!tt_virt.is_null());
            // SAFETY: `tt_virt` is non-null and protected by `self.lock`.
            let harvested_size = unsafe {
                locked.harvest_accessed_page_table(
                    &mut entry_limit,
                    current_vaddr,
                    current_vaddr_rel,
                    remaining_size,
                    top_index_shift,
                    non_terminal_action,
                    terminal_action,
                    tt_virt,
                    cm,
                )
            };
            debug_assert!(harvested_size > 0);
            debug_assert!(harvested_size <= remaining_size);

            remaining_size -= harvested_size;
            current_vaddr = current_vaddr.wrapping_add(harvested_size);
            current_vaddr_rel += harvested_size;
        }

        Ok(())
    }

    /// Sets hardware/software accessed flags on PTEs covering
    /// `[vaddr, vaddr + count * page::SIZE)`.
    ///
    /// # Safety
    /// `self` must be an initialized non-unified `ArmArchVmAspace`, and `destroy()` must not be
    /// called concurrently with `mark_accessed()`.
    pub(super) unsafe fn mark_accessed(&self, vaddr: usize, count: usize) -> Result<(), Status> {
        crate::vm::vm::vm_ktrace_duration!(
            2,
            "ArmArchVmAspace::MarkAccessed",
            "vaddr" => vaddr as u64,
            "count" => count as u64
        );
        self.canary.assert();

        if !page::is_page_rounded(vaddr) || !self.is_valid_vaddr(vaddr) {
            return Err(Status::OUT_OF_RANGE);
        }

        // Mark accessed is called on the restricted and shared aspaces that form the unified
        // aspace, never directly on the unified aspace.
        debug_assert!(!self.is_unified());

        let vaddr_base = self.vaddr_base.load(Ordering::Relaxed);
        let vaddr_rel = vaddr.wrapping_sub(vaddr_base);
        let vaddr_rel_max = 1usize << self.top_size_shift.load(Ordering::Relaxed);
        let size = count.saturating_mul(page::SIZE);

        if size > vaddr_rel_max || vaddr_rel > vaddr_rel_max - size {
            tracef!(
                "vaddr {vaddr:#x}, size {size:#x} out of range vaddr {vaddr_base:#x}, \
                 size {vaddr_rel_max:#x}\n"
            );
            return Err(Status::OUT_OF_RANGE);
        }

        local_ktrace!("mmu mark accessed", "vaddr" => vaddr as u64, "size" => size as u64);

        // Begin our read side critical section that ensures that the page tables we find from
        // `tt_virt` will remain valid. Note that the caller of `mark_accessed` has guaranteed that
        // the aspace is both alive and that `destroy()` will not be called during.
        stack_pin_init!(let reader = AutoSimpleGenerationalReader::new(&self.rcu));

        let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
        let tt_virt = self.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());
        // SAFETY: `reader` keeps all reachable page tables alive while walking from `tt_virt`.
        unsafe {
            self.mark_accessed_page_table(
                &reader,
                vaddr,
                vaddr_rel,
                size,
                top_index_shift,
                tt_virt,
            );
        }
        // `Ordering::Relaxed` is sufficient because `accessed_since_last_check` is an independent
        // advisory hint flag.
        if !self.accessed_since_last_check.load(Ordering::Relaxed) {
            self.accessed_since_last_check.store(true, Ordering::Relaxed);
        }

        Ok(())
    }

    /// Returns whether any mappings in this aspace have been accessed since the previous check,
    /// optionally clearing the flag.
    pub(super) fn accessed_since_last_check(&self, clear: bool) -> bool {
        // `Ordering::Relaxed` is sufficient for this standalone advisory flag.
        let accessed = self.accessed_since_last_check.load(Ordering::Relaxed);
        if clear && accessed {
            self.accessed_since_last_check.store(false, Ordering::Relaxed);
        }
        accessed
    }

    /// Initializes this address space's root translation table and ASID state.
    ///
    /// # Safety
    /// `self` must not yet be initialized, `self.base` and `self.size` must satisfy architectural
    /// range invariants for `self.aspace_type`, and for `ArmAspaceType::kKernel`,
    /// `arm64_mmu_early_init()` must have completed and at most one kernel `ArmArchVmAspace` may
    /// own `root_kernel_page_table_phys`.
    pub(super) unsafe fn init(&self) -> Result<(), Status> {
        self.canary.assert();
        let type_name = arm_aspace_type_name(self.aspace_type);
        ltracef!(
            "aspace {:p}, base {:#x}, size {:#x}, type {}\n",
            self,
            self.base,
            self.size,
            type_name
        );

        lock!(let a = self.lock_lock());

        // Validate that the base + size is sane and doesn't wrap.
        debug_assert!(self.size > page::SIZE);
        debug_assert!(self.base.wrapping_add(self.size - 1) > self.base);

        if self.aspace_type == ArmAspaceType::kKernel {
            // At the moment we can only deal with address spaces as globally defined.
            debug_assert_eq!(self.base, !0usize << MMU_KERNEL_SIZE_SHIFT);
            debug_assert_eq!(self.size, 1usize << MMU_KERNEL_SIZE_SHIFT);

            self.vaddr_base.store(!0usize << MMU_KERNEL_SIZE_SHIFT, Ordering::Relaxed);
            self.top_size_shift.store(MMU_KERNEL_SIZE_SHIFT, Ordering::Relaxed);
            self.top_index_shift.store(MMU_KERNEL_TOP_SHIFT, Ordering::Relaxed);
            self.page_size_shift.store(MMU_KERNEL_PAGE_SIZE_SHIFT, Ordering::Relaxed);

            let root_pa = root_kernel_page_table_phys.load(Ordering::Relaxed);
            let tt_virt = paddr_to_pte_ptr(root_pa);
            self.tt_virt.store(tt_virt, Ordering::Relaxed);
            self.tt_phys.store(root_pa, Ordering::Relaxed);
            let page = pmm::paddr_to_vm_page(PAddr(root_pa as usize))
                .expect("kernel root page table must have VmPage");
            debug_assert_eq!(page.state(), VmPageState(vm_page_state::MMU));
            self.tt_page.store(page.as_non_null().as_ptr(), Ordering::Relaxed);
            self.asid.store(MMU_ARM64_GLOBAL_ASID, Ordering::Relaxed);
        } else {
            match self.aspace_type {
                ArmAspaceType::kUser => {
                    debug_assert!(self.base + self.size <= 1usize << MMU_USER_SIZE_SHIFT);

                    self.vaddr_base.store(0, Ordering::Relaxed);
                    self.top_size_shift.store(MMU_USER_SIZE_SHIFT, Ordering::Relaxed);
                    self.top_index_shift.store(MMU_USER_TOP_SHIFT, Ordering::Relaxed);
                    self.page_size_shift.store(MMU_USER_PAGE_SIZE_SHIFT, Ordering::Relaxed);

                    if FEAT_ASID_ENABLED.load(Ordering::Relaxed) {
                        let status = asid_alloc();
                        if let Err(err) = status {
                            kprint::kprint!("ARM: out of ASIDs!\n");
                            return Err(err);
                        }
                        self.asid.store(status.unwrap(), Ordering::Relaxed);
                    } else {
                        // Initialize to a valid value even when disabled to distinguish from the
                        // destroyed case.
                        self.asid.store(MMU_ARM64_FIRST_USER_ASID, Ordering::Relaxed);
                    }
                }
                ArmAspaceType::kGuest => {
                    debug_assert!(self.base + self.size <= 1usize << MMU_GUEST_SIZE_SHIFT);

                    self.vaddr_base.store(0, Ordering::Relaxed);
                    self.top_size_shift.store(MMU_GUEST_SIZE_SHIFT, Ordering::Relaxed);
                    self.top_index_shift.store(MMU_GUEST_TOP_SHIFT, Ordering::Relaxed);
                    self.page_size_shift.store(MMU_GUEST_PAGE_SIZE_SHIFT, Ordering::Relaxed);
                }
                ArmAspaceType::kKernel => unreachable!(),
            }

            // allocate a top level page table to serve as the translation table
            let locked = LockedAspace::new(self, &a);
            let page = locked.alloc_page_table()?;
            // SAFETY: `page` was just allocated for this top-level translation table.
            let pa = unsafe { page.paddr() }.0 as u64;

            let va = paddr_to_pte_ptr(pa);

            self.tt_virt.store(va, Ordering::Relaxed);
            self.tt_phys.store(pa, Ordering::Relaxed);
            self.tt_page.store(page.as_non_null().as_ptr(), Ordering::Relaxed);

            // zero the top level translation table.
            // SAFETY: `va` is a page-aligned physmap pointer of `page::SIZE` bytes.
            unsafe { arch_zero_page(va.cast()) };
            dsb_ishst();
        }
        self.pt_pages.store(1, Ordering::Relaxed);
        VM_MMU_PAGE_TABLE_ALLOC.add(1);

        ltracef!(
            "tt_phys {:#x} tt_virt {:p}\n",
            self.tt_phys.load(Ordering::Relaxed),
            self.tt_virt.load(Ordering::Relaxed)
        );

        Ok(())
    }

    /// Initializes this address space as a restricted user address space.
    ///
    /// # Safety
    /// `self` must be an uninitialized user `ArmArchVmAspace` whose `base` and `size` satisfy
    /// architectural user restricted address space geometry invariants.
    pub(super) unsafe fn init_restricted(&self) -> Result<(), Status> {
        self.role.store(ArmAspaceRole::kRestricted as u8, Ordering::Relaxed);
        // SAFETY: Caller upholds `init` preconditions for this restricted user address space.
        unsafe { self.init() }
    }

    /// Initializes this address space as a shared user address space with prepopulated top-level
    /// entries.
    ///
    /// # Safety
    /// `self` must be an uninitialized user `ArmArchVmAspace` whose `base` and `size` satisfy
    /// architectural user shared address space geometry invariants.
    pub(super) unsafe fn init_shared(&self) -> Result<(), Status> {
        // SAFETY: Caller upholds `init` preconditions for this shared user address space.
        unsafe { self.init() }?;
        self.role.store(ArmAspaceRole::kShared as u8, Ordering::Relaxed);

        lock!(let a = self.lock_lock());
        let locked = LockedAspace::new(self, &a);

        // Prepopulate the portion of the top level page table spanned by this aspace by allocating
        // the necessary second level entries.
        let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
        let tt_virt = self.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());
        // SAFETY: `tt_virt` was initialized in `self.init()` and is protected by `self.lock`.
        let top_table = unsafe { locked.page_table_slice(tt_virt) };

        let start = self.base >> top_index_shift;
        let end = (self.base + (self.size - 1)) >> top_index_shift;
        for entry in &top_table[start..=end] {
            debug_assert_eq!(
                load_pte(entry, Ordering::Relaxed) & MMU_PTE_DESCRIPTOR_MASK,
                MMU_PTE_DESCRIPTOR_INVALID
            );
            let page = locked.alloc_page_table()?;
            // SAFETY: `page` was just allocated for this page table.
            let page_table_paddr = unsafe { page.paddr() }.0 as u64;
            let pt_vaddr = paddr_to_pte_ptr(page_table_paddr);
            // SAFETY: `pt_vaddr` is a page-aligned physmap address of `page::SIZE`.
            unsafe { arch_zero_page(pt_vaddr.cast()) };
            dsb_ishst();
            // Do not eagerly set MMU_PTE_ATTR_RES_SOFTWARE_AF for these top level entries. Let the
            // accessed fault (or mapping) path set it only for entries that actually do get
            // accessed, as is the case with any typical aspace.
            //
            // When resolving an accessed fault that originated in the unified aspace, we will look
            // up the faulting address, and if it falls in the shared region, switch to the shared
            // aspace to resolve it. No accessed faults or regular hardware page faults get
            // resolved on the unified aspace. So we do have the ability to set the AF bit only on
            // portions of the shared aspace that are indeed accessed. This saves the accessed bit
            // harvesting code from having to do unnecessary work recursing *all* top level page
            // tables every time.
            //
            // When these top level entries are copied into a unified aspace, they might not have
            // the AF bit set, and since no faults are resolved on the unified aspace, the AF bit
            // might never be set for these entries. This is fine, because we never do any
            // harvesting on the unified aspace anyway, so whether or not AF is set is irrelevant.
            // Similarly any entries that did have AF set at the time of copying are fine too. All
            // harvesting is done on the shared and restricted aspaces that the unified aspace is
            // composed of.
            update_pte(entry, page_table_paddr | MMU_PTE_L012_DESCRIPTOR_TABLE, Ordering::Relaxed);
        }
        Ok(())
    }

    /// Initializes this aspace as a unified user address space backed by `shared` and
    /// `restricted`.
    ///
    /// # Safety
    /// `self` must be an uninitialized user `ArmArchVmAspace` distinct from `shared` and
    /// `restricted`, `shared` must be initialized via `init_shared()`, `restricted` must be an
    /// empty non-unified aspace initialized via `init_restricted()` with top-level indices
    /// disjoint from `shared`, and both `shared` and `restricted` must remain live and
    /// undestroyed until after `self.destroy()` completes.
    pub(super) unsafe fn init_unified(
        &self,
        shared: &ArmArchVmAspace,
        restricted: &ArmArchVmAspace,
    ) -> Result<(), Status> {
        self.canary.assert();
        let type_name = arm_aspace_type_name(self.aspace_type);
        ltracef!(
            "unified aspace {:p}, base {:#x}, size {:#x}, type {}\n",
            self,
            self.base,
            self.size,
            type_name
        );

        // Initialize this aspace.
        {
            lock!(let _a = self.lock_lock());
            debug_assert!(self.base + self.size <= 1usize << MMU_USER_SIZE_SHIFT);

            self.vaddr_base.store(0, Ordering::Relaxed);
            self.top_size_shift.store(MMU_USER_SIZE_SHIFT, Ordering::Relaxed);
            self.top_index_shift.store(MMU_USER_TOP_SHIFT, Ordering::Relaxed);
            self.page_size_shift.store(MMU_USER_PAGE_SIZE_SHIFT, Ordering::Relaxed);

            // Assign the restricted address space's ASID to this address space.
            if FEAT_ASID_ENABLED.load(Ordering::Relaxed) {
                self.asid.store(restricted.asid.load(Ordering::Relaxed), Ordering::Relaxed);
            } else {
                // Initialize to a valid value even when disabled to distinguish from the destroyed
                // case.
                self.asid.store(MMU_ARM64_FIRST_USER_ASID, Ordering::Relaxed);
            }

            // Unified aspaces use the same page table root that the restricted page table does.
            let r_tt_virt = restricted.tt_virt.load(Ordering::Relaxed);
            let r_tt_phys = restricted.tt_phys.load(Ordering::Relaxed);
            self.tt_virt.store(r_tt_virt, Ordering::Relaxed);
            self.tt_phys.store(r_tt_phys, Ordering::Relaxed);
            self.tt_page.store(restricted.tt_page.load(Ordering::Relaxed), Ordering::Relaxed);

            // Set up our pointers to the restricted and shared aspaces.
            self.restricted_aspace
                .store(core::ptr::NonNull::from_ref(restricted).as_ptr(), Ordering::Relaxed);
            self.shared_aspace
                .store(core::ptr::NonNull::from_ref(shared).as_ptr(), Ordering::Relaxed);
            self.role.store(ArmAspaceRole::kUnified as u8, Ordering::Relaxed);

            ltracef!("tt_phys {r_tt_phys:#x} tt_virt {r_tt_virt:p}\n");
        }

        let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
        let page_size_shift = self.page_size_shift.load(Ordering::Relaxed);
        let restricted_start = restricted.base >> top_index_shift;
        let restricted_end = (restricted.base + (restricted.size - 1)) >> top_index_shift;
        let shared_start = shared.base >> top_index_shift;
        let shared_end = (shared.base + (shared.size - 1)) >> top_index_shift;
        debug_assert!(restricted_end < shared_start);

        // Validate that the restricted aspace is empty and set its metadata.
        {
            lock!(let mut a = restricted.lock_lock());
            let r_tt_virt = restricted.tt_virt.load(Ordering::Relaxed);
            assert!(!r_tt_virt.is_null());
            debug_assert_eq!(a.state().num_references, 0);
            debug_assert!(!restricted.is_unified());
            {
                let r_locked = LockedAspace::new(restricted, &a);
                // SAFETY: `r_tt_virt` is non-null and protected by `restricted.lock`.
                let r_table = unsafe { r_locked.page_table_slice(r_tt_virt) };
                for entry in &r_table[restricted_start..=restricted_end] {
                    debug_assert_eq!(
                        load_pte(entry, Ordering::Relaxed) & MMU_PTE_DESCRIPTOR_MASK,
                        MMU_PTE_DESCRIPTOR_INVALID
                    );
                }
            }
            a.as_mut().state_mut().num_references += 1;
        }

        // Copy all mappings from the shared aspace and set its metadata.
        {
            lock!(let mut a = shared.lock_lock());
            let s_tt_virt = shared.tt_virt.load(Ordering::Relaxed);
            let u_tt_virt = self.tt_virt.load(Ordering::Relaxed);
            assert!(!s_tt_virt.is_null());
            assert!(!u_tt_virt.is_null());
            debug_assert!(shared.is_shared());
            debug_assert!(!restricted.is_unified());
            {
                let s_locked = LockedAspace::new(shared, &a);
                // SAFETY: `s_tt_virt` is non-null and protected by `shared.lock`.
                let s_table = unsafe { s_locked.page_table_slice(s_tt_virt) };
                let pt_len = 1usize << (page_size_shift - 3);
                // SAFETY: `u_tt_virt` is non-null and `shared_start..=shared_end` is disjoint from
                // the restricted half and owned exclusively by `self` during `init_unified`.
                let u_table = unsafe { core::slice::from_raw_parts(u_tt_virt, pt_len) };
                for i in shared_start..=shared_end {
                    let shared_pte = load_pte(&s_table[i], Ordering::Relaxed);
                    debug_assert_eq!(
                        shared_pte & MMU_PTE_DESCRIPTOR_MASK,
                        MMU_PTE_L012_DESCRIPTOR_TABLE
                    );
                    update_pte(&u_table[i], shared_pte, Ordering::Relaxed);
                }
            }
            a.as_mut().state_mut().num_references += 1;
        }
        Ok(())
    }

    /// Disables further page table updates on this address space and verifies individual aspaces
    /// are empty.
    pub(super) fn disable_updates(&self) {
        self.canary.assert();

        lock!(let mut a = self.lock_lock());
        a.as_mut().state_mut().updates_enabled = false;
        if self.tt_virt.load(Ordering::Relaxed).is_null() {
            // Initialization must not have succeeded.
            return;
        }
        if !self.is_shared() && !self.is_unified() {
            let locked = LockedAspace::new(self, &a);
            locked.assert_empty_locked();
        }
    }

    /// The `destroy_individual` and `destroy_unified` functions are called by the public `destroy`
    /// function depending on whether this address space is unified or not. Note that unified
    /// aspaces must be destroyed prior to destroying their constituent aspaces. In other words,
    /// the shared and restricted aspaces must have a longer lifetime than the unified aspaces they
    /// are part of.
    fn destroy_individual(&self) -> Result<(), Status> {
        debug_assert!(!self.is_unified());

        lock!(let a = self.lock_lock());
        debug_assert_eq!(a.state().num_references, 0);

        let tt_virt = self.tt_virt.load(Ordering::Relaxed);
        assert!(!tt_virt.is_null());
        let top_index_shift = self.top_index_shift.load(Ordering::Relaxed);
        let locked = LockedAspace::new(self, &a);

        // If this page table has a prepopulated top level, we need to manually clean up the
        // entries we created in `init_shared`. We know for sure that these entries are no longer
        // referenced by other page tables because we expect those page tables to have been
        // destroyed before this one.
        if self.is_shared() {
            // SAFETY: `tt_virt` is non-null and protected by `self.lock`.
            let top_table = unsafe { locked.page_table_slice(tt_virt) };
            let start = self.base >> top_index_shift;
            let end = (self.base + (self.size - 1)) >> top_index_shift;
            for entry in &top_table[start..=end] {
                let pte = load_pte(entry, Ordering::Relaxed);
                let paddr = pte & MMU_PTE_OUTPUT_ADDR_MASK;
                let page = pmm::paddr_to_vm_page(PAddr(paddr as usize))
                    .expect("shared top-level PTE must reference a valid VmPage");
                debug_assert_eq!(page.state(), VmPageState(vm_page_state::MMU));
                // SAFETY: Every top-level entry in `start..=end` was populated by `init_shared()`
                // with an `MMU` page table page (verified above), and the caller of `destroy()`
                // guarantees all referencing unified aspaces are destroyed with no concurrent
                // operations.
                unsafe { cache_free_page(page) };
                self.pt_pages.fetch_sub(1, Ordering::Relaxed);
                update_pte(entry, MMU_PTE_DESCRIPTOR_INVALID, Ordering::Relaxed);
            }
        }

        locked.assert_empty_locked();

        // Need a DSB to synchronize any page table updates prior to flushing the TLBs.
        dsb_ish();

        // Flush the ASID or VMID associated with this aspace
        locked.flush_asid();

        // Need a DSB to ensure all other cpus have fully processed the TLB flush.
        dsb_ish();

        // Free any ASID.
        if self.aspace_type == ArmAspaceType::kUser {
            let asid = self.asid.load(Ordering::Relaxed);
            if FEAT_ASID_ENABLED.load(Ordering::Relaxed) {
                asid_free(asid).expect("asid_free failed");
            } else {
                debug_assert_eq!(asid, MMU_ARM64_FIRST_USER_ASID);
            }
            self.asid.store(MMU_ARM64_UNUSED_ASID, Ordering::Relaxed);
        }

        // Free the top level page table.
        let page = self.tt_page_ptr();
        debug_assert!(page.is_some());
        // SAFETY: Caller of `destroy()` guarantees `self` is not active on any CPU and has no
        // concurrent readers during or after `destroy()`; `flush_asid()` and `dsb_ish()` completed
        // TLB shootdown, and clearing `self.tt_page` below ensures `page` is freed only once.
        unsafe { cache_free_page(page.expect("tt_page must be initialized")) };
        self.pt_pages.fetch_sub(1, Ordering::Relaxed);
        VM_MMU_PAGE_TABLE_FREE.add(1);

        self.tt_phys.store(0, Ordering::Relaxed);
        self.tt_virt.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.tt_page.store(core::ptr::null_mut(), Ordering::Relaxed);

        Ok(())
    }

    fn destroy_unified(&self) -> Result<(), Status> {
        {
            let shared_ptr = self.shared_aspace.load(Ordering::Relaxed);
            assert!(!shared_ptr.is_null());
            // SAFETY: Caller of `destroy()` guarantees `self`'s constituent `shared` aspace is
            // still live and undestroyed.
            let shared_ref = unsafe { &*shared_ptr };
            lock!(let mut a = shared_ref.lock_lock());
            // The shared page table should be referenced by at least this page table, and could be
            // referenced by many other unified page tables.
            debug_assert!(a.state().num_references > 0);
            a.as_mut().state_mut().num_references -= 1;
        }
        {
            let restricted_ptr = self.restricted_aspace.load(Ordering::Relaxed);
            assert!(!restricted_ptr.is_null());
            // SAFETY: Caller of `destroy()` guarantees `self`'s constituent `restricted` aspace is
            // still live and undestroyed.
            let restricted_ref = unsafe { &*restricted_ptr };
            lock!(let mut a = restricted_ref.lock_lock());
            // The `restricted_aspace` page table can only be referenced by a singular unified page
            // table.
            debug_assert_eq!(a.state().num_references, 1);
            a.as_mut().state_mut().num_references -= 1;
        }
        self.shared_aspace.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.restricted_aspace.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.asid.store(MMU_ARM64_UNUSED_ASID, Ordering::Relaxed);
        self.tt_phys.store(0, Ordering::Relaxed);
        self.tt_page.store(core::ptr::null_mut(), Ordering::Relaxed);
        self.tt_virt.store(core::ptr::null_mut(), Ordering::Relaxed);
        Ok(())
    }

    /// Destroys this address space, freeing its root page table and ASID once unmapped.
    ///
    /// # Safety
    /// `self` may be uninitialized or have failed `init*` partially; it must not be active on any
    /// CPU, no concurrent `mark_accessed` or other operations may run on `self` during or after
    /// `destroy()`, any unified aspaces referencing `self` must already be destroyed, and if
    /// `self` is unified its constituent `shared` and `restricted` aspaces must still be live and
    /// undestroyed.
    pub(super) unsafe fn destroy(&self) -> Result<(), Status> {
        self.canary.assert();
        ltracef!("aspace {:p}\n", self);

        // We cannot destroy the kernel address space.
        debug_assert_ne!(self.aspace_type, ArmAspaceType::kKernel);

        // Make sure initialization succeeded.
        if self.tt_virt.load(Ordering::Relaxed).is_null() {
            debug_assert_eq!(self.tt_phys.load(Ordering::Relaxed), 0);
            debug_assert!(self.tt_page.load(Ordering::Relaxed).is_null());
            return Ok(());
        }

        if self.is_unified() {
            return self.destroy_unified();
        }
        self.destroy_individual()
    }

    /// Swaps the user MMU hardware context (`TTBR0_EL1` and `TCR_EL1`) during a thread context
    /// switch. Assumes `old_aspace != aspace` and optimizes as such.
    ///
    /// # Safety
    /// `old_aspace` must match the user address space active on the current CPU (or `None` for
    /// kernel-only), `aspace` must be an initialized user `ArmArchVmAspace` (or `None`), and local
    /// interrupts/scheduler state must prevent migration across the switch.
    pub(super) unsafe fn context_switch(
        old_aspace: Option<&ArmArchVmAspace>,
        aspace: Option<&ArmArchVmAspace>,
    ) {
        let tcr: u64;
        let ttbr: u64;
        // If we're not using ASIDs, we need to trigger a TLB flush here so we don't leak entries
        // across the context switch. Note that we do not need to perform this flush if we are
        // switching to the kernel's address space, as those mappings are global and will be
        // unaffected by the flush.
        if let Some(aspace) = aspace
            && !FEAT_ASID_ENABLED.load(Ordering::Relaxed)
        {
            let asid = aspace.asid.load(Ordering::Relaxed);
            // `asid` is always set to MMU_ARM64_FIRST_USER_ASID when ASID use is disabled, so this
            // will invalidate all TLB entries except the global ones.
            debug_assert_eq!(asid, MMU_ARM64_FIRST_USER_ASID);
            arm64_tlbi_asid!(aside1, asid);
            // DSB to ensure TLB flushes happen prior to returning to user.
            dsb_ish();
        }
        if let Some(aspace) = aspace {
            aspace.canary.assert();
            // Check that we are switching to a user aspace, and that the asid is in the valid
            // range.
            debug_assert_eq!(aspace.aspace_type, ArmAspaceType::kUser);
            let asid = aspace.asid.load(Ordering::Relaxed);
            debug_assert!(asid >= MMU_ARM64_FIRST_USER_ASID);

            // Compute the user space TTBR with the translation table and user space ASID.
            ttbr = ((asid as u64) << 48) | aspace.tt_phys.load(Ordering::Relaxed);
            tcr = aspace.tcr();

            // Update TCR and TTBR0 if the new aspace uses different values, or if we're switching
            // away from the kernel aspace.
            match old_aspace {
                None => {
                    // SAFETY: Context-switch path with migration suppressed; `ttbr` and `tcr` come
                    // from an initialized user `ArmArchVmAspace` and affect only EL0 translations,
                    // never kernel (`TTBR1_EL1`) memory that Rust reasons about.
                    unsafe {
                        arm_wsr64!("ttbr0_el1", ttbr);
                        arm_wsr64!("tcr_el1", tcr);
                    }
                    isb();
                }
                Some(old_aspace) => {
                    let old_ttbr = ((old_aspace.asid.load(Ordering::Relaxed) as u64) << 48)
                        | old_aspace.tt_phys.load(Ordering::Relaxed);
                    let mut needs_isb = false;
                    if old_ttbr != ttbr {
                        // SAFETY: `ttbr` comes from an initialized user `ArmArchVmAspace` on the
                        // context-switch path and affects only EL0 user translations.
                        unsafe { arm_wsr64!("ttbr0_el1", ttbr) };
                        needs_isb = true;
                    }
                    if old_aspace.tcr() != aspace.tcr() {
                        // SAFETY: `tcr` preserves `TTBR1_EL1` kernel translation settings and only
                        // updates EL0 user walk configuration on the context-switch path.
                        unsafe { arm_wsr64!("tcr_el1", tcr) };
                        needs_isb = true;
                    }
                    if needs_isb {
                        isb();
                    }
                    // `Ordering::Relaxed` is sufficient: `num_active_cpus` is only consulted by
                    // the adjacent `debug_assert!`s, so nothing is decided from its value and
                    // nothing is published through it.
                    let prev = old_aspace.num_active_cpus.fetch_sub(1, Ordering::Relaxed);
                    debug_assert!(prev > 0);
                }
            }
            let prev = aspace.num_active_cpus.fetch_add(1, Ordering::Relaxed);
            debug_assert!(prev < SMP_MAX_CPUS as u32);
        } else {
            // Switching to the null aspace, which means kernel address space only.
            // Load a null TTBR0 and disable page table walking for user space.
            tcr = MMU_TCR_FLAGS_KERNEL;
            // SAFETY: Disables `TTBR0_EL1` user walks (`EPD0 = 1`) while preserving `TTBR1_EL1`
            // kernel mappings on the context-switch path.
            unsafe { arm_wsr64!("tcr_el1", tcr) };
            isb();

            ttbr = 0; // MMU_ARM64_UNUSED_ASID
            // SAFETY: `EPD0` in `tcr_el1` is already set and synchronized by `isb()` above, so
            // clearing `ttbr0_el1` performs no table walks and touches no Rust memory.
            unsafe { arm_wsr64!("ttbr0_el1", ttbr) };
            isb();

            if let Some(old_aspace) = old_aspace {
                // `Ordering::Relaxed` is sufficient: `num_active_cpus` is only consulted by the
                // adjacent `debug_assert!`s, so nothing is decided from its value and nothing is
                // published through it.
                let prev = old_aspace.num_active_cpus.fetch_sub(1, Ordering::Relaxed);
                debug_assert!(prev > 0);
            }
        }
        if TRACE_CONTEXT_SWITCH {
            tracef!(
                "old aspace {:p} aspace {:p} ttbr {ttbr:#x}, tcr {tcr:#x}\n",
                old_aspace.map_or(core::ptr::null(), core::ptr::from_ref),
                aspace.map_or(core::ptr::null(), core::ptr::from_ref)
            );
        }
    }

    /// Initializes parity bits and `num_mappings` counts on boot page table pages handed off from
    /// physboot.
    ///
    /// # Safety
    /// Every page in `mmu_pages` must be a valid physmap-mapped boot page table page exclusively
    /// owned by early boot handoff.
    pub(super) unsafe fn handoff_page_tables_from_physboot(
        mut mmu_pages: Pin<&mut VmPageDoublyLinkedList>,
    ) {
        while let Some(page_nn) = mmu_pages.as_mut().pop_front() {
            let page = VmPagePtr::new(page_nn);
            // SAFETY: Single-threaded physboot handoff owns all boot MMU pages in `mmu_pages`;
            // live `TTBR1_EL1` entries are accessed atomically through `&[Pte]`.
            unsafe {
                page.set_state(VmPageState(vm_page_state::MMU));

                let pt_ptr: *const Pte =
                    core::ptr::with_exposed_provenance(paddr_to_physmap(page.paddr()).0);
                assert!(!pt_ptr.is_null());
                let entries =
                    core::slice::from_raw_parts(pt_ptr, page::SIZE / core::mem::size_of::<Pte>());
                let mut num_mappings: u32 = 0;
                for entry in entries {
                    let pte = entry.load(Ordering::Relaxed);
                    let with_parity = add_parity(pte);
                    if with_parity != pte {
                        entry.store(with_parity, Ordering::Relaxed);
                    }
                    if (with_parity & MMU_PTE_VALID) != 0 {
                        num_mappings += 1;
                    }
                }
                set_page_num_mappings(page, num_mappings);
            }
        }
    }
}

/// Batches data-to-instruction cache maintenance and barriers across mapped page ranges.
#[repr(C)]
#[derive(Default, pin_init::Zeroable)]
pub(super) struct ArmVmICacheConsistencyManager {
    /// When set `sync_addr` will always clean to PoC, even if cleaning is not required by the
    /// implementation.
    clean_poc: bool,
    need_dsb: bool,
    need_invalidate: bool,
}

impl Drop for ArmVmICacheConsistencyManager {
    fn drop(&mut self) {
        self.finish();
    }
}

impl ArmVmICacheConsistencyManager {
    /// If a cache operation is going to be performed anyway on the addresses provided, then can
    /// avoid doing duplicate operations by insisting `sync_addr` cleans to the PoC. If `sync_addr`
    /// did not need to do any cache operation, due to CTR.IDC\[28\]=1, then this still forces it
    /// to do a clean to PoC. Note that the default clean, when CTR.IDC\[28\] = 0, is only to PoU.
    pub(super) fn force_clean_to_poc(&mut self) {
        self.clean_poc = true;
    }

    /// Cleans data cache and schedules or performs instruction cache invalidation across
    /// `start..start + len`.
    ///
    /// # Safety
    /// `start..start + len` must be a valid mapped kernel virtual address range.
    pub(super) unsafe fn sync_addr(&mut self, start: usize, len: usize) {
        // Validate we are operating on a kernel address range.
        debug_assert!(is_kernel_address(start));

        // SAFETY: Only the boot CPU's `arm64_feature_init` writes `arm64_cache_features`, before
        // secondary CPUs start and before any address space mappings run, so no write races with
        // this read.
        let features = unsafe { super::feature::arm64_cache_features };

        // Use the physmap to clean the range. If we have been requested to clean to PoC then we
        // must do that now, otherwise we can either clean to POU or if the cpu allows it, skip
        // cleaning. Both of the clean functions have a DSB at the end to synchronize the DC cache
        // instructions.
        if self.clean_poc {
            // SAFETY: Caller guarantees `start..start + len` is a mapped kernel virtual range.
            unsafe { arch_clean_cache_range(start, len) };
        } else if !features.idc {
            // We need to clean to POU to maintain D->I cache coherency.
            // SAFETY: Caller guarantees `start..start + len` is a mapped kernel virtual range.
            unsafe { arm64_clean_cache_range_pou(start, len) };
        } else {
            // IDC == 1
            self.need_dsb = true;
        }

        // Do we need to invalidate the icache?
        if !features.dic {
            // yes
            // Are we VIPT or PIPT?
            if features.pipt {
                // We can invalidate just this alias of the page.
                if self.need_dsb {
                    // If we haven't DSBed earlier, we must now.
                    dsb_ishst();
                }
                // SAFETY: Caller guarantees `start..start + len` is a mapped kernel virtual range.
                unsafe { InvalidateInstructionCacheRange(start, len) };
            } else {
                // We will batch the icache global invalidate and perform it at the end.
                self.need_dsb = true;
                self.need_invalidate = true;
            }
        } else {
            // IF ctr.dic == 1, we do not need to invalidate the icache at all, but we will need to
            // insert an ISB at the end, so set need_invalidate here and sync later.
            self.need_invalidate = true;
        }
    }

    /// Completes any pending barriers or batched instruction cache invalidations.
    pub(super) fn finish(&mut self) {
        if !self.need_invalidate {
            return;
        }

        // Previously we were instructed to DSB, so do so here before any icache flushing or
        // syncing.
        if self.need_dsb {
            dsb_ishst();
        }

        // SAFETY: Only the boot CPU's `arm64_feature_init` writes `arm64_cache_features`, before
        // secondary CPUs start, so no write races with this read.
        let dic = unsafe { super::feature::arm64_cache_features.dic };

        // In the ideal scenario where the cpu is both instruction to data and data to instruction
        // coherent, we only need a data barrier and instruction barrier to keep the cpu from
        // prefetching around the barrier.
        // NOTE 1: if DIC is set, then we do not need to concern ourselves with aliased pages,
        // and thus VIPT and PIPT are not relevant.
        // NOTE 2: if DIC is 1 then IDC must also be 1.
        if dic {
            isb();
            return;
        }

        // If we're here and `need_invalidate` is set we must be VIPT and we do not know all of
        // the virtual aliases of the sections we cleaned so our only option is to
        // dump the entire icache.
        invalidate_global_instruction_cache();
        self.need_invalidate = false;
    }
}

/// Zeroes a single `page::SIZE` page using `dc zva`.
///
/// # Safety
/// `_ptr` must point to a valid writable page-aligned (`page::SIZE`) Normal Cacheable memory
/// region of size `page::SIZE`, and `arm64_cache_features.zva_size` must have been initialized to
/// a non-zero power-of-two cache block size `<= page::SIZE`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arch_zero_page(_ptr: *mut core::ffi::c_void) {
    // SAFETY: Only the boot CPU's `arm64_feature_init` writes `arm64_cache_features`, before
    // secondary CPUs start and before `arch_zero_page` is called, so no write races with this read.
    let zva_size = unsafe { super::feature::arm64_cache_features.zva_size } as usize;

    let mut ptr = _ptr.expose_provenance();
    debug_assert!(
        zva_size != 0
            && zva_size <= page::SIZE
            && page::SIZE.is_multiple_of(zva_size)
            && is_rounded(ptr, page::SIZE)
    );
    let end_ptr = ptr + page::SIZE;
    while ptr < end_ptr {
        // SAFETY: Caller guarantees `_ptr.._ptr + page::SIZE` is valid Normal Cacheable page
        // memory, and `ptr` remains aligned within `_ptr..end_ptr`.
        unsafe {
            core::arch::asm!("dc zva, {p}", p = in(reg) ptr, options(nostack, preserves_flags));
        }
        ptr += zva_size;
    }
}

/// Translates `va` using hardware `AT` instructions and writes the resulting physical address to
/// `*pa`.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_mmu_translate(
    va: VAddr,
    pa: &mut PAddr,
    user: bool,
    write: bool,
) -> Result<(), Status> {
    // disable interrupts around this operation to make the at/par instruction combination atomic
    let va = va.0;
    let par: u64;
    {
        let _irqd = crate::arch_rs::InterruptDisableGuard::new();

        // SAFETY: `AT` writes `PAR_EL1` without dereferencing memory; `_irqd` keeps local IRQs
        // disabled through the `PAR_EL1` read.
        unsafe {
            if user {
                if write {
                    core::arch::asm!(
                        "at s1e0w, {va}",
                        va = in(reg) va,
                        options(nostack, preserves_flags)
                    );
                } else {
                    core::arch::asm!(
                        "at s1e0r, {va}",
                        va = in(reg) va,
                        options(nostack, preserves_flags)
                    );
                }
            } else if write {
                core::arch::asm!(
                    "at s1e1w, {va}",
                    va = in(reg) va,
                    options(nostack, preserves_flags)
                );
            } else {
                core::arch::asm!(
                    "at s1e1r, {va}",
                    va = in(reg) va,
                    options(nostack, preserves_flags)
                );
            }
        }

        // Explicitly synchronize before reading from the PAR register, as documented as a
        // general requirement after modifying a control register that affects the result
        // of another one.
        isb();

        par = arm_rsr64!("par_el1");
    }

    // if bit 0 is clear, the translation succeeded
    if (par & 1) != 0 {
        return Err(Status::NOT_FOUND);
    }

    // physical address is stored in bits [51..12], naturally aligned
    let paddr = ((par & 0x000f_ffff_ffff_f000) as usize) | (va & (page::SIZE - 1));
    *pa = PAddr(paddr);

    Ok(())
}

/// Captures boot `TTBR0_EL1`/`TTBR1_EL1` root page table addresses and initializes the ASID
/// allocator.
///
/// # Safety
/// Must be called at most once during single-threaded early boot on the boot CPU after CPU MMU
/// features (`arm64_mmu_features`) have been initialized and before secondary CPUs are started.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_mmu_early_init() {
    root_lower_page_table_phys.store(TTBR0_EL1.read().addr(), Ordering::Relaxed);
    root_kernel_page_table_phys.store(TTBR1_EL1.read().addr(), Ordering::Relaxed);

    // SAFETY: Only the boot CPU's `arm64_feature_init` writes `arm64_mmu_features`, before any
    // other CPU starts and before `arm64_mmu_early_init` runs, so no write races with this read.
    let asid_width = unsafe { super::feature::arm64_mmu_features.asid_width };
    debug_assert_ne!(asid_width, arm64_asid_width::UNKNOWN);

    // Our current ASID allocation scheme is very naive and allocates a unique ASID to every
    // address space, which means that there are often not enough ASIDs when the machine uses 8-bit
    // ASIDs. Therefore, if we detect that we are only given 8-bit ASIDs, disable their use.
    let enabled = boot_options::BootOptions::get().arm64_enable_asid
        && asid_width != arm64_asid_width::ASID_8;
    FEAT_ASID_ENABLED.store(enabled, Ordering::Relaxed);

    // After we've probed the feature set and parsed the boot options, initialize the asid
    // allocator.
    if enabled {
        // SAFETY: Caller guarantees single-threaded one-time boot CPU execution after CPU features
        // have been initialized and before any concurrent ASID allocator accesses.
        unsafe { asid_allocator_init() };
    } else {
        dprintf!(INFO, "mmu: not using ASIDs\n");
    }
}

/// Reports supported user address tagging hardware features
/// (`ZX_ARM64_FEATURE_ADDRESS_TAGGING_TBI`).
#[unsafe(no_mangle)]
pub extern "C" fn arch_address_tagging_features() -> u32 {
    zr::static_assert!((MMU_TCR_FLAGS_USER & MMU_TCR_TBI0) != 0);
    ZX_ARM64_FEATURE_ADDRESS_TAGGING_TBI
}

zr::static_assert_size_and_align!(
    ArmArchVmAspace,
    bindings::kArm64ArchVmAspaceStateSize,
    bindings::kArm64ArchVmAspaceStateAlign
);
zr::static_assert_size_and_align!(
    ArmVmICacheConsistencyManager,
    bindings::kArm64VmICacheConsistencyManagerStateSize,
    bindings::kArm64VmICacheConsistencyManagerStateAlign
);

#[cfg(ktest)]
/// arm64 mmu tests
#[unittest::suite(name = "arm64_mmu")]
mod tests {
    use super::super::{USER_ASPACE_BASE, USER_ASPACE_SIZE};
    use unittest::{assert_false, assert_ok, expect_eq, expect_ok, expect_true, subtest};

    const TEST_VIRTUAL_ADDRESS: usize = 1usize << 30; // arbitrary address

    #[doc = "perms"]
    #[test]
    fn perms() {
        stack_pin_init!(
            let aspace = ArmArchVmAspace::new(
                USER_ASPACE_BASE,
                USER_ASPACE_SIZE,
                ArmAspaceType::kUser,
                None,
            )
        );
        // SAFETY: `aspace` is freshly constructed with valid user range `[USER_ASPACE_BASE, ...)`.
        expect_ok!(unsafe { aspace.init() });

        let map_query_test = subtest!(|mmu_perms: ArchMmuFlags| {
            let (vm_page, pa_typed) = pmm::alloc_page(0).expect("pmm::alloc_page failed");
            let pa = pa_typed.0 as u64;
            // SAFETY: `aspace` is initialized and never switched onto a CPU; `pa` is owned here.
            expect_ok!(unsafe {
                aspace.map(TEST_VIRTUAL_ADDRESS, &[pa], mmu_perms, ExistingEntryAction::Error)
            });

            let mut query_pa = 0;
            let mut query_flags = 0;
            expect_ok!(aspace.query(TEST_VIRTUAL_ADDRESS).map(|(pa_out, flags_out)| {
                query_pa = pa_out;
                query_flags = flags_out;
            }));
            expect_eq!(pa, query_pa);
            expect_eq!(mmu_perms, query_flags);

            // FUTURE ENHANCEMENT: use a private api to read the terminal page table entry
            // and validate the bits that were set.

            // SAFETY: `aspace` is inactive so unmapping `TEST_VIRTUAL_ADDRESS` affects no runner.
            let unmap_res =
                unsafe { aspace.unmap(TEST_VIRTUAL_ADDRESS, 1, ARCH_UNMAP_OPTION_NONE) };
            expect_ok!(unmap_res);
            if unmap_res.is_ok() {
                // SAFETY: `vm_page` was allocated exclusively for this iteration and unmapped.
                unsafe { pmm::free_page(vm_page) };
            }
        });

        // map nox page, query to see that X bit isn't set
        expect_true!(map_query_test(ARCH_MMU_FLAG_PERM_READ));
        expect_true!(map_query_test(ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE));
        expect_true!(map_query_test(ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE));
        expect_true!(map_query_test(
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE
        ));

        // map X page, query to see that X bit is set
        expect_true!(map_query_test(ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ));
        expect_true!(map_query_test(
            ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE
        ));
        expect_true!(map_query_test(
            ARCH_MMU_FLAG_PERM_USER | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE
        ));
        expect_true!(map_query_test(
            ARCH_MMU_FLAG_PERM_USER
                | ARCH_MMU_FLAG_PERM_READ
                | ARCH_MMU_FLAG_PERM_WRITE
                | ARCH_MMU_FLAG_PERM_EXECUTE
        ));

        // TODO: https://fxbug.dev/42169684 Add a more comprehensive test that reads back the page
        // table entries and all the translation tables leading up to it to make sure the
        // permission bits are set properly. Requires plumbing through some code to the
        // ArmArchVmAspace to return a copy of all the levels of the translation tables and
        // terminal entry.

        // SAFETY: `aspace` was never active on any CPU and all mappings have been unmapped.
        expect_ok!(unsafe { aspace.destroy() });
    }

    #[doc = "destroy-without-init"]
    #[test]
    fn destroy_without_init() {
        // See that it's OK to Destroy even if Init was never called.
        stack_pin_init!(
            let aspace = ArmArchVmAspace::new(
                USER_ASPACE_BASE,
                USER_ASPACE_SIZE,
                ArmAspaceType::kUser,
                None,
            )
        );
        // SAFETY: `aspace` is local, inactive, and `destroy()` permits uninitialized aspaces.
        assert_ok!(unsafe { aspace.destroy() });

        // See that double Destroy is also OK.
        // SAFETY: `aspace` remains local and inactive with `tt_virt == null`.
        assert_ok!(unsafe { aspace.destroy() });
    }

    #[doc = "non-physmap-exec"]
    #[test]
    fn non_physmap_exec() {
        stack_pin_init!(
            let aspace = ArmArchVmAspace::new(
                USER_ASPACE_BASE,
                USER_ASPACE_SIZE,
                ArmAspaceType::kUser,
                None,
            )
        );
        // SAFETY: `aspace` is freshly constructed with valid user range `[USER_ASPACE_BASE, ...)`.
        assert_ok!(unsafe { aspace.init() });

        // Choose a physical address outside the physmap range to verify that cache maintenance
        // during executable mappings does not attempt to dereference an unmapped physmap virtual
        // address.
        // SAFETY: `gPhysmapSize` is a boot-initialized read-only global variable.
        let pa = page::round_up(unsafe { bindings::gPhysmapSize }) as u64;
        assert_false!(is_physmap_phys_addr(PAddr(pa as usize)));

        const RX_FLAGS: ArchMmuFlags =
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_EXECUTE | ARCH_MMU_FLAG_CACHED;
        const RO_FLAGS: ArchMmuFlags = ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_CACHED;

        // SAFETY: `aspace` is never switched onto a CPU so no hardware dereferences `pa`.
        expect_ok!(unsafe {
            aspace.map(TEST_VIRTUAL_ADDRESS, &[pa], RX_FLAGS, ExistingEntryAction::Error)
        });
        let mut query_pa = 0;
        let mut query_flags = 0;
        expect_ok!(aspace.query(TEST_VIRTUAL_ADDRESS).map(|(pa_out, flags_out)| {
            query_pa = pa_out;
            query_flags = flags_out;
        }));
        expect_eq!(pa, query_pa);
        expect_eq!(RX_FLAGS, query_flags);
        // SAFETY: `aspace` is local and inactive.
        expect_ok!(unsafe { aspace.unmap(TEST_VIRTUAL_ADDRESS, 1, ARCH_UNMAP_OPTION_NONE) });

        // SAFETY: `aspace` is never switched onto a CPU so no hardware dereferences `pa`.
        expect_ok!(unsafe { aspace.map_contiguous(TEST_VIRTUAL_ADDRESS, pa, 1, RX_FLAGS) });
        // SAFETY: `aspace` is local and inactive.
        expect_ok!(unsafe { aspace.unmap(TEST_VIRTUAL_ADDRESS, 1, ARCH_UNMAP_OPTION_NONE) });

        // SAFETY: `aspace` is never switched onto a CPU so no hardware dereferences `pa`.
        expect_ok!(unsafe {
            aspace.map(TEST_VIRTUAL_ADDRESS, &[pa], RO_FLAGS, ExistingEntryAction::Error)
        });
        // SAFETY: `aspace` is local and inactive.
        expect_ok!(unsafe {
            aspace.protect(TEST_VIRTUAL_ADDRESS, 1, RX_FLAGS, ARCH_UNMAP_OPTION_NONE)
        });
        // SAFETY: `aspace` is local and inactive.
        expect_ok!(unsafe { aspace.unmap(TEST_VIRTUAL_ADDRESS, 1, ARCH_UNMAP_OPTION_NONE) });

        // SAFETY: `aspace` was never active on any CPU and all mappings have been unmapped.
        expect_ok!(unsafe { aspace.destroy() });
    }
}
