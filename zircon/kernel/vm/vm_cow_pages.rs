// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::page::VmPagePtr;
use crate::vm::pmm::PmmOptDelayReuse;
use core::marker::PhantomPinned;
use core::ptr::NonNull;
use fbl::{HasRefCount, Recyclable, RefCounted, RefPtr};
use kalloc::AllocError;
use vm_cow_pages_bindings as bindings;
use zr::Opaque;
use zx_status::Status;

pub type VmCowReclaimFailure = bindings::VmCowReclaimFailure;
pub type PageSourceType = bindings::PageSourceType;

/// Used to track dirty_state in the vm_page_t.
///
/// The transitions between the three states can roughly be summarized as follows:
/// 1. A page starts off as Clean when supplied.
/// 2. A write transitions the page from Clean to Dirty.
/// 3. A writeback_begin moves the Dirty page to AwaitingClean.
/// 4. A writeback_end moves the AwaitingClean page to Clean.
/// 5. A write that comes in while the writeback is in progress (i.e. the page is AwaitingClean)
///    moves the AwaitingClean page back to Dirty.
pub type DirtyState = bindings::VmCowPages_DirtyState;

pub type EvictionAction = bindings::VmCowPages_EvictionAction;

pub mod options {
    use vm_cow_pages_bindings as bindings;

    pub const NONE: u32 = bindings::VmCowPagesOptions_kNone;
    pub const USER_PAGER_BACKED_ROOT: u32 = bindings::VmCowPagesOptions_kUserPagerBackedRoot;
    pub const PAGE_SOURCE_ROOT: u32 = bindings::VmCowPagesOptions_kPageSourceRoot;

    /// With this clear, zeroing a page tries to decommit the page.  With this set, zeroing never
    /// decommits the page.  Currently this is only set for contiguous VMOs.
    //
    // TODO(dustingreen): Once we're happy with the reliability of page borrowing, we should be able
    // to relax this restriction.  We may still need to flush zeroes to RAM during reclaim to
    // mitigate a hypothetical client incorrectly assuming that cache-clean status will remain
    // intact while pages aren't pinned, but that mitigation should be sufficient (even assuming
    // such a client) to allow implicit decommit when zeroing or when zero scanning, as long as no
    // clients are doing DMA to/from contiguous while not pinned.
    pub const CANNOT_DECOMMIT_ZERO_PAGES: u32 =
        bindings::VmCowPagesOptions_kCannotDecommitZeroPages;

    pub const HIDDEN: u32 = bindings::VmCowPagesOptions_kHidden;
}

/// A copy-on-write page hierarchy.
#[repr(C)]
pub struct VmCowPages {
    raw: Opaque<bindings::VmCowPages>,
    phantom: PhantomPinned,
}

impl HasRefCount for VmCowPages {
    fn ref_count(&self) -> &RefCounted {
        let raw = unsafe { bindings::cpp_vm_cow_pages_get_ref_counted(self.as_raw()) };
        unsafe { &*(raw.cast::<RefCounted>()) }
    }
}

unsafe impl Recyclable for VmCowPages {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            bindings::cpp_vm_cow_pages_free(ptr.as_ptr().cast());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}

impl VmCowPages {
    /// Domain-specific conversion: returns raw pointer for `VmCowPages`.
    pub fn as_raw(&self) -> *mut bindings::VmCowPages {
        self.raw.get()
    }

    /// Domain-specific conversion: constructs a `RefPtr<VmCowPages>` from an exported pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid raw `VmCowPages` pointer exported from C++.
    pub unsafe fn from_raw(ptr: *mut bindings::VmCowPages) -> Option<RefPtr<Self>> {
        unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
    }

    /// Replaces a page at offset with a loaned page.
    pub fn replace_page_with_loaned(
        &self,
        before_page: VmPagePtr,
        offset: u64,
    ) -> Result<(), Status> {
        let status = unsafe {
            bindings::cpp_vm_cow_pages_replace_page_with_loaned(
                self.as_raw(),
                before_page.as_raw(),
                offset,
            )
        };
        Status::ok(status)
    }

    /// Returns whether page reuse should be delayed on free (i.e. if ever pinned).
    pub fn should_delay_reuse_on_free(&self) -> PmmOptDelayReuse {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_should_delay_reuse_on_free(self.as_raw()) }
    }

    /// Returns the parent `VmCowPages` in the COW hierarchy, if any.
    pub fn debug_get_parent(&self) -> Option<RefPtr<VmCowPages>> {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        let raw = unsafe { bindings::cpp_vm_cow_pages_debug_get_parent(self.as_raw()) };
        // SAFETY: cpp_vm_cow_pages_debug_get_parent returns a valid exported `VmCowPages` pointer,
        // or null if there is no parent.
        unsafe { Self::from_raw(raw) }
    }

    /// Deduplicates a committed zero page at `offset`.
    pub fn dedup_zero_page(&self, page: VmPagePtr, offset: u64) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, and `page.as_raw()` is a
        // valid `vm_page_t` pointer.
        unsafe { bindings::cpp_vm_cow_pages_dedup_zero_page(self.as_raw(), page.as_raw(), offset) }
    }
}

fn initialize_page_cache(level: init::LkInitLevel) {
    unsafe {
        bindings::cpp_vm_cow_pages_initialize_page_cache(level.0);
    }
}

// Initialize the cache after the percpu data structures are initialized.
init::lk_init_hook!(vm_cow_pages_cache_init, initialize_page_cache, init::LK_INIT_LEVEL_KERNEL);
