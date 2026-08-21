// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::page::VmPagePtr;
use crate::kernel::types::PAddr;
use crate::vm::compressor::VmCompressor;
use crate::vm::discardable_vmo_tracker::DiscardableVmoTracker;
use crate::vm::pmm::PmmOptDelayReuse;
use core::marker::PhantomPinned;
use core::mem::MaybeUninit;
use core::ptr::{self, NonNull};
use fbl::{HasRefCount, Recyclable, RefCounted, RefPtr};
use kalloc::AllocError;
use vm_cow_pages_bindings as bindings;
use zr::Opaque;
use zx_status::Status;
use zx_types::zx_status_t;

pub type VmCowRange = bindings::VmCowRange;
pub type VmCowReclaimFailure = bindings::VmCowReclaimFailure;
pub type VmCowReclaimSuccess = bindings::VmCowReclaimSuccess;
pub type VmCowReclaimType = bindings::VmCowReclaimSuccess_Type;
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
                before_page.as_ffi(),
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
        unsafe { bindings::cpp_vm_cow_pages_dedup_zero_page(self.as_raw(), page.as_ffi(), offset) }
    }

    /// Returns the page at `offset`, if present in this node.
    pub fn debug_get_page(&self, offset: u64) -> Option<VmPagePtr> {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        let raw = unsafe { bindings::cpp_vm_cow_pages_debug_get_page(self.as_raw(), offset) };
        // SAFETY: `raw` is either null or points to a valid `vm_page_t`.
        unsafe { VmPagePtr::from_ffi(raw) }
    }

    /// Returns whether this node has no page at `offset`.
    pub fn debug_is_empty(&self, offset: u64) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_debug_is_empty(self.as_raw(), offset) }
    }

    /// Returns the debug discardable tracker, if present.
    pub fn debug_get_discardable_tracker(&self) -> Option<&DiscardableVmoTracker> {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        let raw =
            unsafe { bindings::cpp_vm_cow_pages_debug_get_discardable_tracker(self.as_raw()) };
        if raw.is_null() {
            None
        } else {
            // SAFETY: `raw` points to a live `DiscardableVmoTracker` owned by `self`.
            Some(unsafe { DiscardableVmoTracker::from_raw_ref(raw) })
        }
    }

    /// Evict a specific loaned page for the use case of reclaiming loaned pages by the physical
    /// page provider. Unlike ReclaimPage this function can assume it just needs to evict, and
    /// has no requirements on updating any reclamation lists.
    ///
    /// # Safety
    ///
    /// `page` must be an object-associated page.
    pub unsafe fn evict_loaned_page(&self, page: VmPagePtr, offset: u64) -> Result<(), Status> {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, and `page.as_raw()` is a
        // valid `vm_page_t` pointer.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_evict_loaned_page(self.as_raw(), page.as_ffi(), offset)
        };
        Status::ok(status)
    }

    /// Asks the VMO to attempt to reclaim the specified page. There are a few possible outcomes:
    /// 1. Exactly this page is reclaimed.
    /// 2. This page and other pages are reclaimed.
    /// 3. Just other pages are reclaimed.
    /// 4. No pages are reclaimed.
    ///
    /// Pages other than the one requested may get reclaimed due to any internal relationships
    /// between pages that make it meaningless or difficult to reclaim just the single page in
    /// question. In the cases of (3) and (4) there are some guarantees provided:
    /// 1. If the `page` was not from this VMO (or not at the specified offset) then nothing about
    ///    the `page` or this VMO will be modified.
    /// 2. If the `page` is from this VMO and offset (and was not reclaimed) then the page will have
    ///    been removed from any candidate reclamation lists (such as the DontNeed pager backed
    ///    list).
    ///
    /// The effect of (2) is that the caller can assume in the case of reclamation failure it will
    /// not keep finding this page as a reclamation candidate and infinitely retry it.
    /// `eviction_action` indicates whether the `always_need` eviction hint should be respected or
    /// ignored. Require will force eviction.
    ///
    /// The actual number of pages reclaimed is returned if successful, or a failure reason if not.
    ///
    /// # Safety
    ///
    /// The caller must know that it is sound to reclaim `page` at `offset`.
    pub unsafe fn reclaim_page(
        &self,
        page: VmPagePtr,
        offset: u64,
        eviction_action: EvictionAction,
        compressor: Option<&VmCompressor>,
    ) -> Result<VmCowReclaimSuccess, VmCowReclaimFailure> {
        let compressor_ptr = match compressor {
            Some(c) => c.as_raw(),
            None => ptr::null_mut(),
        };
        let mut success = MaybeUninit::uninit();
        let mut failure = MaybeUninit::uninit();
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, `page.as_raw()` is a
        // valid `vm_page_t` pointer, and out-pointers are valid for writes.
        let ok = unsafe {
            bindings::cpp_vm_cow_pages_reclaim_page(
                self.as_raw(),
                page.as_ffi(),
                offset,
                eviction_action,
                compressor_ptr,
                success.as_mut_ptr(),
                failure.as_mut_ptr(),
            )
        };
        if ok {
            // SAFETY: `cpp_vm_cow_pages_reclaim_page` initialized `success` when returning true.
            Ok(unsafe { success.assume_init() })
        } else {
            // SAFETY: `cpp_vm_cow_pages_reclaim_page` initialized `failure` when returning false.
            Err(unsafe { failure.assume_init() })
        }
    }

    /// Similar to LookupLocked, but enumerate all readable pages in the hierarchy within the
    /// requested range. The offset passed to the `lookup_fn` is the offset this page is visible at
    /// in this object, even if the page itself is committed in a parent object. The physical
    /// addresses given to the `lookup_fn` should not be retained in any way unless the range has
    /// also been pinned by the caller.
    /// Ranges of length zero are considered invalid and will return `ZX_ERR_INVALID_ARGS`. The
    /// `lookup_fn` can terminate iteration early by returning `ZX_ERR_STOP`.
    ///
    /// # Warning
    ///
    /// This function only exists for test code. There is no way non-test code can use this function
    /// in a correct way due to its internal locking.
    pub fn debug_lookup_readable<T: Sized>(
        &self,
        range: VmCowRange,
        ctx: &mut T,
        lookup_fn: fn(u64, PAddr, &mut T) -> Result<(), Status>,
    ) -> Result<(), Status> {
        struct LookupState<'a, T> {
            ctx: &'a mut T,
            lookup_fn: fn(u64, PAddr, &mut T) -> Result<(), Status>,
        }

        /// # Safety
        ///
        /// `ctx` must point to a valid `LookupState<'_, T>` created on the stack in `lookup`
        /// that remains valid for the duration of the C++ FFI lookup callback.
        unsafe extern "C" fn lookup_callback_shim<T>(
            ctx: *mut core::ffi::c_void,
            offset: u64,
            paddr: u64,
        ) -> zx_status_t {
            // SAFETY: `ctx` is guaranteed by `cpp_vm_object_lookup` to be the non-null `ctx_ptr`
            // passed from `lookup`, which points to a live `LookupState<'_, T>` on the caller's
            // stack.
            let state = unsafe { ctx.cast::<LookupState<'_, T>>().as_mut_unchecked() };
            Status::result_into_raw((state.lookup_fn)(offset, paddr.into(), state.ctx))
        }

        let mut state = LookupState { ctx, lookup_fn };
        let state_ptr: *mut LookupState<'_, T> = &mut state;
        // Erase the Rust type so we can pass our context pointer through C++'s void* argument.
        let ctx_ptr: *mut core::ffi::c_void = state_ptr.cast();
        let status = unsafe {
            bindings::cpp_vm_cow_pages_debug_lookup_readable(
                self.as_raw(),
                range,
                ctx_ptr,
                Some(lookup_callback_shim::<T>),
            )
        };
        Status::ok(status)
    }
}

fn initialize_page_cache(level: init::LkInitLevel) {
    unsafe {
        bindings::cpp_vm_cow_pages_initialize_page_cache(level.0);
    }
}

// Initialize the cache after the percpu data structures are initialized.
init::lk_init_hook!(vm_cow_pages_cache_init, initialize_page_cache, init::LK_INIT_LEVEL_KERNEL);
