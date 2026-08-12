// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::page_state::VmPageState;
use crate::kernel::types::PAddr;
use core::ptr::NonNull;
use page_bindings as bindings;
use zr::Opaque;

pub use bindings::vm_page_t;

pub mod object {
    use page_bindings as bindings;

    pub const PIN_COUNT_BITS: u32 = bindings::VM_PAGE_OBJECT_PIN_COUNT_BITS;
    pub const MAX_PIN_COUNT: u32 = bindings::VM_PAGE_OBJECT_MAX_PIN_COUNT;
    pub const DIRTY_STATE_BITS: u32 = bindings::VM_PAGE_OBJECT_DIRTY_STATE_BITS;
    pub const MAX_DIRTY_STATES: u32 = bindings::VM_PAGE_OBJECT_MAX_DIRTY_STATES;
    pub const DIRTY_STATES_MASK: u32 = bindings::VM_PAGE_OBJECT_DIRTY_STATES_MASK;
}

/// Type-safe wrapper around a raw pointer to a kernel page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmPagePtr(NonNull<Opaque<bindings::vm_page_t>>);

impl VmPagePtr {
    /// Creates a `VmPagePtr` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a kernel page.
    pub const unsafe fn from_raw(ptr: *mut bindings::vm_page_t) -> Option<Self> {
        // `bindings::vm_page_t` and `Opaque<bindings::vm_page_t>` are layout compatible.
        let ptr: *mut Opaque<bindings::vm_page_t> = ptr.cast();
        match NonNull::new(ptr) {
            Some(nn) => Some(Self(nn)),
            None => None,
        }
    }

    /// Returns the raw pointer.
    pub fn as_raw(self) -> *mut bindings::vm_page_t {
        let ptr: *mut Opaque<bindings::vm_page_t> = self.0.as_ptr();
        // `Opaque<bindings::vm_page_t>` and `bindings::vm_page_t` are layout compatible.
        ptr.cast()
    }

    /// Returns whether this page is in the FREE state. When in the FREE state the page is assumed
    /// to be owned by the relevant PmmNode, and hence unless its lock is held this query must be
    /// assumed to be racy.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it either still has ownership of the page or knows it is safe to
    /// inspect the state.
    pub unsafe fn is_free(self) -> bool {
        // SAFETY: The caller guarantees via function safety preconditions that it is safe to
        // inspect the page state.
        unsafe { self.state().0 == bindings::vm_page_state::FREE }
    }

    /// Returns whether this page is in the FREE_LOANED state. Similar to the FREE state the page is
    /// assumed to be owned by the relevant PmmNode, however this distinguishes whether the page is
    /// part of the general purpose free list, versus the more narrowly usable set of loaned pages.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it either still has ownership of the page or knows it is safe to
    /// inspect the state.
    pub unsafe fn is_free_loaned(self) -> bool {
        // SAFETY: The caller guarantees via function safety preconditions that it is safe to
        // inspect the page state.
        unsafe { self.state().0 == bindings::vm_page_state::FREE_LOANED }
    }

    /// If true, this page is "loaned" in the sense of being loaned from a contiguous VMO (via
    /// decommit) to Zircon.  If the original contiguous VMO is deleted, this page will no longer be
    /// loaned.  A loaned page cannot be pinned.  Instead a different physical page (non-loaned) is
    /// used for the pin.  A loaned page can be (re-)committed back into its original contiguous
    /// VMO, which causes the data in the loaned page to be moved into a different physical page
    /// (which itself can be non-loaned or loaned).  A loaned page cannot be used to allocate a new
    /// contiguous VMO. Maybe queried by anyone who either owns the page, or has sufficient
    /// knowledge that the loaned state cannot be being altered in parallel.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it either still has ownership of the page or knows it is safe to
    /// inspect the state.
    pub unsafe fn is_loaned(self) -> bool {
        // SAFETY: The caller guarantees via function safety preconditions that it is safe to
        // inspect the loaned state.
        unsafe { bindings::cpp_vm_page_is_loaned(self.as_raw()) }
    }

    /// If true, the original contiguous VMO wants the page back.  Such pages won't be reused until
    /// the page is no longer loaned, either via commit of the page back into the contiguous VMO
    /// that loaned the page, or via deletion of the contiguous VMO that loaned the page. Such pages
    /// are not in the free_loaned_list_ in pmm, which is how reuse is prevented. Should only be
    /// called by the PmmNode under its lock.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it either still has ownership of the page or knows it is safe to
    /// inspect the state.
    pub unsafe fn is_loan_cancelled(self) -> bool {
        // SAFETY: The caller guarantees via function safety preconditions that it is safe to
        // inspect the loaned state.
        unsafe { bindings::cpp_vm_page_is_loan_cancelled(self.as_raw()) }
    }

