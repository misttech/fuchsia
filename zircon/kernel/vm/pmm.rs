// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::pmm_arena::PmmArenaInfo;
use super::pmm_node::PmmNode;
use crate::kernel::types::PAddr;
use crate::vm::page::{VmPage, VmPagePtr};
use crate::vm::page_queues::PageQueues;
use core::mem::MaybeUninit;
use core::pin::Pin;
use fbl::DoublyLinkedList;
use pmm_bindings as bindings;
use zx_status::Status;

// Flags for PMM allocation routines.
pub const ALLOC_FLAG_ANY: u32 = bindings::PMM_ALLOC_FLAG_ANY;
pub const ALLOC_FLAG_CAN_WAIT: u32 = bindings::PMM_ALLOC_FLAG_CAN_WAIT;

// One of the members of the PmmNode has a zero sized type, which is not FFI compliant, however we
// are separately validating the equivalence of the final layout of the C++ and Rust objects so
// this is okay.
#[allow(improper_ctypes)]
unsafe extern "C" {
    // C++ name mangled form of `PmmNode Pmm::node_`
    #[link_name = "_ZN3Pmm5node_E"]
    static PMM_NODE: PmmNode;
}

/// Allocates a single physical page from the PMM.
pub fn alloc_page(flags: u32) -> Result<(VmPagePtr, PAddr), Status> {
    let mut page = core::ptr::null_mut();
    let mut paddr: bindings::zx_paddr_t = 0;
    // SAFETY: FFI call passing valid stack addresses to store the page pointer and physical address.
    let status = unsafe { bindings::cpp_pmm_alloc_page(flags, &mut page, &mut paddr) };
    Status::ok(status)?;
    // SAFETY: `page` is a valid page pointer returned by the PMM on success.
    let page_ptr = unsafe { VmPagePtr::from_ffi(page) }.ok_or(Status::NO_MEMORY)?;
    Ok((page_ptr, PAddr(paddr)))
}

/// Frees a single physical page back to the PMM.
///
/// # Safety
///
/// Caller must ensure `page` is a valid allocated PMM page that has not already been freed.
pub unsafe fn free_page(page: VmPagePtr) {
    // SAFETY: Caller guarantees `page` is a valid allocated PMM page.
    unsafe { bindings::cpp_pmm_free_page(page.as_ffi()) };
}

/// Frees every page on `list` back to the PMM, emptying the list.  The PMM lock
/// is taken once for the whole list rather than once per page.
///
/// # Safety
///
/// Caller must ensure every page on the list is a valid allocated PMM page that
/// has not already been freed.
pub unsafe fn free_list(list: Pin<&mut DoublyLinkedList<*mut VmPage>>) {
    if list.is_empty() {
        return;
    }
    // SAFETY: `DoublyLinkedList` is `repr(C)` and mirrors `VmPageDoublyLinkedList`
    // -- a bare head pointer with the same sentinel encoding -- so C++ can drain it
    // in place.  The caller guarantees the pages.
    unsafe {
        bindings::cpp_pmm_free_list(
            (list.get_unchecked_mut() as *mut DoublyLinkedList<*mut VmPage>).cast(),
        )
    };
}

/// Converts a physical address to a `VmPagePtr`.
pub fn paddr_to_vm_page(paddr: PAddr) -> Option<VmPagePtr> {
    let raw = unsafe { bindings::cpp_paddr_to_vm_page(paddr.0) };
    // SAFETY: cpp_paddr_to_vm_page returns a valid VmPagePtr, or null.
    unsafe { VmPagePtr::from_ffi(raw) }
}

/// Allocate count pages of physical memory, adding to the tail of the passed list.
/// The list must be initialized.
/// Note that if PMM_ALLOC_FLAG_CAN_WAIT is passed in then this could always return
/// ZX_ERR_SHOULD_WAIT. Since there is no way to wait until an arbitrary number of pages can be
/// allocated (see comment on |pmm_wait_till_should_retry_single_alloc|) passing
/// PMM_ALLOC_FLAG_CAN_WAIT here should be used as an optimistic fast path, and the caller should
/// have a fallback of allocating single pages.
pub fn alloc_pages(
    count: usize,
    flags: u32,
    list: Pin<&mut DoublyLinkedList<*mut VmPage>>,
) -> Result<(), Status> {
    // SAFETY: FFI call passing pointer to `list`.
    let status = unsafe {
        bindings::cpp_pmm_alloc_pages(
            count,
            flags,
            (list.get_unchecked_mut() as *mut DoublyLinkedList<*mut VmPage>).cast(),
        )
    };
    Status::ok(status)
}

/// Allocate a run of contiguous pages, aligned on log2 byte boundary (0-31).
/// Return the base address of the run in the physical address pointer and
/// append the allocate page structures to the tail of the passed in list.
pub fn alloc_contiguous(
    count: usize,
    flags: u32,
    align_log2: u8,
    list: Pin<&mut DoublyLinkedList<*mut VmPage>>,
) -> Result<PAddr, Status> {
    let mut pa: bindings::zx_paddr_t = 0;
    // SAFETY: FFI call passing stack pointer for `pa` and pointer to `list`.
    let status = unsafe {
        bindings::cpp_pmm_alloc_contiguous(
            count,
            flags,
            align_log2,
            &mut pa,
            (list.get_unchecked_mut() as *mut DoublyLinkedList<*mut VmPage>).cast(),
        )
    };
    Status::ok(status)?;
    Ok(PAddr(pa))
}

/// Returns the number of physical memory arenas.
pub fn num_arenas() -> usize {
    // SAFETY: No preconditions.
    unsafe { bindings::cpp_pmm_num_arenas() }
}

// Fills |buffer| with PmmArenaInfo objects starting at |offset| arena, ordered by base address.
// For example, passing an |offset| of 1 would skip the 1st arena.
//
// Returns OUT_OF_RANGE if |offset| would yield an invalid range or |buffer| is too large.
//
// Returns BUFFER_TOO_SMALL if the |buffer| is too small.
pub fn get_arena_info(
    offset: usize,
    buffer: &mut [MaybeUninit<PmmArenaInfo>],
) -> Result<&mut [PmmArenaInfo], Status> {
    // SAFETY: FFI call passing point to |buffer|.
    let status = unsafe {
        bindings::cpp_pmm_get_arena_info(
            buffer.len(),
            offset as u64,
            buffer.as_mut_ptr().cast(),
            buffer.len() * core::mem::size_of::<PmmArenaInfo>(),
        )
    };
    Status::ok(status)?;
    // SAFETY: cpp_pmm_get_arena_info fully initializes this buffer on success.
    unsafe { Ok(buffer.assume_init_mut()) }
}

/// Returns the static `PageQueues` instance associated with the PMM.
pub fn page_queues() -> &'static PageQueues {
    // SAFETY: No preconditions.
    let queues = unsafe { bindings::cpp_pmm_page_queues() };
    let queues: *const PageQueues = queues.cast();
    // SAFETY: `cpp_pmm_page_queues` returns a valid static pointer to the global PmmNode's
    // PageQueues.
    unsafe { queues.as_ref_unchecked() }
}

/// Returns a reference to the global `PmmNode` instance.
pub fn node() -> &'static PmmNode {
    unsafe { &PMM_NODE }
}
