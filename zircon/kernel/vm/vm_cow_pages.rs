// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::anonymous_page_request::AnonymousPageRequest;
use super::page::{VmPageDoublyLinkedList, VmPagePtr};
use super::vm_page_list::VmPageSpliceList;
use crate::kernel::types::PAddr;
use crate::vm::compressor::VmCompressor;
use crate::vm::discardable_vmo_tracker::DiscardableVmoTracker;
use crate::vm::page_source::MultiPageRequest;
use crate::vm::pmm_node::PmmOptDelayReuse;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use fbl::{HasRefCount, HasRefCountUpgradeable, Recyclable, RefCounted, RefPtr};
use kalloc::AllocError;
use ksync::{KMutex, KMutexGuard, LockClass, LockToken, RawCriticalMutex};
use pin_init::{PinInit, pin_data};
use vm_cow_pages_bindings as bindings;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;
use zx_types::zx_status_t;

pub type VmCowRange = bindings::VmCowRange;
pub type VmCowReclaimFailure = bindings::VmCowReclaimFailure;
pub type VmCowReclaimSuccess = bindings::VmCowReclaimSuccess;
pub type VmCowReclaimType = bindings::VmCowReclaimSuccess_Type;
pub type PageSourceType = bindings::PageSourceType;
pub type DiscardablePageCounts = bindings::VmCowPages_DiscardablePageCounts;

/// Argument that specifies the context in which we are supplying pages.
pub type SupplyOptions = bindings::SupplyOptions;

/// Controls the type of `VmPageOrMarker` slot in `self` `VmCowPages`' `page_list_` that can be
/// overwritten by the `add_[new_]page[s]_locked` functions. It is the caller's responsibility to
/// ensure that the previous content is dealt with correctly (e.g. any pages and compressed
/// references are freed).
pub type CanOverwriteSlot = bindings::VmCowPages_CanOverwriteSlot;

#[derive(Debug, Copy, Clone)]
pub struct VmCowPagesLockClass;

impl LockClass for VmCowPagesLockClass {
    const ID: *mut c_void = core::ptr::null_mut();
}

pub type VmCowPagesLock = KMutex<VmCowPagesLockClass, RawCriticalMutex>;

/// Helper object for finishing VmCowPages operations that must occur after the lock is dropped.
/// This is necessary due to some operations being externally locked. Consider this sequence:
///
/// ```
/// stack_pin_init!(let deferred = DeferredOps::new(&cow_object));
/// ksync::lock!(let guard = cow_object.lock());
/// cow_object.do_operation_locked(&deferred);
/// ```
///
/// The destruction order will then allow `deferred` to perform its actions after `guard` is
/// destructed and the lock is dropped.
///
/// This struct is not thread safe.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct DeferredOps<'a> {
    opaque: Opaque<bindings::VmCowPages_DeferredOps>,
    phantom: PhantomData<&'a VmCowPages>,
}

unsafe_pinned_drop_ffi!(DeferredOps<'_>, bindings::cpp_vm_cow_pages_deferred_ops_destroy);

impl<'a> DeferredOps<'a> {
    /// Construct a `DeferredOps` for the given `VmCowPages`. Must be constructed, and
    /// deconstructed, without the lock held. It is the caller's responsibility to ensure the
    /// pointer remains valid over the lifetime of the object.
    pub fn new(cow: &'a VmCowPages) -> impl PinInit<Self> {
        pin_init_ffi!(bindings::cpp_vm_cow_pages_deferred_ops_construct, cow.as_raw())
    }
}

/// `PriorityChanger` is a transaction object for changing the high priority count of `VmCowPages`
/// objects. The high priority count is used to manage page reclamation. This object ensures that a
/// `DeferredOps` object is created against the correct `VmCowPages`, which might be an ancestor of
/// `target`. `PriorityChanger` does the following:
///
/// * Manages the lifetime of `VmCowPages` to ensure they last until the
/// priority change operation, by holding a `RefPtr`.
/// * Handles the construction of the `DeferredOps` object.
/// * Clarifies which operations need to be done within the `VmCowPages` lock and which need to
///   happen without the lock being held.
/// * Presents a high-level API to `VmMapping` and `VmObjectPaged`.
/// * Handles traversing up the `VmCowPages` hierarchy to ensure that high priority status is
/// propagated.
///
/// Callers should prefer `VmMapping::set_memory_priority_locked`,
/// `VmMapping::set_memory_priority_default_locked_object`, or
/// `VmMapping::set_memory_priority_high_already_positive_locked_object`, if applicable.
///
/// To change the high priority count of a `VmCowPages`, you should first construct the
/// `PriorityChanger` object. Then,
/// * If `delta` < 0, you must immediately call one of the change methods (such as
///   [`change_high_priority_count_locked`](Self::change_high_priority_count_locked)), which
///   change the high priority count of `target`.
/// * If `delta` > 0, you must call an applicable prepare method to set up the prerequisites for the
///   priority change operation, then call one of the change methods.
///
/// This struct is not thread-safe.
#[pin_data(PinnedDrop)]
#[repr(transparent)]
pub struct PriorityChanger<'a> {
    opaque: Opaque<bindings::PriorityChanger>,
    phantom: PhantomData<&'a VmCowPages>,
}

