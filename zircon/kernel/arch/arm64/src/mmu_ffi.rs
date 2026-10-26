// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Zero-logic C FFI shims forwarding `<arch/aspace.h>` to `super::mmu`.

use super::mmu::{
    ArmArchVmAspace, ArmAspaceType, ArmVmICacheConsistencyManager, PageAllocFn,
    aspace_type_from_flags,
};
use crate::vm::arch_vm_aspace::{ExistingEntryAction, NonTerminalAction, TerminalAction};
use crate::vm::page::VmPageDoublyLinkedList;
use zx_status::Status;

/// Initializes an `ArmArchVmAspace` in place with an explicit address space type.
///
/// # Safety
/// `ptr` must be non-null, aligned to `align_of::<ArmArchVmAspace>()`, valid for writes of
/// `size_of::<ArmArchVmAspace>()` bytes, and remain pinned in place without moving until
/// `rust_arm64_aspace_destruct` is called on it.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_construct(
    ptr: *mut ArmArchVmAspace,
    base: usize,
    size: usize,
    aspace_type: ArmAspaceType,
    paf: Option<PageAllocFn>,
) {
    // SAFETY: Caller guarantees `ptr` is valid, aligned, pinned storage for `ArmArchVmAspace`.
    unsafe { ArmArchVmAspace::init_in_place(ptr, base, size, aspace_type, paf) };
}

/// Initializes an `ArmArchVmAspace` in place using architectural MMU flags.
///
/// # Safety
/// `ptr` must be non-null, aligned to `align_of::<ArmArchVmAspace>()`, valid for writes of
/// `size_of::<ArmArchVmAspace>()` bytes, and remain pinned in place without moving until
/// `rust_arm64_aspace_destruct` is called on it.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_construct_flags(
    ptr: *mut ArmArchVmAspace,
    base: usize,
    size: usize,
    mmu_flags: u8,
    paf: Option<PageAllocFn>,
) {
    // SAFETY: Caller guarantees `ptr` is valid, aligned, pinned storage for `ArmArchVmAspace`.
    unsafe {
        ArmArchVmAspace::init_in_place(ptr, base, size, aspace_type_from_flags(mmu_flags), paf)
    };
}

/// Drops an `ArmArchVmAspace` in place.
///
/// # Safety
/// `ptr` must point to a live pinned `ArmArchVmAspace` previously initialized via
/// `rust_arm64_aspace_construct*` and not yet dropped, with exclusive access for drop.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_destruct(ptr: *mut ArmArchVmAspace) {
    // SAFETY: Caller guarantees `ptr` is initialized, exclusively owned, and dropped at most once.
    unsafe { core::ptr::drop_in_place(ptr) };
}

/// Initializes a standalone `ArmArchVmAspace`.
///
/// # Safety
/// `aspace` must satisfy `ArmArchVmAspace::init`'s range and boot MMU preconditions.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_init(aspace: &ArmArchVmAspace) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::init` preconditions on `aspace`.
    unsafe { aspace.init() }
}

/// Initializes a shared user `ArmArchVmAspace`.
///
/// # Safety
/// `shared_aspace` must satisfy `ArmArchVmAspace::init_shared`'s user shared range preconditions.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_init_shared(
    shared_aspace: &ArmArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::init_shared` preconditions on `shared_aspace`.
    unsafe { shared_aspace.init_shared() }
}

/// Initializes a restricted user `ArmArchVmAspace`.
///
/// # Safety
/// `aspace` must satisfy `ArmArchVmAspace::init_restricted`'s user restricted range preconditions.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_init_restricted(
    aspace: &ArmArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::init_restricted` preconditions on `aspace`.
    unsafe { aspace.init_restricted() }
}

/// Initializes a unified user `ArmArchVmAspace` backed by `shared` and `restricted`.
///
/// # Safety
/// `aspace`, `shared`, and `restricted` must satisfy `ArmArchVmAspace::init_unified`'s role,
/// disjoint-range, and lifetime preconditions until `aspace.destroy()` completes.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_init_unified(
    aspace: &ArmArchVmAspace,
    shared: &ArmArchVmAspace,
    restricted: &ArmArchVmAspace,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::init_unified` preconditions on all three aspaces.
    unsafe { aspace.init_unified(shared, restricted) }
}

/// Disables further page table updates on `aspace`.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_aspace_disable_updates(aspace: &ArmArchVmAspace) {
    aspace.disable_updates();
}

/// Destroys the translation tables and ASID owned by `aspace`.
///
/// # Safety
/// `aspace` (even if uninitialized or after a partial `init*` failure) must not be active on any
/// CPU, must have no concurrent operations running on it, any unified aspaces referencing `aspace`
/// must already be destroyed, and if `aspace` is unified its constituent `shared` and `restricted`
/// aspaces must still be live and undestroyed.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_destroy(aspace: &ArmArchVmAspace) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::destroy` preconditions on `aspace`.
    unsafe { aspace.destroy() }
}

