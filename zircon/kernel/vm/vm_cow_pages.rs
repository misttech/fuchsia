// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::anonymous_page_request::AnonymousPageRequest;
use super::page::{VmPageDoublyLinkedList, VmPagePtr};
use super::page_queues::PageQueues;
use super::page_state::VmPageState;
use super::vm_page_list::VmPageSpliceList;
use crate::kernel::types::PAddr;
use crate::vm::compressor::VmCompressor;
use crate::vm::discardable_vmo_tracker::DiscardableVmoTracker;
use crate::vm::page_source::MultiPageRequest;
use crate::vm::pmm_node::PmmOptDelayReuse;
use bitflags::bitflags;
use core::cmp;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use fbl::{Canary, HasRefCount, HasRefCountUpgradeable, Recyclable, RefCounted, RefPtr};
use kalloc::AllocError;
use ksync::{KMutex, KMutexGuard, LockClass, LockToken, RawCriticalMutex};
use pin_init::{PinInit, pin_data};
use vm_constants_rs as constants;
use vm_cow_pages_bindings as bindings;
use zr::{Opaque, pin_init_ffi, unsafe_pinned_drop_ffi};
use zx_status::Status;
use zx_types::zx_status_t;

/// A copy-on-write page hierarchy range.
///
/// Arithmetic methods assume that callers have validated the range (e.g. via
/// [`VmCowRange::is_bounded_by`]) so that `offset + len` and any transformations do not overflow
/// or underflow `u64`.
#[repr(C)]
#[must_use]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VmCowRange {
    pub offset: u64,
    pub len: u64,
}

impl VmCowRange {
    #[inline]
    pub const fn new(offset: u64, len: u64) -> Self {
        Self { offset, len }
    }

    #[inline]
    pub const fn end(self) -> u64 {
        self.offset + self.len
    }

    #[inline]
    pub const fn is_empty(self) -> bool {
        self.len == 0
    }

    #[inline]
    pub const fn is_page_aligned(self) -> bool {
        ::page::is_page_rounded(self.offset as usize) && ::page::is_page_rounded(self.len as usize)
    }

    #[inline]
    pub const fn expand_till_page_aligned(self) -> VmCowRange {
        let start = ::page::round_down(self.offset as usize) as u64;
        VmCowRange::new(start, (::page::round_up(self.end() as usize) as u64).wrapping_sub(start))
    }

    #[inline]
    pub const fn offset_by(self, delta: u64) -> VmCowRange {
        VmCowRange::new(self.offset + delta, self.len)
    }

    /// Trims `amount` bytes from the start of the range. The caller must ensure `amount <= self.len`.
    #[inline]
    pub const fn trimmed_from_start(self, amount: u64) -> VmCowRange {
        debug_assert!(amount <= self.len);
        VmCowRange::new(self.offset + amount, self.len - amount)
    }

    /// Returns the minimal range that covers both `self` and `other`. If these ranges are disjoint
    /// then the returned range will be larger than combined length of `self` and `other` in
    /// order to span both using a single range.
    #[inline]
    pub fn cover(self, other: VmCowRange) -> VmCowRange {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let start = cmp::min(self.offset, other.offset);
        let end = cmp::max(self.end(), other.end());
        VmCowRange::new(start, end.wrapping_sub(start))
    }

    #[inline]
    pub const fn with_length(self, new_length: u64) -> VmCowRange {
        VmCowRange::new(self.offset, new_length)
    }

    #[inline]
    pub fn is_bounded_by(self, max: u64) -> bool {
        ::range_check::in_range_max(self.offset, self.len, max)
    }
}

impl From<bindings::VmCowRange> for VmCowRange {
    #[inline]
    fn from(bindings_range: bindings::VmCowRange) -> Self {
        Self::new(bindings_range.offset, bindings_range.len)
    }
}

impl From<VmCowRange> for bindings::VmCowRange {
    #[inline]
    fn from(range: VmCowRange) -> Self {
        Self { offset: range.offset, len: range.len }
    }
}

