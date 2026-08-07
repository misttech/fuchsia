// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::page::VmPagePtr;
use core::marker::{PhantomData, PhantomPinned};
use page_queues_bindings as bindings;

#[derive(Debug)]
pub struct QueueAge(pub usize);

#[repr(C)]
pub struct PageQueues {
    raw: bindings::PageQueues,
    phantom: PhantomData<PhantomPinned>,
}

impl PageQueues {
    /// Domain-specific conversion: returns raw pointer for `PageQueues`.
    pub fn as_raw(&self) -> *mut bindings::PageQueues {
        core::ptr::from_ref(&self.raw).cast_mut()
    }

    /// Returns whether `page` is in the wired queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is attached to a VM object.
    pub unsafe fn debug_page_is_wired(&self, page: VmPagePtr) -> bool {
        // SAFETY: `self` is valid for required accesses, and the caller guarantees `page` is
        // attached to a VM object per function safety preconditions.
        unsafe { bindings::cpp_page_queues_debug_page_is_wired(self.as_raw(), page.as_raw()) }
    }

    /// Returns whether `page` is in any anonymous queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is attached to a VM object.
    pub unsafe fn debug_page_is_any_anonymous(&self, page: VmPagePtr) -> bool {
        // SAFETY: `self` is valid for required accesses, and the caller guarantees `page` is
        // attached to a VM object per function safety preconditions.
        unsafe {
            bindings::cpp_page_queues_debug_page_is_any_anonymous(self.as_raw(), page.as_raw())
        }
    }

    /// Returns `Some(QueueAge)` if `page` is currently in a reclaim queue, or `None` if it is not.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is attached to a VM object.
    pub unsafe fn debug_page_is_reclaim(&self, page: VmPagePtr) -> Option<QueueAge> {
        let mut age = 0;
        // SAFETY: `self` and `&mut age` are valid for required accesses, and the caller
        // guarantees `page` is attached to a VM object per function safety preconditions.
        let is_reclaim = unsafe {
            bindings::cpp_page_queues_debug_page_is_reclaim(self.as_raw(), page.as_raw(), &mut age)
        };
        if is_reclaim { Some(QueueAge(age)) } else { None }
    }

    /// Rotates the reclaim queues.
    pub fn rotate_reclaim_queues(&self) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_rotate_reclaim_queues(self.as_raw()) }
    }
}