/// Maps `count` physical pages from `phys` at `vaddr`.
///
/// # Safety
/// When `count > 0 && !phys.is_null()`, `phys` must be aligned to `align_of::<u64>()` and valid
/// for `count` reads of initialized `u64` physical addresses (`count > 0` requires non-null
/// `phys`), and mapping those pages at `vaddr` with `mmu_flags` and `existing_action` must satisfy
/// `ArmArchVmAspace::map`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_map(
    aspace: &ArmArchVmAspace,
    vaddr: usize,
    phys: *const u64,
    count: usize,
    mmu_flags: u8,
    existing_action: ExistingEntryAction,
) -> Result<(), Status> {
    let phys_slice: &[u64] = if count == 0 || phys.is_null() {
        assert_eq!(count, 0, "null phys array with non-zero count");
        &[]
    } else {
        // SAFETY: Caller guarantees `phys` is aligned to `align_of::<u64>()` and valid for `count`
        // reads of initialized `u64` physical addresses.
        unsafe { core::slice::from_raw_parts(phys, count) }
    };
    // SAFETY: Caller upholds `ArmArchVmAspace::map` preconditions for `vaddr` and `phys_slice`.
    unsafe { aspace.map(vaddr, phys_slice, mmu_flags, existing_action) }
}

/// Maps a contiguous run of `count` physical pages starting at `paddr` to `vaddr`.
///
/// # Safety
/// Mapping `[paddr, paddr + count * page::SIZE)` at `vaddr` with `mmu_flags` must satisfy
/// `ArmArchVmAspace::map_contiguous`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_map_contiguous(
    aspace: &ArmArchVmAspace,
    vaddr: usize,
    paddr: u64,
    count: usize,
    mmu_flags: u8,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::map_contiguous` preconditions on `aspace`.
    unsafe { aspace.map_contiguous(vaddr, paddr, count, mmu_flags) }
}

/// Unmaps `count` pages starting at `vaddr`.
///
/// # Safety
/// Unmapping `count` pages starting at `vaddr` with `enlarge` must satisfy
/// `ArmArchVmAspace::unmap`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_unmap(
    aspace: &ArmArchVmAspace,
    vaddr: usize,
    count: usize,
    enlarge: u8,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::unmap` preconditions on `aspace`.
    unsafe { aspace.unmap(vaddr, count, enlarge) }
}

/// Updates permissions on `count` pages starting at `vaddr`.
///
/// # Safety
/// Updating permissions on `count` pages starting at `vaddr` with `mmu_flags` and `enlarge` must
/// satisfy `ArmArchVmAspace::protect`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_protect(
    aspace: &ArmArchVmAspace,
    vaddr: usize,
    count: usize,
    mmu_flags: u8,
    enlarge: u8,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::protect` preconditions on `aspace`.
    unsafe { aspace.protect(vaddr, count, mmu_flags, enlarge) }
}

/// Queries the physical address and MMU flags mapped at `vaddr`.
///
/// # Safety
/// If non-null, `paddr` and `mmu_flags` must be properly aligned (`align_of::<u64>()` and
/// `align_of::<u8>()`), valid for writes, and must not alias `*aspace` or each other.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_query(
    aspace: &ArmArchVmAspace,
    vaddr: usize,
    paddr: *mut u64,
    mmu_flags: *mut u8,
) -> Result<(), Status> {
    let (pa, flags) = aspace.query(vaddr)?;
    if !paddr.is_null() {
        // SAFETY: Caller guarantees non-null `paddr` is aligned, non-aliasing, and valid for
        // writes.
        unsafe { *paddr = pa };
    }
    if !mmu_flags.is_null() {
        // SAFETY: Caller guarantees non-null `mmu_flags` is aligned, non-aliasing, and valid for
        // writes.
        unsafe { *mmu_flags = flags };
    }
    Ok(())
}

/// Picks a virtual address spot within `base..end`.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_aspace_pick_spot(
    aspace: &ArmArchVmAspace,
    base: usize,
    end: usize,
    align: usize,
    size: usize,
    mmu_flags: u8,
) -> usize {
    aspace.pick_spot(base, end, align, size, mmu_flags)
}

/// Marks PTEs in `[vaddr, vaddr + count * page::SIZE)` as accessed.
///
/// # Safety
/// `aspace` must be initialized and `destroy()` must not run concurrently with `mark_accessed`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_mark_accessed(
    aspace: &ArmArchVmAspace,
    vaddr: usize,
    count: usize,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::mark_accessed` preconditions on `aspace`.
    unsafe { aspace.mark_accessed(vaddr, count) }
}