    /// Sets the loaned flag on the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns the page and holds the loaned pages lock of the PmmNode
    pub unsafe fn set_is_loaned(self) {
        // SAFETY: The caller guarantees ownership of the page and holds the necessary PmmNode lock.
        unsafe { bindings::cpp_vm_page_set_is_loaned(self.as_raw()) }
    }
    /// Clears the loaned flag on the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns the page and holds the loaned pages lock of the PmmNode
    pub unsafe fn clear_is_loaned(self) {
        // SAFETY: The caller guarantees ownership of the page and holds the necessary PmmNode lock.
        unsafe { bindings::cpp_vm_page_clear_is_loaned(self.as_raw()) }
    }

    /// Sets the loan_cancelled flag on the page. May be done even if not the owner of the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it holds the loaned pages lock of the PmmNode
    pub unsafe fn set_is_loan_cancelled(self) {
        // SAFETY: The caller guarantees holding the necessary PmmNode lock.
        unsafe { bindings::cpp_vm_page_set_is_loan_cancelled(self.as_raw()) }
    }
    /// Clears the loan_cancelled flag on the page. May be done even if not the owner of the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it holds the loaned pages lock of the PmmNode
    pub unsafe fn clear_is_loan_cancelled(self) {
        // SAFETY: The caller guarantees holding the necessary PmmNode lock.
        unsafe { bindings::cpp_vm_page_clear_is_loan_cancelled(self.as_raw()) }
    }

    /// Dumps information about the page to the debuglog.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it either still has ownership of the page or knows it is safe to
    /// access the pages state.
    pub unsafe fn dump(self) {
        // SAFETY: The caller guarantees via function safety preconditions that it is safe to access
        // the page state.
        unsafe { bindings::cpp_vm_page_dump(self.as_raw()) }
    }

    /// Return the physical address of the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it either still has ownership of the page or knows it is safe to
    /// inspect the state.
    pub unsafe fn paddr(self) -> PAddr {
        // SAFETY: The caller guarantees via function safety preconditions that it is safe to
        // inspect the page state.
        unsafe { PAddr(bindings::cpp_vm_page_paddr(self.as_raw())) }
    }

    /// Returns the backlink object pointer for the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the page is attached to a VM object.
    pub unsafe fn get_object(self) -> *mut core::ffi::c_void {
        // SAFETY: Safety deferred to caller per function safety preconditions.
        unsafe { bindings::cpp_vm_page_object_get_object(self.as_raw()) }
    }

    /// Returns the page offset in the backlink object for the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the page is attached to a VM object.
    pub unsafe fn get_page_offset(self) -> u64 {
        // SAFETY: Safety deferred to caller per function safety preconditions.
        unsafe { bindings::cpp_vm_page_object_get_page_offset(self.as_raw()) }
    }

    /// Return the current VmPageState of this page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it either still has ownership of the page or knows it is safe to
    /// inspect the state.
    pub unsafe fn state(self) -> VmPageState {
        // SAFETY: The caller guarantees via function safety preconditions that it is safe to
        // inspect the page state.
        let state = unsafe { bindings::cpp_vm_page_state(self.as_raw()) };
        VmPageState(state)
    }

    /// Sets the VmPageState of this page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns the page or holds the necessary locks to modify its
    /// state.
    pub unsafe fn set_state(self, new_state: VmPageState) {
        // SAFETY: The caller guarantees ownership of the page or holding the necessary locks to
        // modify its state.
        unsafe { bindings::cpp_vm_page_set_state(self.as_raw(), new_state.0) }
    }
}

// Return the approximate number of pages in state |state|.
//
// When called concurrently with |set_state|, the count may be off by a small amount.
#[inline]
pub fn get_count(state: VmPageState) -> u64 {
    // SAFETY: cpp_get_count is a thread-safe FFI call that disables preemption and reads atomic
    // per-CPU counters.
    unsafe { bindings::cpp_get_count(state.0) }
}

// Add |n| to the count of pages in state |state|.
//
// Should be used when first constructing pages.
#[inline]
pub fn add_to_initial_count(state: VmPageState, n: u64) {
    // SAFETY: cpp_add_to_initial_count is a thread-safe FFI call that disables preemption and
    // modifies atomic per-CPU counters during initialization.
    unsafe {
        bindings::cpp_add_to_initial_count(state.0, n);
    }
}
