// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::page_state::VmPageState;
use core::ffi::c_void;
use core::ptr::NonNull;
use kernel::types::PAddr;

unsafe extern "C" {
    fn cpp_get_count(state: VmPageState) -> u64;
    fn cpp_add_to_initial_count(state: VmPageState, n: u64);
    fn cpp_vm_page_is_loaned(page: *mut c_void) -> bool;
    fn cpp_vm_page_is_loan_cancelled(page: *mut c_void) -> bool;
    fn cpp_vm_page_set_is_loaned(page: *mut c_void);
    fn cpp_vm_page_clear_is_loaned(page: *mut c_void);
    fn cpp_vm_page_set_is_loan_cancelled(page: *mut c_void);
    fn cpp_vm_page_clear_is_loan_cancelled(page: *mut c_void);
    fn cpp_vm_page_dump(page: *mut c_void);
    fn cpp_vm_page_paddr(page: *mut c_void) -> PAddr;
    fn cpp_vm_page_state(page: *mut c_void) -> VmPageState;
    fn cpp_vm_page_set_state(page: *mut c_void, new_state: VmPageState);
}

/// Type-safe wrapper around a raw pointer to a kernel page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VmPagePtr(NonNull<c_void>);

impl VmPagePtr {
    /// Creates a `VmPagePtr` from a raw pointer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `ptr` is a valid pointer to a kernel page.
    pub const unsafe fn from_raw(ptr: *mut c_void) -> Option<Self> {
        match NonNull::new(ptr) {
            Some(nn) => Some(Self(nn)),
            None => None,
        }
    }

    /// Returns the raw pointer.
    pub fn as_raw(self) -> *mut c_void {
        self.0.as_ptr()
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
        unsafe { self.state() == VmPageState::Free }
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
        unsafe { self.state() == VmPageState::FreeLoaned }
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
        unsafe { cpp_vm_page_is_loaned(self.as_raw()) }
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
        unsafe { cpp_vm_page_is_loan_cancelled(self.as_raw()) }
    }

    /// Sets the loaned flag on the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns the page and holds the loaned pages lock of the PmmNode
    pub unsafe fn set_is_loaned(self) {
        // SAFETY: The caller guarantees ownership of the page and holds the necessary PmmNode lock.
        unsafe { cpp_vm_page_set_is_loaned(self.as_raw()) }
    }
    /// Clears the loaned flag on the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it owns the page and holds the loaned pages lock of the PmmNode
    pub unsafe fn clear_is_loaned(self) {
        // SAFETY: The caller guarantees ownership of the page and holds the necessary PmmNode lock.
        unsafe { cpp_vm_page_clear_is_loaned(self.as_raw()) }
    }

    /// Sets the loan_cancelled flag on the page. May be done even if not the owner of the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it holds the loaned pages lock of the PmmNode
    pub unsafe fn set_is_loan_cancelled(self) {
        // SAFETY: The caller guarantees holding the necessary PmmNode lock.
        unsafe { cpp_vm_page_set_is_loan_cancelled(self.as_raw()) }
    }
    /// Clears the loan_cancelled flag on the page. May be done even if not the owner of the page.
    ///
    /// # Safety
    ///
    /// The caller must ensure that it holds the loaned pages lock of the PmmNode
    pub unsafe fn clear_is_loan_cancelled(self) {
        // SAFETY: The caller guarantees holding the necessary PmmNode lock.
        unsafe { cpp_vm_page_clear_is_loan_cancelled(self.as_raw()) }
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
        unsafe { cpp_vm_page_dump(self.as_raw()) }
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
        unsafe { cpp_vm_page_paddr(self.as_raw()) }
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
        unsafe { cpp_vm_page_state(self.as_raw()) }
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
        unsafe { cpp_vm_page_set_state(self.as_raw(), new_state) }
    }
}

// Return the approximate number of pages in state |state|.
//
// When called concurrently with |set_state|, the count may be off by a small amount.
#[inline]
pub fn get_count(state: VmPageState) -> u64 {
    // SAFETY: cpp_get_count is a thread-safe FFI call that disables preemption and reads atomic
    // per-CPU counters.
    unsafe { cpp_get_count(state) }
}

// Add |n| to the count of pages in state |state|.
//
// Should be used when first constructing pages.
#[inline]
pub fn add_to_initial_count(state: VmPageState, n: u64) {
    // SAFETY: cpp_add_to_initial_count is a thread-safe FFI call that disables preemption and
    // modifies atomic per-CPU counters during initialization.
    unsafe {
        cpp_add_to_initial_count(state, n);
    }
}