/// Harvests accessed bits across `[vaddr, vaddr + count * page::SIZE)`.
///
/// # Safety
/// Harvesting accessed mappings across `[vaddr, vaddr + count * page::SIZE)` with
/// `non_terminal_action` and `terminal_action` must satisfy `ArmArchVmAspace::harvest_accessed`.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_harvest_accessed(
    aspace: &ArmArchVmAspace,
    vaddr: usize,
    count: usize,
    non_terminal_action: NonTerminalAction,
    terminal_action: TerminalAction,
) -> Result<(), Status> {
    // SAFETY: Caller upholds `ArmArchVmAspace::harvest_accessed` preconditions on `aspace`.
    unsafe { aspace.harvest_accessed(vaddr, count, non_terminal_action, terminal_action) }
}

/// Returns whether the address space has been accessed since the last check.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_aspace_accessed_since_last_check(
    aspace: &ArmArchVmAspace,
    clear: bool,
) -> bool {
    aspace.accessed_since_last_check(clear)
}

/// Returns the root physical translation table address of `aspace`.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_aspace_arch_table_phys(aspace: &ArmArchVmAspace) -> u64 {
    aspace.arch_table_phys()
}

/// Returns the ASID or VMID assigned to `aspace`.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_aspace_arch_asid(aspace: &ArmArchVmAspace) -> u16 {
    aspace.arch_asid()
}

/// Updates the ASID or VMID assigned to `aspace`.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_aspace_arch_set_asid(aspace: &ArmArchVmAspace, asid: u16) {
    aspace.arch_set_asid(asid);
}

/// Switches the active user address space hardware context on the current CPU.
///
/// # Safety
/// `from` must match the user address space active on the current CPU (or `None` for kernel-only),
/// `to` must be an initialized user `ArmArchVmAspace` (or `None`), and local interrupts/preemption
/// state must permit hardware `TTBR0_EL1`/`TCR_EL1` updates on the current CPU.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_context_switch(
    from: Option<&ArmArchVmAspace>,
    to: Option<&ArmArchVmAspace>,
) {
    // SAFETY: Caller upholds `ArmArchVmAspace::context_switch` preconditions on the current CPU.
    unsafe { ArmArchVmAspace::context_switch(from, to) };
}

/// Transfers ownership of physboot-constructed kernel page tables to the MMU subsystem.
///
/// # Safety
/// `mmu_pages` must be non-null, aligned, and point to a pinned `VmPageDoublyLinkedList` of
/// physmap-mapped boot MMU page table pages exclusively owned during single-threaded kernel
/// handoff.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_aspace_handoff_page_tables_from_physboot(
    mmu_pages: *mut VmPageDoublyLinkedList,
) {
    // SAFETY: Caller guarantees `mmu_pages` points to a valid pinned list of boot MMU pages.
    unsafe {
        let pinned = core::pin::Pin::new_unchecked(&mut *mmu_pages);
        ArmArchVmAspace::handoff_page_tables_from_physboot(pinned);
    }
}

/// Forces subsequent `sync_addr` calls on `state` to clean data caches to PoC.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_icache_force_clean_to_poc(state: &mut ArmVmICacheConsistencyManager) {
    state.force_clean_to_poc();
}

/// Cleans/invalidates caches across kernel virtual range `[start, start + len)`.
///
/// # Safety
/// `[start, start + len)` must be a valid mapped kernel virtual address range.
#[unsafe(no_mangle)]
unsafe extern "C" fn rust_arm64_icache_sync_addr(
    state: &mut ArmVmICacheConsistencyManager,
    start: usize,
    len: usize,
) {
    // SAFETY: Caller guarantees `[start, start + len)` is a valid mapped kernel virtual range.
    unsafe { state.sync_addr(start, len) };
}

/// Finishes any batched instruction cache invalidation and barrier synchronization on `state`.
#[unsafe(no_mangle)]
extern "C" fn rust_arm64_icache_finish(state: &mut ArmVmICacheConsistencyManager) {
    state.finish();
}