::zr::static_assert!(core::mem::size_of::<VmCowRange>() == 16);
::zr::static_assert!(
    core::mem::size_of::<VmCowRange>() == core::mem::size_of::<bindings::VmCowRange>()
);
::zr::static_assert!(core::mem::align_of::<VmCowRange>() == 8);
::zr::static_assert!(
    core::mem::align_of::<VmCowRange>() == core::mem::align_of::<bindings::VmCowRange>()
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowRange, offset)
        == core::mem::offset_of!(bindings::VmCowRange, offset)
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowRange, len) == core::mem::offset_of!(bindings::VmCowRange, len)
);
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmCowReclaimFailure {
    EvictAccessed = 0,
    CompressAccessed = 1,
    CompressFailed = 2,
    IncorrectPage = 3,
    Other = 4,
}
::zr::static_assert_size_and_align!(
    VmCowReclaimFailure,
    core::mem::size_of::<bindings::VmCowReclaimFailure>(),
    core::mem::align_of::<bindings::VmCowReclaimFailure>(),
);
::zr::static_assert!(
    VmCowReclaimFailure::EvictAccessed as u8 == bindings::VmCowReclaimFailure::EvictAccessed as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::CompressAccessed as u8
        == bindings::VmCowReclaimFailure::CompressAccessed as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::CompressFailed as u8
        == bindings::VmCowReclaimFailure::CompressFailed as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::IncorrectPage as u8 == bindings::VmCowReclaimFailure::IncorrectPage as u8
);
::zr::static_assert!(
    VmCowReclaimFailure::Other as u8 == bindings::VmCowReclaimFailure::Other as u8
);
impl From<bindings::VmCowReclaimFailure> for VmCowReclaimFailure {
    #[inline]
    fn from(value: bindings::VmCowReclaimFailure) -> Self {
        match value {
            bindings::VmCowReclaimFailure::EvictAccessed => Self::EvictAccessed,
            bindings::VmCowReclaimFailure::CompressAccessed => Self::CompressAccessed,
            bindings::VmCowReclaimFailure::CompressFailed => Self::CompressFailed,
            bindings::VmCowReclaimFailure::IncorrectPage => Self::IncorrectPage,
            bindings::VmCowReclaimFailure::Other => Self::Other,
        }
    }
}
impl From<VmCowReclaimFailure> for bindings::VmCowReclaimFailure {
    #[inline]
    fn from(value: VmCowReclaimFailure) -> Self {
        match value {
            VmCowReclaimFailure::EvictAccessed => Self::EvictAccessed,
            VmCowReclaimFailure::CompressAccessed => Self::CompressAccessed,
            VmCowReclaimFailure::CompressFailed => Self::CompressFailed,
            VmCowReclaimFailure::IncorrectPage => Self::IncorrectPage,
            VmCowReclaimFailure::Other => Self::Other,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VmCowReclaimType {
    Evict = 0,
    Discard = 1,
    Compress = 2,
}
::zr::static_assert_size_and_align!(
    VmCowReclaimType,
    core::mem::size_of::<bindings::VmCowReclaimSuccess_Type>(),
    core::mem::align_of::<bindings::VmCowReclaimSuccess_Type>(),
);
::zr::static_assert!(
    VmCowReclaimType::Evict as u8 == bindings::VmCowReclaimSuccess_Type::Evict as u8
);
::zr::static_assert!(
    VmCowReclaimType::Discard as u8 == bindings::VmCowReclaimSuccess_Type::Discard as u8
);
::zr::static_assert!(
    VmCowReclaimType::Compress as u8 == bindings::VmCowReclaimSuccess_Type::Compress as u8
);
impl From<bindings::VmCowReclaimSuccess_Type> for VmCowReclaimType {
    #[inline]
    fn from(value: bindings::VmCowReclaimSuccess_Type) -> Self {
        match value {
            bindings::VmCowReclaimSuccess_Type::Evict => Self::Evict,
            bindings::VmCowReclaimSuccess_Type::Discard => Self::Discard,
            bindings::VmCowReclaimSuccess_Type::Compress => Self::Compress,
        }
    }
}
impl From<VmCowReclaimType> for bindings::VmCowReclaimSuccess_Type {
    #[inline]
    fn from(value: VmCowReclaimType) -> Self {
        match value {
            VmCowReclaimType::Evict => Self::Evict,
            VmCowReclaimType::Discard => Self::Discard,
            VmCowReclaimType::Compress => Self::Compress,
        }
    }
}

/// Result of attempting to reclaim a single page. See `VmCowPages::ReclaimPage` for more details.
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmCowReclaimSuccess {
    pub type_: VmCowReclaimType,
    pub num_pages: u64,
    pub num_loaned_pages: u64,
}
::zr::static_assert_size_and_align!(
    VmCowReclaimSuccess,
    core::mem::size_of::<bindings::VmCowReclaimSuccess>(),
    core::mem::align_of::<bindings::VmCowReclaimSuccess>(),
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowReclaimSuccess, type_)
        == core::mem::offset_of!(bindings::VmCowReclaimSuccess, type_)
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowReclaimSuccess, num_pages)
        == core::mem::offset_of!(bindings::VmCowReclaimSuccess, num_pages)
);
::zr::static_assert!(
    core::mem::offset_of!(VmCowReclaimSuccess, num_loaned_pages)
        == core::mem::offset_of!(bindings::VmCowReclaimSuccess, num_loaned_pages)
);
impl From<bindings::VmCowReclaimSuccess> for VmCowReclaimSuccess {
    #[inline]
    fn from(value: bindings::VmCowReclaimSuccess) -> Self {
        Self {
            type_: value.type_.into(),
            num_pages: value.num_pages,
            num_loaned_pages: value.num_loaned_pages,
        }
    }
}
impl From<VmCowReclaimSuccess> for bindings::VmCowReclaimSuccess {
    #[inline]
    fn from(value: VmCowReclaimSuccess) -> Self {
        Self {
            type_: value.type_.into(),
            __bindgen_padding_0: [0; 7],
            num_pages: value.num_pages,
            num_loaned_pages: value.num_loaned_pages,
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageSourceType {
    /// The VmCowPages is not directly backed by a `page_source`.
    Anonymous = 0,
    /// The VmCowPages is directly backed by a contiguous page source. That has the following
    /// implications:
    /// * The direct page source provides specific physical pages.
    /// * The direct page source (possibly with the help of the VmCowPages) provides zeroed pages.
    Contiguous = 1,
    /// The VmCowPages is directly backed by a user pager. That has the following implications:
    /// * The direct page source is responsible for freeing the page content.
    /// * The VmCowPages must implement dirty tracking.
    UserPager = 2,
}
::zr::static_assert_size_and_align!(
    PageSourceType,
    core::mem::size_of::<bindings::PageSourceType>(),
    core::mem::align_of::<bindings::PageSourceType>(),
);
::zr::static_assert!(PageSourceType::Anonymous as u8 == bindings::PageSourceType::Anonymous as u8);
::zr::static_assert!(
    PageSourceType::Contiguous as u8 == bindings::PageSourceType::Contiguous as u8
);
::zr::static_assert!(PageSourceType::UserPager as u8 == bindings::PageSourceType::UserPager as u8);
impl From<bindings::PageSourceType> for PageSourceType {
    #[inline]
    fn from(value: bindings::PageSourceType) -> Self {
        match value {
            bindings::PageSourceType::Anonymous => Self::Anonymous,
            bindings::PageSourceType::Contiguous => Self::Contiguous,
            bindings::PageSourceType::UserPager => Self::UserPager,
        }
    }
}
impl From<PageSourceType> for bindings::PageSourceType {
    #[inline]
    fn from(value: PageSourceType) -> Self {
        match value {
            PageSourceType::Anonymous => Self::Anonymous,
            PageSourceType::Contiguous => Self::Contiguous,
            PageSourceType::UserPager => Self::UserPager,
        }
    }
}

/// See DiscardableVmoTracker::DebugDiscardablePageCounts().
#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct DiscardablePageCounts {
    pub locked: u64,
    pub unlocked: u64,
}
::zr::static_assert_size_and_align!(
    DiscardablePageCounts,
    core::mem::size_of::<bindings::VmCowPages_DiscardablePageCounts>(),
    core::mem::align_of::<bindings::VmCowPages_DiscardablePageCounts>(),
);
::zr::static_assert!(
    core::mem::offset_of!(DiscardablePageCounts, locked)
        == core::mem::offset_of!(bindings::VmCowPages_DiscardablePageCounts, locked)
);
::zr::static_assert!(
    core::mem::offset_of!(DiscardablePageCounts, unlocked)
        == core::mem::offset_of!(bindings::VmCowPages_DiscardablePageCounts, unlocked)
);
impl From<bindings::VmCowPages_DiscardablePageCounts> for DiscardablePageCounts {
    #[inline]
    fn from(value: bindings::VmCowPages_DiscardablePageCounts) -> Self {
        Self { locked: value.locked, unlocked: value.unlocked }
    }
}
impl From<DiscardablePageCounts> for bindings::VmCowPages_DiscardablePageCounts {
    #[inline]
    fn from(value: DiscardablePageCounts) -> Self {
        Self { locked: value.locked, unlocked: value.unlocked }
    }
}

/// Argument that specifies the context in which we are supplying pages.
pub type SupplyOptions = bindings::SupplyOptions;

/// Controls the type of `VmPageOrMarker` slot in `self`'s `page_list` that can be overwritten by
/// the `add_[new_]page[s]_locked` functions. It is the caller's responsibility to ensure that the
/// previous content is dealt with correctly (e.g. any pages and compressed references are freed).
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CanOverwriteSlot {
    /// Only overwrite empty slots.
    Empty = 0,
    /// Can overwrite parent content markers (in addition to empty slots).
    EmptyOrParent = 1,
    /// Can overwrite zero page markers and zero intervals (in addition to empty slots and
    /// parent content markers).
    ZeroMarkerOrInterval = 2,
    /// Can overwrite pages and compressed references (in addition to empty slots, parent content
    /// markers, zero markers and intervals), i.e. can overwrite all VmPageOrMarker types.
    PageOrRef = 3,
}
::zr::static_assert_size_and_align!(
    CanOverwriteSlot,
    core::mem::size_of::<bindings::VmCowPages_CanOverwriteSlot>(),
    core::mem::align_of::<bindings::VmCowPages_CanOverwriteSlot>(),
);
::zr::static_assert!(
    CanOverwriteSlot::Empty as u8 == bindings::VmCowPages_CanOverwriteSlot::Empty as u8
);
::zr::static_assert!(
    CanOverwriteSlot::EmptyOrParent as u8
        == bindings::VmCowPages_CanOverwriteSlot::EmptyOrParent as u8
);
::zr::static_assert!(
    CanOverwriteSlot::ZeroMarkerOrInterval as u8
        == bindings::VmCowPages_CanOverwriteSlot::ZeroMarkerOrInterval as u8
);
::zr::static_assert!(
    CanOverwriteSlot::PageOrRef as u8 == bindings::VmCowPages_CanOverwriteSlot::PageOrRef as u8
);
impl From<bindings::VmCowPages_CanOverwriteSlot> for CanOverwriteSlot {
    #[inline]
    fn from(value: bindings::VmCowPages_CanOverwriteSlot) -> Self {
        match value {
            bindings::VmCowPages_CanOverwriteSlot::Empty => Self::Empty,
            bindings::VmCowPages_CanOverwriteSlot::EmptyOrParent => Self::EmptyOrParent,
            bindings::VmCowPages_CanOverwriteSlot::ZeroMarkerOrInterval => {
                Self::ZeroMarkerOrInterval
            }
            bindings::VmCowPages_CanOverwriteSlot::PageOrRef => Self::PageOrRef,
        }
    }
}
impl From<CanOverwriteSlot> for bindings::VmCowPages_CanOverwriteSlot {
    #[inline]
    fn from(value: CanOverwriteSlot) -> Self {
        match value {
            CanOverwriteSlot::Empty => Self::Empty,
            CanOverwriteSlot::EmptyOrParent => Self::EmptyOrParent,
            CanOverwriteSlot::ZeroMarkerOrInterval => Self::ZeroMarkerOrInterval,
            CanOverwriteSlot::PageOrRef => Self::PageOrRef,
        }
    }
}

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
    #[pin]
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
    #[pin]
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
        // SAFETY: `self.as_raw()` points to a live `PriorityChanger`, guaranteed by
        // `Pin<&mut Self>` and its exclusive borrow.
        unsafe {
            bindings::cpp_priority_changer_change_high_priority_count_locked(self.as_raw());
        }
    }
}

#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EvictionAction {
    FollowHint = 0,
    IgnoreHint = 1,
}
::zr::static_assert_size_and_align!(
    EvictionAction,
    core::mem::size_of::<bindings::VmCowPages_EvictionAction>(),
    core::mem::align_of::<bindings::VmCowPages_EvictionAction>(),
);
::zr::static_assert!(
    EvictionAction::FollowHint as u8 == bindings::VmCowPages_EvictionAction::FollowHint as u8
);
::zr::static_assert!(
    EvictionAction::IgnoreHint as u8 == bindings::VmCowPages_EvictionAction::IgnoreHint as u8
);
impl From<bindings::VmCowPages_EvictionAction> for EvictionAction {
    #[inline]
    fn from(value: bindings::VmCowPages_EvictionAction) -> Self {
        match value {
            bindings::VmCowPages_EvictionAction::FollowHint => Self::FollowHint,
            bindings::VmCowPages_EvictionAction::IgnoreHint => Self::IgnoreHint,
        }
    }
}
impl From<EvictionAction> for bindings::VmCowPages_EvictionAction {
    #[inline]
    fn from(value: EvictionAction) -> Self {
        match value {
            EvictionAction::FollowHint => Self::FollowHint,
            EvictionAction::IgnoreHint => Self::IgnoreHint,
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
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DirtyState {
    /// The page does not track dirty state. Used for non pager backed pages.
    Untracked = 0,
    /// The page is clean, i.e. its contents have not been altered from when the page was supplied.
    Clean = 1,
    /// The page's contents have been modified from the time of supply, and should be written back
    /// to the page source at some point.
    Dirty = 2,
    /// The page still has modified contents, but the page source is in the process of writing back
    /// the changes. This is used to ensure that a consistent version is written back, and that any
    /// new modifications that happen during the writeback are not lost. The page source will mark
    /// pages AwaitingClean before starting any writeback.
    AwaitingClean = 3,
    /// Number of states.
    NumStates = 4,
}
::zr::static_assert_size_and_align!(
    DirtyState,
    core::mem::size_of::<bindings::VmCowPages_DirtyState>(),
    core::mem::align_of::<bindings::VmCowPages_DirtyState>(),
);
::zr::static_assert!(
    DirtyState::Untracked as u8 == bindings::VmCowPages_DirtyState::Untracked as u8
);
::zr::static_assert!(DirtyState::Clean as u8 == bindings::VmCowPages_DirtyState::Clean as u8);
::zr::static_assert!(DirtyState::Dirty as u8 == bindings::VmCowPages_DirtyState::Dirty as u8);
::zr::static_assert!(
    DirtyState::AwaitingClean as u8 == bindings::VmCowPages_DirtyState::AwaitingClean as u8
);
::zr::static_assert!(
    DirtyState::NumStates as u8 == bindings::VmCowPages_DirtyState::NumStates as u8
);
::zr::static_assert!((DirtyState::NumStates as u32) <= crate::vm::page::object::MAX_DIRTY_STATES);
impl From<bindings::VmCowPages_DirtyState> for DirtyState {
    #[inline]
    fn from(value: bindings::VmCowPages_DirtyState) -> Self {
        match value {
            bindings::VmCowPages_DirtyState::Untracked => Self::Untracked,
            bindings::VmCowPages_DirtyState::Clean => Self::Clean,
            bindings::VmCowPages_DirtyState::Dirty => Self::Dirty,
            bindings::VmCowPages_DirtyState::AwaitingClean => Self::AwaitingClean,
            bindings::VmCowPages_DirtyState::NumStates => Self::NumStates,
        }
    }
}
impl From<DirtyState> for bindings::VmCowPages_DirtyState {
    #[inline]
    fn from(value: DirtyState) -> Self {
        match value {
            DirtyState::Untracked => Self::Untracked,
            DirtyState::Clean => Self::Clean,
            DirtyState::Dirty => Self::Dirty,
            DirtyState::AwaitingClean => Self::AwaitingClean,
            DirtyState::NumStates => Self::NumStates,
        }
    }
}

#[repr(C)]
pub struct VmCowPagesState {
    /// magic value
    canary: Canary<{ fbl::magic(b"VMCP") }>,
    pmm_alloc_flags: u32,
    options: VmCowPagesOptions,
}

::zr::static_assert_size_and_align!(
    VmCowPagesState,
    constants::kVmCowPagesStateSize,
    constants::kVmCowPagesStateAlign,
);

bitflags! {
    #[repr(transparent)]
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    pub struct VmCowPagesOptions: u32 {
        // Externally-usable flags:
        const NONE = 0;
        const USER_PAGER_BACKED_ROOT = 1 << 0;
        const PAGE_SOURCE_ROOT = 1 << 1;
        /// With this clear, zeroing a page tries to decommit the page.  With this set, zeroing
        /// never decommits the page.  Currently this is only set for contiguous VMOs.
        //
        // TODO(dustingreen): Once we're happy with the reliability of page borrowing, we should
        // be able to relax this restriction.  We may still need to flush zeroes to RAM during
        // reclaim to mitigate a hypothetical client incorrectly assuming that cache-clean status
        // will remain intact while pages aren't pinned, but that mitigation should be sufficient
        // (even assuming such a client) to allow implicit decommit when zeroing or when zero
        // scanning, as long as no clients are doing DMA to/from contiguous while not pinned.
        const CANNOT_DECOMMIT_ZERO_PAGES = 1 << 2;

        // Internal-only flags:
        const HIDDEN = 1 << 3;
        const INTERNAL_ONLY_MASK = Self::HIDDEN.bits();
    }
}

::zr::static_assert!(VmCowPagesOptions::NONE.bits() == bindings::VmCowPagesOptions_kNone);
::zr::static_assert!(
    VmCowPagesOptions::USER_PAGER_BACKED_ROOT.bits()
        == bindings::VmCowPagesOptions_kUserPagerBackedRoot
);
::zr::static_assert!(
    VmCowPagesOptions::PAGE_SOURCE_ROOT.bits() == bindings::VmCowPagesOptions_kPageSourceRoot
);
::zr::static_assert!(
    VmCowPagesOptions::CANNOT_DECOMMIT_ZERO_PAGES.bits()
        == bindings::VmCowPagesOptions_kCannotDecommitZeroPages
);
::zr::static_assert!(VmCowPagesOptions::HIDDEN.bits() == bindings::VmCowPagesOptions_kHidden);
::zr::static_assert!(
    VmCowPagesOptions::INTERNAL_ONLY_MASK.bits() == bindings::VmCowPagesOptions_kInternalOnlyMask
);

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
    /// Returns the raw dirty state bits of `page`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    unsafe fn page_dirty_state(page: VmPagePtr) -> u8 {
        // SAFETY: The caller guarantees `page` is a valid `VmPage` pointer.
        let page = unsafe { page.as_ref() };
        debug_assert_eq!(page.state(), VmPageState::OBJECT);
        // SAFETY: The caller guarantees `object` is the active union variant and that reading its
        // flags does not race with concurrent writes.
        unsafe { (*(*page.state_union.get()).object).dirty_state() }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_dirty_tracked(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) != DirtyState::Untracked as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_dirty(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) == DirtyState::Dirty as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_clean(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) == DirtyState::Clean as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state, and
    /// that reading its `object` union variant / `flags` is safe without data races (e.g. by
    /// holding the owning `VmCowPages` lock).
    #[inline]
    pub unsafe fn is_page_awaiting_clean(page: VmPagePtr) -> bool {
        // SAFETY: The caller upholds the safety preconditions of `page_dirty_state`.
        unsafe { Self::page_dirty_state(page) == DirtyState::AwaitingClean as u8 }
    }

    /// # Safety
    ///
    /// The caller must ensure that `page` points to a valid `VmPage` in the `OBJECT` state (owned
    /// by a VMO and assigned to `PageQueues`).
    #[inline]
    pub unsafe fn is_page_reclaimable(page: VmPagePtr) -> bool {
        // SAFETY: The caller guarantees `page` is a valid `VmPage` in the `OBJECT` state owned by
        // a VMO and assigned to `PageQueues`.
        unsafe { PageQueues::is_page_reclaimable(page) }
    }

    /// Non-borrowing accessor to the Rust state in `VmCowPages`.
    #[inline]
    fn state(&self) -> &VmCowPagesState {
        // SAFETY: `self.as_raw()` points to a live `VmCowPages`. `cpp_vm_cow_pages_state` returns
        // a non-null, suitably aligned pointer to the `VmCowPagesState` that the C++ constructor
        // initialised via `rust_vm_cow_pages_state_init`. It lives as long as `self`, and C++
        // never writes to it after construction, so creating a shared reference is sound.
        unsafe {
            bindings::cpp_vm_cow_pages_state(self.as_raw())
                .cast::<VmCowPagesState>()
                .as_ref_unchecked()
        }
    }

    #[inline]
    pub(crate) fn options(&self) -> VmCowPagesOptions {
        self.state().options
    }

    #[inline]
    pub(crate) fn pmm_alloc_flags(&self) -> u32 {
        self.state().pmm_alloc_flags
    }

    #[inline]
    pub(crate) fn assert_canary(&self) {
        self.state().canary.assert();
    }

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
        let mut success = MaybeUninit::<bindings::VmCowReclaimSuccess>::zeroed();
        let mut failure = MaybeUninit::<bindings::VmCowReclaimFailure>::uninit();
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, `page.as_raw()` is a
        // valid `vm_page_t` pointer, and out-pointers are valid for writes.
        let ok = unsafe {
            bindings::cpp_vm_cow_pages_reclaim_page(
                self.as_raw(),
                page.as_ffi(),
                offset,
                eviction_action.into(),
                compressor_ptr,
                success.as_mut_ptr(),
                failure.as_mut_ptr(),
            )
        };
        if ok {
            // SAFETY: `cpp_vm_cow_pages_reclaim_page` initialized `success` when returning true.
            Ok(unsafe { success.assume_init() }.into())
        } else {
            // SAFETY: `cpp_vm_cow_pages_reclaim_page` initialized `failure` when returning false.
            Err(unsafe { failure.assume_init() }.into())
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
                range.into(),
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
                overwrite.into(),
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
                range.into(),
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
                range.into(),
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
        let mut counts = MaybeUninit::<bindings::VmCowPages_DiscardablePageCounts>::uninit();
        // SAFETY: `self.as_raw()` returns a valid `VmCowPages` pointer, and `counts` is valid
        // for writing. `cpp_vm_cow_pages_debug_get_discardable_page_counts` unconditionally
        // initializes `counts`.
        unsafe {
            bindings::cpp_vm_cow_pages_debug_get_discardable_page_counts(
                self.as_raw(),
                counts.as_mut_ptr(),
            );
            counts.assume_init().into()
        }
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

/// # Safety
///
/// * The pointer passed for `storage` must be non-null, suitably aligned, point to
///   `kVmCowPagesStateSize` bytes of storage, and must not be aliased for the duration of the call.
#[unsafe(no_mangle)]
pub extern "C" fn rust_vm_cow_pages_state_init(
    storage: &mut MaybeUninit<VmCowPagesState>,
    pmm_alloc_flags: u32,
    options: u32,
) {
    storage.write(VmCowPagesState {
        canary: Canary::default(),
        pmm_alloc_flags,
        options: VmCowPagesOptions::from_bits_retain(options),
    });
}

/// # Safety
///
/// * The pointer passed for `storage` must be non-null, suitably aligned, point to
///   `kVmCowPagesStateSize` bytes of initialized storage, and must not be aliased for the duration
///   of the call.
/// * After this call returns, the caller must not use `storage` again as an initialized
///   `VmCowPagesState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_cow_pages_destruct(storage: &mut MaybeUninit<VmCowPagesState>) {
    // SAFETY: The caller guarantees `storage` is initialised and is not used afterwards.
    unsafe {
        storage.assume_init_drop();
    }
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_cow_pages_assert_canary(cow: &VmCowPages) {
    cow.assert_canary();
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_cow_pages_options(cow: &VmCowPages) -> u32 {
    cow.options().bits()
}

/// # Safety
///
/// * `cow` must point to a valid `VmCowPages`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vm_cow_pages_pmm_alloc_flags(cow: &VmCowPages) -> u32 {
    cow.pmm_alloc_flags()
}

#[cfg(ktest)]
#[unittest::suite]
/// Tests for the Rust port of VmCowRange.
mod vm_cow_pages_rs {
    use super::VmCowRange;
    use unittest::{expect_eq, expect_false, expect_true};

    /// Tests various methods of VmCowRange.
    #[test]
    fn vm_cow_range_test() {
        // construction/end/is_empty
        let r1 = VmCowRange::new(0, 0);
        expect_true!(r1.is_empty());
        expect_eq!(r1.end(), 0);

        let r2 = VmCowRange::new(4096, 8192);
        expect_false!(r2.is_empty());
        expect_eq!(r2.end(), 12288);
        expect_eq!(r2.offset + r2.len, r2.end());

        let r3 = VmCowRange::new(u64::MAX, 0);
        expect_true!(r3.is_empty());
        expect_eq!(r3.end(), u64::MAX);

        let r4 = VmCowRange::new(0, u64::MAX);
        expect_false!(r4.is_empty());
        expect_eq!(r4.end(), u64::MAX);

        // page alignment
        let page_size = ::page::SIZE as u64;

        let r_aligned = VmCowRange::new(page_size, page_size * 2);
        expect_true!(r_aligned.is_page_aligned());

        let r_unaligned_len = VmCowRange::new(page_size, page_size * 2 - 1);
        expect_false!(r_unaligned_len.is_page_aligned());
        let r_unaligned_len_expanded = r_unaligned_len.expand_till_page_aligned();
        expect_true!(r_unaligned_len_expanded.is_page_aligned());
        expect_eq!(r_unaligned_len_expanded.offset, page_size);
        expect_eq!(r_unaligned_len_expanded.len, page_size * 2);

        let r_unaligned_offset = VmCowRange::new(page_size + 1, page_size);
        expect_false!(r_unaligned_offset.is_page_aligned());
        let r_unaligned_offset_expanded = r_unaligned_offset.expand_till_page_aligned();
        expect_true!(r_unaligned_offset_expanded.is_page_aligned());
        expect_eq!(r_unaligned_offset_expanded.offset, page_size);
        expect_eq!(r_unaligned_offset_expanded.len, page_size * 2);

        // offset_by/trimmed_from_start/with_length
        let r_arith = VmCowRange::new(4096, 8192);

        let r_offset = r_arith.offset_by(4096);
        expect_eq!(r_offset.offset, 8192);
        expect_eq!(r_offset.len, 8192);

        let r_trimmed = r_arith.trimmed_from_start(4096);
        expect_eq!(r_trimmed.offset, 8192);
        expect_eq!(r_trimmed.len, 4096);

        let r_length = r_arith.with_length(4096);
        expect_eq!(r_length.offset, 4096);
        expect_eq!(r_length.len, 4096);

        // cover
        let cover_base = VmCowRange::new(4096, 8192);
        let cover_next = VmCowRange::new(8192, 8192);
        let cover1 = cover_base.cover(cover_next);
        expect_eq!(cover1.offset, 4096);
        expect_eq!(cover1.len, 12288);

        let disjoint1 = VmCowRange::new(4096, 4096);
        let disjoint2 = VmCowRange::new(16384, 4096);
        let cover2 = disjoint1.cover(disjoint2);
        expect_eq!(cover2.offset, 4096);
        expect_eq!(cover2.len, 16384);

        let empty_range = VmCowRange::new(0, 0);
        let cover_empty1 = cover_base.cover(empty_range);
        expect_eq!(cover_empty1.offset, cover_base.offset);
        expect_eq!(cover_empty1.len, cover_base.len);

        let cover_empty2 = empty_range.cover(cover_base);
        expect_eq!(cover_empty2.offset, cover_base.offset);
        expect_eq!(cover_empty2.len, cover_base.len);

        // is_bounded_by incl. the overflow case
        let r_bounded = VmCowRange::new(4096, 8192);
        expect_true!(r_bounded.is_bounded_by(16384));
        expect_true!(r_bounded.is_bounded_by(12288));
        expect_false!(r_bounded.is_bounded_by(12287));
        expect_false!(r_bounded.is_bounded_by(4096));

        let overflow_r = VmCowRange::new(u64::MAX - 4095, 8192);
        expect_false!(overflow_r.is_bounded_by(u64::MAX));
    }
}
