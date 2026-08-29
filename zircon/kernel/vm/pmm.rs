// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::pmm_node::PmmNode;
use crate::kernel::types::PAddr;
use crate::vm::page::{VmPage, VmPagePtr};
use crate::vm::page_queues::PageQueues;
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
pub unsafe fn free_list(list: &mut DoublyLinkedList<*mut VmPage>) {
    if list.is_empty() {
        return;
    }
    // SAFETY: `DoublyLinkedList` is `repr(C)` and mirrors `VmPageDoublyLinkedList`
    // -- a bare head pointer with the same sentinel encoding -- so C++ can drain it
    // in place.  The caller guarantees the pages.
    unsafe { bindings::cpp_pmm_free_list((list as *mut DoublyLinkedList<*mut VmPage>).cast()) };
}

/// Converts a physical address to a `VmPagePtr`.
pub fn paddr_to_vm_page(paddr: PAddr) -> Option<VmPagePtr> {
    let raw = unsafe { bindings::cpp_paddr_to_vm_page(paddr.0) };
    // SAFETY: cpp_paddr_to_vm_page returns a valid VmPagePtr, or null.
    unsafe { VmPagePtr::from_ffi(raw) }
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