unsafe_pinned_drop_ffi!(PriorityChanger<'_>, bindings::cpp_priority_changer_destroy);

impl<'a> PriorityChanger<'a> {
    /// Construct a priority changer to change the high priority count of `target`. When `delta` is
    /// positive, we go towards high priority. When it's negative, we move towards default
    /// priority.
    ///
    /// The constructor may be called with or without `target`'s lock being held.
    ///
    /// The caller must ensure that `target` remains valid during the lifetime of `PriorityChanger`.
    ///
    /// `delta != 0` is required (`delta == 0` does not do anything meaningful).
    /// Before dropping this object, a change method (such as `change_high_priority_count_locked`)
    /// must be called, and the object must be dropped without holding `target`'s lock.
    pub fn new(cow: &'a VmCowPages, delta: i64) -> impl PinInit<Self> {
        pin_init_ffi!(bindings::cpp_priority_changer_construct, cow.as_raw(), delta)
    }

    pub fn as_raw(self: Pin<&mut Self>) -> *mut bindings::PriorityChanger {
        // SAFETY: Obtaining a raw pointer to `opaque` does not move the pinned object.
        unsafe { self.get_unchecked_mut().opaque.get() }
    }

    /// Returns a reference to the target `VmCowPages`'s lock.
    pub fn lock(&self) -> &'a VmCowPagesLock {
        // SAFETY: `cpp_priority_changer_lock` returns `&target_->lock_`, a subobject of the
        // target `VmCowPages`. The C++ `Lock<CriticalMutex>` is layout-compatible with
        // `KMutex<VmCowPagesLockClass, RawCriticalMutex>`.
        unsafe { &*bindings::cpp_priority_changer_lock(self.opaque.get()).cast::<VmCowPagesLock>() }
    }

    /// This method must be called *without* holding `target`'s `VmCowPages` lock. In addition, the
    /// caller must not hold a `PriorityChanger` or `DeferredOps` on any other `VmCowPages` in
    /// `target`'s hierarchy.
    pub fn prepare_may_not_already_be_high_priority(self: Pin<&mut Self>) {
        // TODO(ethanws): Find a way for the caller to annotate their guarantee that they
        // don't hold the lock.

        // SAFETY: `self.as_raw()` points to a live `PriorityChanger`, guaranteed by
        // `Pin<&mut Self>` and its exclusive borrow.
        unsafe {
            bindings::cpp_priority_changer_prepare_may_not_already_be_high_priority(self.as_raw());
        }
    }

    /// This method may be called if `target` already has a `high_priority_count_` > 0.
    ///
    /// This method must be called with `target`'s lock held.
    pub fn prepare_is_already_high_priority_locked(
        self: Pin<&mut Self>,
        _token: &LockToken<'_, VmCowPagesLockClass>,
    ) {
        // SAFETY: `self.as_raw()` points to a live `PriorityChanger`, guaranteed by
        // `Pin<&mut Self>` and its exclusive borrow.
        unsafe {
            bindings::cpp_priority_changer_prepare_is_already_high_priority_locked(self.as_raw());
        }
    }

    /// Increments or decrements the priority count of this VMO. The high priority count is used to
    /// control any page reclamation, and applies to the whole VMO, including its parents. The count
    /// is never allowed to go negative and so callers must only subtract what they have already
    /// added. Further, callers are required to remove any additions before the VMO is destroyed.
    pub fn change_high_priority_count_locked(
        self: Pin<&mut Self>,
        _token: &LockToken<'_, VmCowPagesLockClass>,
    ) {
        // SAFETY: `self.as_raw()` points to a live `PriorityChanger`, guaranteed by `Pin<&mut Self>`
        // and its exclusive borrow.
        unsafe {
            bindings::cpp_priority_changer_change_high_priority_count_locked(self.as_raw());
        }
    }
}

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

impl HasRefCountUpgradeable for VmCowPages {}

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
    #[inline]
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

    /// Domain-specific conversion: constructs a `&VmCowPages` from a raw pointer.
    ///
    /// Unlike [`VmCowPages::from_raw`] and [`VmCowPages::upgrade_from_raw`], this acquires no
    /// reference at all; the caller is responsible for keeping the object alive for `'a`.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null pointer to a `VmCowPages` that remains live for the
    /// lifetime `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::VmCowPages) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::VmCowPages` is layout-compatible with `VmCowPages`, and the caller
        // guarantees `ptr` is valid for `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Upgrades a raw `VmCowPages` pointer to a `RefPtr<VmCowPages>`, or returns `None` if the
    /// object is being destroyed.
    ///
    /// # Safety
    ///
    /// `cow` must be a valid pointer to `VmCowPages`.
    pub unsafe fn upgrade_from_raw(cow: *mut bindings::VmCowPages) -> Option<RefPtr<Self>> {
        let ptr = unsafe { bindings::cpp_vm_cow_pages_upgrade_from_raw(cow) };
        if ptr.is_null() {
            None
        } else {
            // SAFETY: `cpp_vm_cow_pages_upgrade_from_raw` incremented the refcount,
            // so we can adopt it.
            unsafe { RefPtr::try_from_raw(ptr.cast::<Self>()) }
        }
    }

    /// Returns true if the `VmCowPages` is capable of borrowing pages.
    #[inline]
    pub fn can_borrow(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_can_borrow(self.as_raw()) }
    }

    /// Returns true if the `VmCowPages` can evict pages (i.e. is user-pager backed).
    #[inline]
    pub fn can_evict(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_can_evict(self.as_raw()) }
    }

    /// Returns true if the `VmCowPages` is discardable.
    #[inline]
    pub fn is_discardable(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_is_discardable(self.as_raw()) }
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

    /// Unlocked wrapper around `ReplacePageLocked`, exposed for the physical page provider to
    /// cancel loans with.
    ///
    /// If page is still at offset, replace it with a different page. If `with_loaned` is true,
    /// replace with a loaned page. If `with_loaned` is false, replace with a non-loaned page and a
    /// `page_request` is required to be provided.
    ///
    /// The replacement page is returned on success.
    pub fn replace_page(
        &self,
        before_page: VmPagePtr,
        offset: u64,
        with_loaned: bool,
        page_request: Option<Pin<&mut AnonymousPageRequest>>,
    ) -> Result<VmPagePtr, Status> {
        let mut out_page = ptr::null_mut();
        let page_request_ptr = match page_request {
            Some(page_request) => page_request.as_raw(),
            None => ptr::null_mut(),
        };
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, `before_page.as_ffi()` is
        // a valid `vm_page_t` pointer, `&raw mut out_page` is valid for writing a
        // `vm_page_t` pointer, and `page_request_ptr` is either null or points to a live
        // `AnonymousPageRequest`.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_replace_page(
                self.as_raw(),
                before_page.as_ffi(),
                offset,
                with_loaned,
                &raw mut out_page,
                page_request_ptr,
            )
        };

        // SAFETY: `out_page` must be non-null if `status == OK`, in which case the
        // conversion from FFI will succeed.
        Status::ok(status)
            .map(|()| unsafe { VmPagePtr::from_ffi(out_page).expect("page pointer is non-null") })
    }

    /// Returns whether this `VmCowPages`'s lock is held by the current thread.
    ///
    /// Intended for `debug_assert!`s that both document and check a caller's obligation to hold
    /// the VMO lock across a call.
    pub fn lock_is_held(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_lock_is_held(self.as_raw()) }
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

    /// Returns whether the slot at `offset` holds a parent content marker.
    pub fn debug_is_parent_content(&self, offset: u64) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_debug_is_parent_content(self.as_raw(), offset) }
    }

    /// Returns whether this node has no page at `offset`.
    pub fn debug_is_empty(&self, offset: u64) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_debug_is_empty(self.as_raw(), offset) }
    }

    pub fn debug_is_page(&self, offset: u64) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_debug_is_page(self.as_raw(), offset) }
    }

    /// Returns whether this VmCowPages is marked high memory priority.
    pub fn debug_is_high_memory_priority(&self) -> bool {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_debug_is_high_memory_priority(self.as_raw()) }
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

    /// Acquires the lock via pin-init for use with `ksync::lock!`.
    #[inline]
    pub fn lock(
        &self,
    ) -> impl PinInit<KMutexGuard<'_, VmCowPagesLockClass, RawCriticalMutex>, Infallible> {
        // SAFETY: `self.as_raw()` points to a live VmCowPages.
        let lock = unsafe { bindings::cpp_vm_cow_pages_get_lock(self.as_raw()) };
        let lock: *mut VmCowPagesLock = lock.cast();
        // SAFETY: `lock` is valid for reads for the lifetime of `self`.
        let lock = unsafe { lock.as_ref_unchecked() };
        lock.lock()
    }

    /// Adds a set of pages consecutively starting from the given offset. Regardless of the
    /// return result ownership of the pages is taken. Pages are assumed to be in the ALLOC state
    /// and can be optionally zeroed before inserting. `start_offset` must be page aligned.
    ///
    /// `overwrite` controls how the function handles pre-existing content in the range, however
    /// it is not valid to specify the `CanOverwriteSlot::PageOrRef` option, as any pages or
    /// compressed references that would get released as a consequence cannot be returned.
    pub fn add_new_pages_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        start_offset: u64,
        list: Pin<&mut VmPageDoublyLinkedList>,
        overwrite: CanOverwriteSlot,
        zero: bool,
        deferred: Pin<&mut DeferredOps<'_>>,
    ) -> Result<(), Status> {
        // SAFETY: We do not move `list`.
        let list: &mut VmPageDoublyLinkedList = unsafe { list.get_unchecked_mut() };
        let list: *mut VmPageDoublyLinkedList = list;
        let list: *mut bindings::VmPageDoublyLinkedList = list.cast();
        // SAFETY: We do not move `deferred`.
        let deferred: &mut DeferredOps<'_> = unsafe { deferred.get_unchecked_mut() };
        let deferred: *mut bindings::VmCowPages_DeferredOps = deferred.opaque.get();
        // SAFETY: `self.as_raw()` is a live VmCowPages.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_add_new_pages_locked(
                self.as_raw(),
                start_offset,
                list,
                overwrite,
                zero,
                deferred,
            )
        };
        Status::ok(status)
    }

    /// After successful completion the range of pages will all read as zeros. The mechanism used
    /// to achieve this is not guaranteed to decommit, but it will try to.
    ///
    /// `range` must be page aligned offsets within the range of the object. `dirty_track`
    /// specifies whether the range being zeroed subscribes to dirty tracking, if `true` the range
    /// will start out as dirty. `dirty_track` only has meaning if the VMO supports dirty tracking,
    /// otherwise it is ignored.
    ///
    /// Returns a status, and the number of bytes that were actually zeroed. The byte count may be
    /// nonzero even if the status is an error.
    ///
    /// The returned status is one of the following:
    ///  * `Ok` => The whole range was successfully zeroed.
    ///  * `Err(Status::SHOULD_WAIT)` => The caller needs to wait on the `page_request` and then
    ///    retry the operation. The caller may advance the start offset by the number of zeroed
    ///    bytes before retrying.
    ///  * Any other error indicates a failure to zero a part of the range or the whole range.
    ///
    /// The caller must hold *this* object's lock. `token` proves only that some lock of the
    /// `VmCowPagesLockClass` class is held, and the C++ side runs with thread safety analysis
    /// disabled, so neither side can check this.
    pub fn zero_pages_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        dirty_track: bool,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Pin<&mut MultiPageRequest>,
    ) -> (Result<(), Status>, u64) {
        // SAFETY: We do not move `deferred`.
        let deferred: &mut DeferredOps<'_> = unsafe { deferred.get_unchecked_mut() };
        let deferred: *mut bindings::VmCowPages_DeferredOps = deferred.opaque.get();
        let page_request = page_request.as_raw();
        let mut zeroed_bytes = 0;
        // SAFETY: `self.as_raw()` is a live VmCowPages, `deferred` and `page_request` point to the
        // live pinned objects borrowed above, and `zeroed_bytes` is valid for writing.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_zero_pages_locked(
                self.as_raw(),
                range,
                dirty_track,
                deferred,
                page_request,
                &mut zeroed_bytes,
            )
        };
        (Status::ok(status), zeroed_bytes)
    }

    /// See `VmObject::supply_pages`.
    ///
    /// Should never return `Err(Status::SHOULD_WAIT)`, waiting on page requests is managed by
    /// `ProcessPagesForSupply`.
    pub fn supply_pages_locked(
        &self,
        _token: &LockToken<'_, VmCowPagesLockClass>,
        range: VmCowRange,
        pages: Pin<&mut VmPageSpliceList>,
        options: SupplyOptions,
        deferred: Pin<&mut DeferredOps<'_>>,
        page_request: Option<Pin<&mut MultiPageRequest>>,
    ) -> Result<(), Status> {
        // SAFETY: We do not move `deferred`.
        let deferred: &mut DeferredOps<'_> = unsafe { deferred.get_unchecked_mut() };
        let deferred: *mut bindings::VmCowPages_DeferredOps = deferred.opaque.get();
        let page_request_ptr = match page_request {
            Some(page_request) => page_request.as_raw(),
            None => ptr::null_mut(),
        };
        // SAFETY: `self.as_raw()` is a live VmCowPages, `pages.as_raw()` points to a live
        // `VmPageSpliceList`, `deferred` points to a live `DeferredOps` and `page_request_ptr` is
        // either null or points to a live `MultiPageRequest`.
        let status = unsafe {
            bindings::cpp_vm_cow_pages_supply_pages_locked(
                self.as_raw(),
                range,
                pages.as_raw(),
                options,
                deferred,
                page_request_ptr,
            )
        };
        Status::ok(status)
    }

    /// Test-only interface to get the current populated slots count.
    ///
    /// This method panics if the kernel is built without support for the functional continuous
    /// attribution tracker (EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED).
    pub fn debug_get_populated_slots_count(&self) -> u32 {
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer.
        unsafe { bindings::cpp_vm_cow_pages_debug_get_populated_slots_count(self.as_raw()) }
    }

    /// Returns the number of locked and unlocked pages in this discardable VMO (zero counts if
    /// this VMO is not discardable, or has not opted into locking / unlocking yet).
    ///
    /// See `DiscardableVmoTracker::debug_discardable_page_counts()`.
    pub fn debug_get_discardable_page_counts(&self) -> DiscardablePageCounts {
        let mut counts = DiscardablePageCounts { locked: 0, unlocked: 0 };
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, and `counts` is valid
        // for writing.
        unsafe {
            bindings::cpp_vm_cow_pages_debug_get_discardable_page_counts(
                self.as_raw(),
                &mut counts,
            );
        }
        counts
    }

    /// Dumps all the reclamation counters to the debuglog.
    pub fn debug_dump_reclaim_counters() {
        // SAFETY: `cpp_vm_cow_pages_debug_dump_reclaim_counters` has no preconditions.
        unsafe {
            bindings::cpp_vm_cow_pages_debug_dump_reclaim_counters();
        }
    }
}

fn initialize_page_cache(level: init::LkInitLevel) {
    unsafe {
        bindings::cpp_vm_cow_pages_initialize_page_cache(level.0);
    }
}

// Initialize the cache after the percpu data structures are initialized.
init::lk_init_hook!(vm_cow_pages_cache_init, initialize_page_cache, init::LK_INIT_LEVEL_KERNEL);
