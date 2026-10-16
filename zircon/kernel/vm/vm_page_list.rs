// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::pmm::{self, node as pmm_node};
use crate::kernel::types::PAddr;
use core::ops::Deref;
use core::pin::Pin;
use debug::{ltracef, ltracef_level};
use pin_init::pin_data;
use vm_constants_rs::{
    kPmmNodeIndexZeroBits, kVmPageListFanOut, kVmPageListIntervalBits,
    kVmPageListIntervalSentinelBits, kVmPageListIntervalType, kVmPageListIntervalTypeBits,
    kVmPageListPageType, kVmPageListParentContentType, kVmPageListReferenceType,
    kVmPageListTypeBits, kVmPageListZeroMarkerType,
};
use vm_page_list_bindings as bindings;
use zr::Opaque;
use zx_status::Status;

use crate::vm::page::{VmPageDoublyLinkedList, VmPagePtr};

const LOCAL_TRACE: u32 = 0;

/// RAII helper for representing content in a page list node. This supports being in one of these
/// states:
///  * Empty       - Contains nothing.
///  * Page p      - Contains a `VmPagePtr` 'p'. This 'p' is considered owned by this wrapper and
///    `release_page` must be called to give up ownership.
///  * Reference r - Contains a reference 'r' to some content. This 'r' is considered owned by this
///    wrapper and `release_reference` must be called to give up ownership.
///  * Marker      - Indicates that whilst not a page, it is also not empty. Markers can be used to
///    separate the distinction between "there's no page because we've deduped to the zero page" (a
///    `Marker` is inserted) and "there's no page because our parent contains the content" (which is
///    represented as `Empty`).
///  * ParentContent - Indicates that there might be content for this slot, but the page list in the
///    parent must be checked for it. The difference between `Empty`, which can also indicate that
///    the parent must be searched, and `ParentContent` is up to the specific VMO.
///  * Interval    - Indicates that this page is part of a sparse page interval. An interval will
///    have a Start sentinel, and an End sentinel, and all offsets that lie between the two will be
///    empty. If the interval spans a single page, it will be represented as a Slot sentinel, which
///    is conceptually the same as both a Start and an End sentinel.
///
/// There are certain invariants that the page list tries to maintain at all times. It might not
/// always be possible to enforce these as the checks involved might be expensive, however it is
/// important that any code that manipulates the page list abide by them, primarily to keep the
/// memory occupied by the page list nodes in check:
/// 1. Page list nodes cannot be completely empty i.e. they must contain at least one non-empty
///    slot.
/// 2. Any intervals in the page list should span a maximal range. In other words, there should not
///    be consecutive intervals in the page list which it would have been possible to represent with
///    a single interval instead.
///
/// Note on Preconditions & Safety Contracts:
/// `VmPageOrMarker` uses manual bit-packing rather than a native Rust `enum` to maintain
/// C++ memory layout parity and zero-overhead performance. Accessors use `debug_assert!`
/// to validate variant preconditions in debug builds, matching C++ `DEBUG_ASSERT` behavior.
#[derive(Debug)]
#[repr(transparent)]
pub struct VmPageOrMarker {
    raw: u32,
}

/// The types of sparse page interval types that are supported.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum IntervalType {
    /// Represents a range of zero pages.
    Zero = 0,
}

/// Sentinel types that are used to represent a sparse page interval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum SentinelType {
    /// Represents a single page interval.
    Slot = 0,
    /// The first page of a multi-page interval.
    Start,
    /// The last page of a multi-page interval.
    End,
}

/// The various dirty states that a zero interval can be in. Refer to VmCowPages::DirtyState for
/// an explanation of the states. Note that an AwaitingClean state is not encoded in the interval
/// state bits. This information is instead stored using the awaiting_clean_length for convenience,
/// where a non-zero length indicates that the interval is AwaitingClean. Doing this affords
/// more convenient splitting and merging of intervals.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum ZeroRangeDirtyState {
    /// Dirty state is untracked.
    Untracked = 0,
    /// Range is clean.
    Clean,
    /// Range is dirty.
    Dirty,
}

impl ZeroRangeDirtyState {
    /// Decodes a raw discriminant, panicking if it is out of range.
    fn from_raw(raw: u32) -> Self {
        match raw {
            0 => Self::Untracked,
            1 => Self::Clean,
            2 => Self::Dirty,
            _ => panic!("invalid dirty state"),
        }
    }
}

/// The remaining bits of an interval type store any information specific to the type of interval
/// being tracked. The ZeroRange class is defined here to group together the encoding of these bits
/// specific to IntervalType::Zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
struct ZeroRange {
    value: u32,
}

impl ZeroRange {
    // This is the same as kIntervalBits in C++.
    const ALIGN_BITS: u32 = kVmPageListIntervalBits;
    const ALIGN_MASK: u32 = (1 << Self::ALIGN_BITS) - 1;
    // Bound to VM_PAGE_OBJECT_DIRTY_STATE_BITS in C++.
    // For zero range tracking, we also need to track dirty state information, and if the interval
    // is AwaitingClean, the length that is AwaitingClean.
    const DIRTY_STATE_BITS: u32 = crate::vm::page::object::DIRTY_STATE_BITS;
    const DIRTY_STATE_SHIFT: u32 = Self::ALIGN_BITS;
    const AWAITING_CLEAN_LENGTH_SHIFT: u32 = Self::ALIGN_BITS + Self::DIRTY_STATE_BITS;

    const PAGE_SHIFT: u32 = 12;
    const PAGE_SIZE: u64 = 1 << Self::PAGE_SHIFT;
    const DIRTY_STATE_MASK: u32 = ((1 << Self::DIRTY_STATE_BITS) - 1) << Self::DIRTY_STATE_SHIFT;
    const AWAITING_CLEAN_LENGTH_MASK: u32 = !((1 << Self::AWAITING_CLEAN_LENGTH_SHIFT) - 1);

    /// Creates a new `ZeroRange` with a raw aligned value.
    const fn new(val: u32) -> Self {
        debug_assert!((val & Self::ALIGN_MASK) == 0);
        Self { value: val }
    }

    /// Creates a `ZeroRange` with a raw aligned value and initial dirty state.
    fn new_with_state(val: u32, state: ZeroRangeDirtyState) -> Self {
        let mut this = Self::new(val);
        this.set_dirty_state(state);
        this
    }

    /// Returns the raw packed `u32` value.
    const fn value(&self) -> u32 {
        self.value
    }

    /// Returns the `ZeroRangeDirtyState`.
    fn dirty_state(&self) -> ZeroRangeDirtyState {
        let state_bits = (self.value & Self::DIRTY_STATE_MASK) >> Self::DIRTY_STATE_SHIFT;
        ZeroRangeDirtyState::from_raw(state_bits)
    }

    /// Sets the `ZeroRangeDirtyState`.
    fn set_dirty_state(&mut self, state: ZeroRangeDirtyState) {
        debug_assert!(
            matches!(state, ZeroRangeDirtyState::Dirty | ZeroRangeDirtyState::Untracked),
            "Only allow dirty and untracked zero ranges for now"
        );
        self.value =
            (self.value & !Self::DIRTY_STATE_MASK) | ((state as u32) << Self::DIRTY_STATE_SHIFT);
    }

    /// Sets the page-aligned awaiting clean length.
    fn set_awaiting_clean_length(&mut self, len: u64) {
        debug_assert!(len == 0 || self.dirty_state() == ZeroRangeDirtyState::Dirty);
        debug_assert!(len.is_multiple_of(Self::PAGE_SIZE), "len must be page-aligned");
        // The awaiting clean length is always page-aligned, so mask out the low bits and store only
        // upper bits.
        let encoded_len = ((len >> Self::PAGE_SHIFT) << Self::AWAITING_CLEAN_LENGTH_SHIFT) as u32;
        self.value = (self.value & !Self::AWAITING_CLEAN_LENGTH_MASK)
            | (encoded_len & Self::AWAITING_CLEAN_LENGTH_MASK);
    }

    /// Returns the page-aligned awaiting clean length.
    fn awaiting_clean_length(&self) -> u64 {
        let len_bits = (self.value & Self::AWAITING_CLEAN_LENGTH_MASK) as u64;
        (len_bits >> Self::AWAITING_CLEAN_LENGTH_SHIFT) << Self::PAGE_SHIFT
    }
}

/// Minimal wrapper around a `u32` to provide stronger typing in code to prevent accidental
/// mixing of references and other values.
/// Provides a way to query the required alignment of the references and does debug enforcement of
/// this.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ReferenceValue {
    value: u32,
}

impl ReferenceValue {
    // ALIGN_BITS represents the number of low bits in a reference that must be zero so they can be
    // used for internal metadata. This is declared here for convenience, and is asserted to be in
    // sync with the private VmPageOrMarker::TYPE_BITS.
    pub const ALIGN_BITS: u32 = 3;
    pub const ALIGN_MASK: u32 = (1 << Self::ALIGN_BITS) - 1;

    /// Creates a new `ReferenceValue`.
    pub const fn new(val: u32) -> Self {
        debug_assert!((val & Self::ALIGN_MASK) == 0);
        Self { value: val }
    }

    /// Returns the raw value.
    pub fn value(&self) -> u32 {
        self.value
    }
}

impl VmPageOrMarker {
    // The low 3 bits of raw are reserved to represent the type, any other data has to fit into
    // the remaining high bits. Note that there is no explicit Empty type, rather a PAGE_TYPE with a
    // zero pointer is used to represent Empty.
    const TYPE_BITS: u32 = kVmPageListTypeBits;
    const TYPE_MASK: u32 = (1 << Self::TYPE_BITS) - 1;
    const PAGE_TYPE: u32 = kVmPageListPageType;
    const ZERO_MARKER_TYPE: u32 = kVmPageListZeroMarkerType;
    const REFERENCE_TYPE: u32 = kVmPageListReferenceType;
    const INTERVAL_TYPE: u32 = kVmPageListIntervalType;
    const PARENT_CONTENT_TYPE: u32 = kVmPageListParentContentType;

    const MARKER_SHARE_COUNT_MASK: u32 = !Self::TYPE_MASK;
    const MARKER_SHARE_COUNT_STEP: u32 = 1 << Self::TYPE_BITS;

    // In addition to storing the type for an interval, we also need to track the type of interval
    // sentinel: the start, the end, or a single slot marker.
    const INTERVAL_SENTINEL_BITS: u32 = kVmPageListIntervalSentinelBits;
    const INTERVAL_SENTINEL_SHIFT: u32 = Self::TYPE_BITS;
    const INTERVAL_SENTINEL_MASK: u32 = (1 << Self::INTERVAL_SENTINEL_BITS) - 1;

    // Next we also need to store the type of interval being represented; reserve a couple of bits
    // for this. Currently we only support one type of interval: a range of zero pages, but
    // reserving 2 bits allows for more types in the future.
    const INTERVAL_TYPE_BITS: u32 = kVmPageListIntervalTypeBits;
    const INTERVAL_TYPE_SHIFT: u32 = Self::INTERVAL_SENTINEL_SHIFT + Self::INTERVAL_SENTINEL_BITS;
    const INTERVAL_TYPE_MASK: u32 = (1 << Self::INTERVAL_TYPE_BITS) - 1;

    const INTERVAL_BITS: u32 =
        Self::TYPE_BITS + Self::INTERVAL_SENTINEL_BITS + Self::INTERVAL_TYPE_BITS;
    const INTERVAL_MASK: u32 = (1 << Self::INTERVAL_BITS) - 1;

    const _ZERO_RANGE_ALIGN_CHECK: () =
        assert!(ZeroRange::ALIGN_BITS == VmPageOrMarker::INTERVAL_BITS);

    fn get_type(&self) -> u32 {
        self.raw & Self::TYPE_MASK
    }

    /// Creates an empty `VmPageOrMarker`.
    ///
    /// A `PAGE_TYPE` that otherwise holds a null pointer is considered to be Empty.
    pub fn empty() -> Self {
        Self { raw: Self::PAGE_TYPE }
    }

    /// Creates a `VmPageOrMarker` from a page pointer.
    pub fn from_page(p: VmPagePtr) -> Self {
        // Ensure the pmm page-to-index has enough zero bits.
        const _PMM_NODE_INDEX_CHECK: () =
            assert!(VmPageOrMarker::TYPE_BITS <= kPmmNodeIndexZeroBits);
        // A null page is incorrect for two reasons
        // 1. It's a violation of the API of this method
        // 2. A null page cannot be represented internally as this is used to represent Empty
        // (Note: `p` is non-null by `VmPagePtr` invariant)
        let raw = pmm_node().page_to_index(p);
        // Getting zero in `raw` means that `p` lives on the stack or the heap.
        // This is not supported.
        debug_assert!(raw != 0);
        // A pointer should be aligned by definition, and hence the low bits should always be zero,
        // but assert this anyway just in case TYPE_BITS is increased or someone passed an invalid
        // pointer.
        debug_assert!((raw & Self::TYPE_MASK) == 0);
        Self { raw: raw | Self::PAGE_TYPE }
    }

    /// Creates a `VmPageOrMarker` from a reference.
    pub fn from_reference(ref_val: ReferenceValue) -> Self {
        // Ensure the reference values have alignment such the type bits can be set without
        // overlapping actual ref being stored. Unlike the page type, which does not allow the 0
        // value to be stored, a ref value of 0 is valid and may be stored.
        const _REF_ALIGN_CHECK: () =
            assert!(ReferenceValue::ALIGN_BITS == VmPageOrMarker::TYPE_BITS);
        Self { raw: ref_val.value() | Self::REFERENCE_TYPE }
    }

    /// Creates a marker `VmPageOrMarker`.
    pub fn marker() -> Self {
        Self { raw: Self::ZERO_MARKER_TYPE }
    }

    /// Creates a marker `VmPageOrMarker` with a share count.
    pub fn marker_with_share_count(share_count: u32) -> Self {
        Self {
            raw: Self::ZERO_MARKER_TYPE
                | ((share_count << Self::TYPE_BITS) & Self::MARKER_SHARE_COUNT_MASK),
        }
    }

    /// Creates a parent content `VmPageOrMarker`.
    pub fn parent_content() -> Self {
        Self { raw: Self::PARENT_CONTENT_TYPE }
    }

    /// Creates a zero interval `VmPageOrMarker`.
    ///
    /// Only support creation of zero interval type for now.
    /// Private so that an external caller cannot arbitrarily create interval sentinels.
    fn zero_interval(sentinel: SentinelType, state: ZeroRangeDirtyState) -> Self {
        let sentinel_bits = (sentinel as u32) << Self::INTERVAL_SENTINEL_SHIFT;
        let type_bits = (IntervalType::Zero as u32) << Self::INTERVAL_TYPE_SHIFT;
        let zr = ZeroRange::new_with_state(0, state);
        Self { raw: zr.value() | type_bits | sentinel_bits | Self::INTERVAL_TYPE }
    }

    /// Returns true if this is empty.
    ///
    /// A `PAGE_TYPE` that otherwise holds a null pointer is considered to be Empty.
    pub fn is_empty(&self) -> bool {
        self.raw == Self::PAGE_TYPE
    }

    /// Returns true if this is a page.
    pub fn is_page(&self) -> bool {
        !self.is_empty() && (self.get_type() == Self::PAGE_TYPE)
    }

    /// Returns true if this is a reference.
    pub fn is_reference(&self) -> bool {
        self.get_type() == Self::REFERENCE_TYPE
    }

    /// Returns true if this is a page or a reference.
    pub fn is_page_or_ref(&self) -> bool {
        self.is_page() || self.is_reference()
    }

    /// Returns true if this is a marker.
    pub fn is_marker(&self) -> bool {
        self.get_type() == Self::ZERO_MARKER_TYPE
    }

    /// Returns true if this is parent content.
    pub fn is_parent_content(&self) -> bool {
        self.get_type() == Self::PARENT_CONTENT_TYPE
    }

    /// Returns the marker share count.
    pub fn marker_share_count(&self) -> u32 {
        debug_assert!(self.is_marker());
        self.raw >> Self::TYPE_BITS
    }

    /// Sets the marker share count.
    pub fn set_marker_share_count(&mut self, share_count: u32) {
        debug_assert!(self.is_marker());
        self.raw = (self.raw & !Self::MARKER_SHARE_COUNT_MASK)
            | ((share_count << Self::TYPE_BITS) & Self::MARKER_SHARE_COUNT_MASK);
    }

    /// Increments the marker share count.
    pub fn increment_marker_share_count(&mut self) {
        debug_assert!(self.is_marker());
        self.raw += Self::MARKER_SHARE_COUNT_STEP;
    }

    /// Decrements the marker share count.
    pub fn decrement_marker_share_count(&mut self) {
        debug_assert!(self.is_marker());
        // It is invalid to decrement marker share count from zero.
        debug_assert!(self.marker_share_count() > 0);
        self.raw -= Self::MARKER_SHARE_COUNT_STEP;
    }

    /// Returns true if this is an interval.
    pub fn is_interval(&self) -> bool {
        self.get_type() == Self::INTERVAL_TYPE
    }

    /// Returns the interval sentinel type.
    fn interval_sentinel(&self) -> SentinelType {
        let bits = (self.raw >> Self::INTERVAL_SENTINEL_SHIFT) & Self::INTERVAL_SENTINEL_MASK;
        match bits {
            0 => SentinelType::Slot,
            1 => SentinelType::Start,
            2 => SentinelType::End,
            _ => panic!("invalid sentinel type"),
        }
    }

    /// Sets the interval sentinel type.
    fn set_interval_sentinel(&mut self, sentinel: SentinelType) {
        // Clear the old sentinel type.
        self.raw &= !(Self::INTERVAL_SENTINEL_MASK << Self::INTERVAL_SENTINEL_SHIFT);
        // Set the new sentinel type.
        self.raw |= (sentinel as u32) << Self::INTERVAL_SENTINEL_SHIFT;
    }

    /// Returns the interval type.
    fn interval_type(&self) -> IntervalType {
        let bits = (self.raw >> Self::INTERVAL_TYPE_SHIFT) & Self::INTERVAL_TYPE_MASK;
        match bits {
            0 => IntervalType::Zero,
            _ => panic!("invalid interval type"),
        }
    }

    // Getters and setters for the interval type.

    /// Returns true if this is the start of an interval.
    pub fn is_interval_start(&self) -> bool {
        self.is_interval() && self.interval_sentinel() == SentinelType::Start
    }

    /// Returns true if this is the end of an interval.
    pub fn is_interval_end(&self) -> bool {
        self.is_interval() && self.interval_sentinel() == SentinelType::End
    }

    /// Returns true if this is an interval slot.
    pub fn is_interval_slot(&self) -> bool {
        self.is_interval() && self.interval_sentinel() == SentinelType::Slot
    }

    /// Returns true if this is a zero interval.
    pub fn is_interval_zero(&self) -> bool {
        self.is_interval() && self.interval_type() == IntervalType::Zero
    }

    // Getters and setter for the zero interval type.

    /// Returns true if this zero interval is clean.
    pub fn is_zero_interval_clean(&self) -> bool {
        debug_assert!(self.is_interval_zero());
        let zr_val = self.raw & !Self::INTERVAL_MASK;
        ZeroRange::new(zr_val).dirty_state() == ZeroRangeDirtyState::Clean
    }

    /// Returns true if this zero interval is dirty.
    pub fn is_zero_interval_dirty(&self) -> bool {
        debug_assert!(self.is_interval_zero());
        let zr_val = self.raw & !Self::INTERVAL_MASK;
        ZeroRange::new(zr_val).dirty_state() == ZeroRangeDirtyState::Dirty
    }

    /// Returns true if this zero interval is untracked.
    pub fn is_zero_interval_untracked(&self) -> bool {
        debug_assert!(self.is_interval_zero());
        let zr_val = self.raw & !Self::INTERVAL_MASK;
        ZeroRange::new(zr_val).dirty_state() == ZeroRangeDirtyState::Untracked
    }

    /// Returns the dirty state of this zero interval.
    pub fn zero_interval_dirty_state(&self) -> ZeroRangeDirtyState {
        debug_assert!(self.is_interval_zero());
        let zr_val = self.raw & !Self::INTERVAL_MASK;
        ZeroRange::new(zr_val).dirty_state()
    }

    /// Sets the awaiting-clean length of this zero interval.
    pub fn set_zero_interval_awaiting_clean_length(&mut self, len: u64) {
        debug_assert!(self.is_interval_zero());
        debug_assert!(self.is_interval_start() || self.is_interval_slot());
        let mut zr = ZeroRange::new(self.raw & !Self::INTERVAL_MASK);
        zr.set_awaiting_clean_length(len);
        self.raw = (self.raw & Self::INTERVAL_MASK) | zr.value();
    }

    /// Returns the awaiting-clean length of this zero interval.
    pub fn zero_interval_awaiting_clean_length(&self) -> u64 {
        debug_assert!(self.is_interval_zero());
        debug_assert!(self.is_interval_start() || self.is_interval_slot());
        let zr = ZeroRange::new(self.raw & !Self::INTERVAL_MASK);
        zr.awaiting_clean_length()
    }

    /// Change the interval sentinel type for an existing interval, while preserving the rest of the
    /// original state. Only valid to call on an existing interval type. The only permissible
    /// transitions are from Slot to Start/End and vice versa, as these are the only valid
    /// transitions when extending or clipping intervals. Private so that an external caller cannot
    /// arbitrarily manipulate interval sentinels.
    fn change_interval_sentinel(&mut self, new_sentinel: SentinelType) {
        if cfg!(debug_assertions) {
            debug_assert!(self.is_interval());
            let old_sentinel = self.interval_sentinel();
            debug_assert!(old_sentinel != new_sentinel);
            if old_sentinel == SentinelType::Start || old_sentinel == SentinelType::End {
                debug_assert!(new_sentinel == SentinelType::Slot);
            } else {
                debug_assert!(old_sentinel == SentinelType::Slot);
                debug_assert!(
                    new_sentinel == SentinelType::Start || new_sentinel == SentinelType::End
                );
            }
        }
        self.set_interval_sentinel(new_sentinel);
    }

    /// Returns the underlying page. Is only valid to call if `is_page` is true.
    ///
    /// Do not need to mask any bits out of raw, since PAGE_TYPE has 0's for the type anyway.
    pub fn page(&self) -> VmPagePtr {
        debug_assert!(self.is_page());
        // SAFETY: `self.raw` is guaranteed to be a valid page index when `self` is a page.
        unsafe { pmm_node().index_to_page(self.raw) }
    }

    /// Returns the physical address of the page. Is only valid to call if `is_page` is true.
    ///
    /// Can be more efficient than performing `page().paddr()` as it saves a memory de-reference.
    pub fn page_as_paddr(&self) -> PAddr {
        debug_assert!(self.is_page());
        // SAFETY: `self.raw` is guaranteed to be a valid page index when `self` is a page.
        unsafe { pmm_node().index_to_paddr(self.raw) }
    }

    /// Returns the underlying reference.
    pub fn reference(&self) -> ReferenceValue {
        debug_assert!(self.is_reference());
        ReferenceValue::new(self.raw & !ReferenceValue::ALIGN_MASK)
    }

    /// Resets `self` to Empty and returns the raw underlying representation.
    ///
    /// This gives up ownership of any contained page or reference without running
    /// destructor checks, allowing the caller to transfer raw contents safely.
    fn release(&mut self) -> u32 {
        let ret = self.raw;
        self.raw = Self::PAGE_TYPE;
        ret
    }

    /// If this is a page, moves the underlying `VmPagePtr` out and returns it. After this,
    /// `is_page` will be false and `is_empty` will be true.
    pub fn release_page(&mut self) -> VmPagePtr {
        debug_assert!(self.is_page());
        let raw = self.release();
        // SAFETY: `raw` is guaranteed to be a valid page index when `self` was a page.
        unsafe { pmm_node().index_to_page(raw) }
    }

    /// If this is a reference, moves the underlying reference out and returns it. After this,
    /// `is_reference` will be false and `is_empty` will be true.
    pub fn release_reference(&mut self) -> ReferenceValue {
        debug_assert!(self.is_reference());
        let raw = self.release();
        ReferenceValue::new(raw & !ReferenceValue::ALIGN_MASK)
    }

    /// Changes the content from a reference to a page and returns the original reference.
    pub fn swap_reference_for_page(&mut self, p: VmPagePtr) -> ReferenceValue {
        let ref_val = self.release_reference();
        *self = Self::from_page(p);
        ref_val
    }

    /// Changes the content from a page to a reference and returns the original page.
    pub fn swap_page_for_reference(&mut self, ref_val: ReferenceValue) -> VmPagePtr {
        let page = self.release_page();
        *self = Self::from_reference(ref_val);
        page
    }

    /// Changes the content from one reference to a different one and returns the original
    /// reference.
    pub fn swap_reference_for_reference(&mut self, ref_val: ReferenceValue) -> ReferenceValue {
        let old = self.release_reference();
        *self = Self::from_reference(ref_val);
        old
    }

    /// Swaps content with another `VmPageOrMarker`, returning the previous value of `self`.
    pub fn swap(&mut self, mut other: Self) -> Self {
        let ret = self.raw;
        self.raw = other.release();
        Self { raw: ret }
    }
}

impl Drop for VmPageOrMarker {
    fn drop(&mut self) {
        debug_assert!(
            !self.is_page_or_ref(),
            "VmPageOrMarker dropped while containing page or ref"
        );
    }
}

/// Limited reference to a `VmPageOrMarker`. This reference provides unrestricted const access to
/// the underlying `VmPageOrMarker`, but as it holds a non-const `VmPageOrMarker` pointer it has the
/// ability to modify the underlying entry. However, the interface for modification is very limited.
///
/// This allows for the majority of `VmPageList` iterations that are not intended to allow for
/// clearing entries to the Empty state to allow limited mutation (such as between different content
/// states), without being completely mutable.
pub struct VmPageOrMarkerRef<'a> {
    slot: &'a mut VmPageOrMarker,
}

impl<'a> VmPageOrMarkerRef<'a> {
    /// Creates a new `VmPageOrMarkerRef` wrapping `slot`.
    pub fn new(slot: &'a mut VmPageOrMarker) -> Self {
        Self { slot }
    }

    /// Changing the kind of content is an allowed mutation and this takes ownership of the provided
    /// page and returns ownership of the previous reference.
    pub fn swap_reference_for_page(&mut self, page: VmPagePtr) -> ReferenceValue {
        self.slot.swap_reference_for_page(page)
    }

    /// Similar to `swap_reference_for_page`, but takes ownership of the ref and returns ownership
    /// of the previous page.
    pub fn swap_page_for_reference(&mut self, ref_val: ReferenceValue) -> VmPagePtr {
        self.slot.swap_page_for_reference(ref_val)
    }

    /// Similar to `swap_reference_for_page`, but changes one reference for another.
    pub fn swap_reference_for_reference(&mut self, ref_val: ReferenceValue) -> ReferenceValue {
        self.slot.swap_reference_for_reference(ref_val)
    }

    /// Replaces the contents of this `VmPageOrMarker` with some non-empty contents, and returns
    /// what was previously present. The previous content is allowed to be empty, but the provided
    /// content must be non-empty.
    pub fn swap_content(&mut self, other: VmPageOrMarker) -> VmPageOrMarker {
        debug_assert!(!other.is_empty(), "swap_content requires non-empty content");
        self.slot.swap(other)
    }

    /// Forward dirty state updates as an allowed mutation.
    pub fn set_zero_interval_awaiting_clean_length(&mut self, len: u64) {
        self.slot.set_zero_interval_awaiting_clean_length(len);
    }

    /// Returns the share count of this marker.
    pub fn marker_share_count(&self) -> u32 {
        self.slot.marker_share_count()
    }

    /// Increments the share count of this marker.
    pub fn increment_marker_share_count(&mut self) {
        self.slot.increment_marker_share_count();
    }

    /// Decrements the share count of this marker.
    pub fn decrement_marker_share_count(&mut self) {
        self.slot.decrement_marker_share_count();
    }
}

impl<'a> Deref for VmPageOrMarkerRef<'a> {
    type Target = VmPageOrMarker;

    fn deref(&self) -> &Self::Target {
        self.slot
    }
}

/// Node in a `VmPageList` representing a contiguous 64 KiB span (16 pages) of a VMO.
#[derive(Debug)]
#[repr(C)]
pub struct VmPageListNode {
    pages: [VmPageOrMarker; kVmPageListFanOut],
}

impl VmPageListNode {
    /// Number of page slots in a node.
    pub const PAGE_FAN_OUT: usize = kVmPageListFanOut;

    /// Total size in bytes of the address range covered by a single `VmPageListNode` (64 KiB).
    pub(crate) const NODE_SPAN_BYTES: u64 = (Self::PAGE_FAN_OUT as u64) * (page::SIZE as u64);

    /// Creates a new empty `VmPageListNode`.
    pub fn new() -> Self {
        Self { pages: core::array::from_fn(|_| VmPageOrMarker::empty()) }
    }

    /// Computes the end offset for a node starting at `base_offset`.
    pub fn end_offset(base_offset: u64) -> u64 {
        debug_assert_eq!(Self::node_offset(base_offset), base_offset);
        let end = base_offset + Self::NODE_SPAN_BYTES;
        // By construction the node cannot overflow, but the compiler does not know this. By
        // explicitly telling it, some checks can be avoided as the compiler does not have to
        // consider the case where end wrapped.
        if end <= base_offset {
            // SAFETY: `base_offset` is aligned to `NODE_SPAN_BYTES` and cannot wrap past
            // `u64::MAX`.
            unsafe { core::hint::unreachable_unchecked() };
        }
        end
    }

    /// Lookup the page or marker at the specified index.
    pub fn lookup(&self, index: usize) -> &VmPageOrMarker {
        debug_assert!(index < Self::PAGE_FAN_OUT);
        &self.pages[index]
    }

    /// Lookup the mutable page or marker at the specified index.
    pub fn lookup_mut(&mut self, index: usize) -> &mut VmPageOrMarker {
        debug_assert!(index < Self::PAGE_FAN_OUT);
        &mut self.pages[index]
    }

    /// Returns a raw pointer to the slot at `index`, derived from the raw node pointer without
    /// creating a reference to the node.
    ///
    /// `VmPageList` callers hold raw slot pointers into a node across calls that look up other
    /// slots in the same node. Minting a `&mut VmPageListNode` on those paths would invalidate the
    /// held pointers under the aliasing model, so slot access there must go through this helper
    /// (or an equivalent raw-pointer projection) rather than `lookup_mut`.
    ///
    /// # Safety
    ///
    /// `node` must be non-null and point to a live `VmPageListNode`.
    #[inline]
    unsafe fn slot_ptr(node: *mut Self, index: usize) -> *mut VmPageOrMarker {
        debug_assert!(index < Self::PAGE_FAN_OUT);
        // SAFETY: The caller guarantees `node` points to a live node, so the `pages` projection is
        // in bounds, and `addr_of_mut!` does not create a reference. `index < PAGE_FAN_OUT` keeps
        // the resulting pointer within the `pages` array.
        unsafe { core::ptr::addr_of_mut!((*node).pages).cast::<VmPageOrMarker>().add(index) }
    }

    /// A node is empty if it contains no pages, page interval sentinels, references, or markers.
    #[inline]
    pub fn is_empty(&self) -> bool {
        for p in &self.pages {
            if !p.is_empty() {
                return false;
            }
        }
        true
    }

    /// Returns true if there are no pages or references owned by this node. Meant to check whether
    /// the node has any resource that needs to be returned.
    pub fn has_no_page_or_ref(&self) -> bool {
        for p in &self.pages {
            if p.is_page_or_ref() {
                return false;
            }
        }
        true
    }

    /// Returns true if there are no pages, references or markers owned by this node. Meant to check
    /// whether the node has any resource that needs to be returned.
    pub fn has_no_page_ref_or_marker(&self) -> bool {
        for p in &self.pages {
            if p.is_page_or_ref() || p.is_marker() {
                return false;
            }
        }
        true
    }

    /// Returns true if there are no interval sentinels owned by this node.
    pub fn has_no_interval_sentinel(&self) -> bool {
        for p in &self.pages {
            if p.is_interval() {
                return false;
            }
        }
        true
    }

    /// For every page or marker in the node call the passed in function.
    pub fn for_every_page<F>(&self, base: u64, func: F) -> Status
    where
        F: FnMut(&VmPageOrMarker, u64) -> Status,
    {
        self.for_every_page_in_range(base, base, Self::end_offset(base), func)
    }

    /// For every page or marker in the node call the passed in function.
    pub fn for_every_page_ref<F>(&mut self, base: u64, func: F) -> Status
    where
        F: FnMut(VmPageOrMarkerRef<'_>, u64) -> Status,
    {
        self.for_every_page_in_range_ref(base, base, Self::end_offset(base), func)
    }

    /// For every page or marker in the node in the range call the passed in function. The range
    /// is assumed to be within the node's object range.
    pub fn for_every_page_in_range<F>(
        &self,
        base: u64,
        start_offset: u64,
        end_offset: u64,
        mut func: F,
    ) -> Status
    where
        F: FnMut(&VmPageOrMarker, u64) -> Status,
    {
        debug_assert!(end_offset >= start_offset);
        debug_assert!(start_offset >= base);
        debug_assert!(end_offset <= Self::end_offset(base));
        let start = ((start_offset - base) / (page::SIZE as u64)) as usize;
        let end = ((end_offset - base) / (page::SIZE as u64)) as usize;
        #[allow(clippy::needless_range_loop)]
        for i in start..end {
            if !self.pages[i].is_empty() {
                let status = func(&self.pages[i], base + (i as u64) * (page::SIZE as u64));
                if status != Status::NEXT {
                    return status;
                }
            }
        }
        Status::NEXT
    }

    /// For every page or marker in the node in the range call the passed in function with a
    /// [`VmPageOrMarkerRef`]. The range is assumed to be within the node's object range.
    pub fn for_every_page_in_range_ref<F>(
        &mut self,
        base: u64,
        start_offset: u64,
        end_offset: u64,
        mut func: F,
    ) -> Status
    where
        F: FnMut(VmPageOrMarkerRef<'_>, u64) -> Status,
    {
        self.for_every_page_in_range_mut(base, start_offset, end_offset, |slot, offset| {
            func(VmPageOrMarkerRef::new(slot), offset)
        })
    }

    /// For every page or marker in the node in the range call the passed in function with
    /// mutable access. The range is assumed to be within the node's object range.
    pub fn for_every_page_in_range_mut<F>(
        &mut self,
        base: u64,
        start_offset: u64,
        end_offset: u64,
        mut func: F,
    ) -> Status
    where
        F: FnMut(&mut VmPageOrMarker, u64) -> Status,
    {
        debug_assert!(end_offset >= start_offset);
        debug_assert!(start_offset >= base);
        debug_assert!(end_offset <= Self::end_offset(base));
        let start = ((start_offset - base) / (page::SIZE as u64)) as usize;
        let end = ((end_offset - base) / (page::SIZE as u64)) as usize;
        #[allow(clippy::needless_range_loop)]
        for i in start..end {
            if !self.pages[i].is_empty() {
                let status = func(&mut self.pages[i], base + (i as u64) * (page::SIZE as u64));
                if status != Status::NEXT {
                    return status;
                }
            }
        }
        Status::NEXT
    }

    /// Checks if the given offset is part of an interval involving this node. This method cannot
    /// find the full interval, since that may require looking at an additional node, but can
    /// determine if in an interval or not. Returns any interval sentinel found, otherwise `None`.
    pub fn is_offset_in_interval(&self, obj_offset: u64, off: u64) -> Option<&VmPageOrMarker> {
        debug_assert!(off >= obj_offset);
        debug_assert!(off < Self::end_offset(obj_offset));
        let index = ((off - obj_offset) / (page::SIZE as u64)) as usize;
        // If the target slot is any kind of interval (start, end, individual slot), then we are in
        // an interval.
        if !self.pages[index].is_empty() {
            return if self.pages[index].is_interval() { Some(&self.pages[index]) } else { None };
        }
        // Check if there is an interval end to the right, which would cause this to be in an
        // interval. Finding anything else indicates we cannot be in an interval.
        #[allow(clippy::needless_range_loop)]
        for i in (index + 1)..Self::PAGE_FAN_OUT {
            if !self.pages[i].is_empty() {
                return if self.pages[i].is_interval_end() { Some(&self.pages[i]) } else { None };
            }
        }
        // Nothing to our right, so check for an interval start to our left.
        for i in (0..index).rev() {
            if !self.pages[i].is_empty() {
                return if self.pages[i].is_interval_start() { Some(&self.pages[i]) } else { None };
            }
        }
        panic!("Unexpected empty node");
    }

    /// Check if this node begins in an interval, that is if an interval start was in a preceding
    /// node and this nodes contains the end. If the first non-empty slot is an interval end it is
    /// returned, otherwise we cannot have started in an interval and `None` is returned.
    pub fn node_starts_in_interval(&self) -> Option<&VmPageOrMarker> {
        for p in &self.pages {
            if !p.is_empty() {
                return if p.is_interval_end() { Some(p) } else { None };
            }
        }
        panic!("Unexpected empty node");
    }

    #[allow(clippy::too_many_arguments)]
    pub fn merge_range_onto<F>(
        &mut self,
        base: u64,
        other_base: u64,
        mut migrate_fn: F,
        other: &mut VmPageListNode,
        start_offset: u64,
        end_offset: u64,
        other_start_offset: u64,
    ) where
        F: FnMut(&mut VmPageOrMarker, &mut VmPageOrMarker, u64),
    {
        debug_assert!(other_start_offset >= other_base);
        debug_assert!(
            other_start_offset + (end_offset - start_offset) <= Self::end_offset(other_base)
        );
        let _ = self.for_every_page_in_range_mut(base, start_offset, end_offset, |slot, offset| {
            let other_offset = offset - start_offset + other_start_offset;
            debug_assert_eq!(Self::node_offset(other_offset), other_base);
            let other_index = Self::node_index(other_offset);
            migrate_fn(slot, &mut other.pages[other_index], other_offset);
            Status::NEXT
        });
    }

    /// Converts the supplied offset into a VmPageListNode base offset.
    pub const fn node_offset(offset: u64) -> u64 {
        offset & !(Self::NODE_SPAN_BYTES - 1)
    }

    /// Converts the supplied offset into a VmPageListNode index.
    pub const fn node_index(offset: u64) -> usize {
        ((offset >> (page::SHIFT as u32)) % (Self::PAGE_FAN_OUT as u64)) as usize
    }
}

impl Default for VmPageListNode {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VmPageListNode {
    fn drop(&mut self) {
        debug_assert!(
            self.has_no_page_or_ref(),
            "VmPageListNode dropped while containing page or ref"
        );
    }
}

/// A sparse list of pages organized into 64 KiB (16-page) `VmPageListNode` chunks.
#[repr(C)]
pub struct VmPageList {
    list: Opaque<bindings::VmPageListBtree>,
}

zr::static_assert!(core::mem::size_of::<VmPageListNode>() == 64);
zr::static_assert!(core::mem::align_of::<VmPageListNode>() == 4);
zr::static_assert!(
    core::mem::size_of::<VmPageList>() == core::mem::size_of::<bindings::VmPageListBtree>()
);
zr::static_assert!(
    core::mem::align_of::<VmPageList>() == core::mem::align_of::<bindings::VmPageListBtree>()
);

#[derive(Copy, Clone)]
struct VmPageListBtreeNodeEntry {
    offset: u64,
    node: *mut core::ffi::c_void,
}

// The B-Tree FFI functions below return the node pointer directly and write the offset through an
// out-pointer rather than returning a `{ u64, *mut c_void }` struct by value. On x86_64, Clang and
// rustc lower a `{ u64, *mut c_void }` return type to different LLVM IR types (`{ i64, ptr }` vs.
// `{ i64, i64 }`), which prevents ThinLTO from inlining the C++ wrappers.

/// # Safety
///
/// `tree` must point to an initialized `VmPageListBtree`, and `out_cursor` must be null or point to
/// valid storage for a `VmPageListBtreeCursor`.
#[inline]
unsafe fn btree_lower_bound(
    tree: *mut bindings::VmPageListBtree,
    node_offset: u64,
    out_cursor: *mut bindings::VmPageListBtreeCursor,
) -> VmPageListBtreeNodeEntry {
    let mut offset = 0;
    // SAFETY: Guaranteed by caller.
    let node = unsafe {
        bindings::cpp_vm_page_list_btree_lower_bound(tree, node_offset, out_cursor, &mut offset)
    };
    VmPageListBtreeNodeEntry { offset, node }
}

/// # Safety
///
/// `cursor` must point to an initialized `VmPageListBtreeCursor`.
#[inline]
unsafe fn btree_cursor_get(
    cursor: *const bindings::VmPageListBtreeCursor,
) -> VmPageListBtreeNodeEntry {
    let mut offset = 0;
    // SAFETY: Guaranteed by caller.
    let node = unsafe { bindings::cpp_vm_page_list_btree_cursor_get(cursor, &mut offset) };
    VmPageListBtreeNodeEntry { offset, node }
}

/// # Safety
///
/// `cursor` must point to an initialized `VmPageListBtreeCursor`.
#[inline]
unsafe fn btree_cursor_next(
    cursor: *mut bindings::VmPageListBtreeCursor,
) -> VmPageListBtreeNodeEntry {
    let mut offset = 0;
    // SAFETY: Guaranteed by caller.
    let node = unsafe { bindings::cpp_vm_page_list_btree_cursor_next(cursor, &mut offset) };
    VmPageListBtreeNodeEntry { offset, node }
}

/// # Safety
///
/// `cursor` must point to an initialized `VmPageListBtreeCursor`.
#[inline]
unsafe fn btree_cursor_prev(
    cursor: *mut bindings::VmPageListBtreeCursor,
) -> VmPageListBtreeNodeEntry {
    let mut offset = 0;
    // SAFETY: Guaranteed by caller.
    let node = unsafe { bindings::cpp_vm_page_list_btree_cursor_prev(cursor, &mut offset) };
    VmPageListBtreeNodeEntry { offset, node }
}

/// # Safety
///
/// `cursor` must point to an initialized `VmPageListBtreeConstCursor`.
#[inline]
unsafe fn btree_const_cursor_next(
    cursor: *mut bindings::VmPageListBtreeConstCursor,
) -> *const core::ffi::c_void {
    let mut offset = 0;
    // SAFETY: Guaranteed by caller.
    unsafe { bindings::cpp_vm_page_list_btree_const_cursor_next(cursor, &mut offset) }
}

struct NodeIter<'a> {
    cursor: Opaque<bindings::VmPageListBtreeConstCursor>,
    _phantom: core::marker::PhantomData<&'a VmPageList>,
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = &'a VmPageListNode;

    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: `self.cursor.get()` is a valid initialized cursor pointer.
        let node = unsafe { btree_const_cursor_next(self.cursor.get()) };
        if node.is_null() {
            None
        } else {
            // SAFETY: `node` is verified non-null and valid for lifetime `'a`.
            Some(unsafe { node.cast::<VmPageListNode>().as_ref_unchecked() })
        }
    }
}

struct NodeIterMut<'a> {
    cursor: Opaque<bindings::VmPageListBtreeCursor>,
    _phantom: core::marker::PhantomData<&'a mut VmPageList>,
}

impl<'a> Iterator for NodeIterMut<'a> {
    type Item = &'a mut VmPageListNode;

    fn next(&mut self) -> Option<Self::Item> {
        // SAFETY: `self.cursor.get()` is a valid initialized cursor pointer.
        let entry = unsafe { btree_cursor_next(self.cursor.get()) };
        if entry.node.is_null() {
            None
        } else {
            // SAFETY: `entry.node` is verified non-null and valid for lifetime `'a`.
            Some(unsafe { entry.node.cast::<VmPageListNode>().as_mut_unchecked() })
        }
    }
}

/// Cursor that can be used for iterating over contiguous blocks of entries in a page list. The
/// underlying page list must not have any entries removed while using this cursor, as the cursor
/// retains iterators into the page list. It is, however, safe to insert new entries.
///
/// The cursor can be used to iterate over empty contiguous slots, however iteration will always
/// cease if entries are not contiguous.
pub struct VmPageListCursor<'a> {
    cursor: Opaque<bindings::VmPageListBtreeCursor>,
    /// The index into the node that is currently being pointed at to be returned by `current`. The
    /// sentinel value of `VmPageListNode::PAGE_FAN_OUT` is used to indicate that the node is no
    /// longer valid.
    index: usize,
    _phantom: core::marker::PhantomData<&'a mut VmPageList>,
}

impl Default for VmPageListCursor<'_> {
    /// Constructs a cursor that is already exhausted, i.e. `current` will return `None`.
    fn default() -> Self {
        let cursor = Opaque::uninit();
        // SAFETY: `cursor.get()` is a valid pointer to be initialized to a default invalid
        // iterator.
        unsafe { bindings::cpp_vm_page_list_btree_cursor_default_init(cursor.get()) };
        Self { cursor, index: VmPageListNode::PAGE_FAN_OUT, _phantom: core::marker::PhantomData }
    }
}

impl<'a> VmPageListCursor<'a> {
    /// Constructs a cursor pointing at `index` inside the node that `cursor` refers to.
    ///
    /// # Safety
    ///
    /// The caller must ensure `cursor` is an initialized iterator that points at a node of a
    /// `VmPageList` that is mutably borrowed for `'a`.
    unsafe fn new(cursor: Opaque<bindings::VmPageListBtreeCursor>, index: usize) -> Self {
        debug_assert!(index < VmPageListNode::PAGE_FAN_OUT);
        Self { cursor, index, _phantom: core::marker::PhantomData }
    }

    /// Retrieve the current `VmPageOrMarker` pointed at by the cursor. This will be `None` if the
    /// cursor is no longer valid. The slot pointed at may itself be empty.
    ///
    /// Note that it is up to the caller to know the offset, which it can track by remembering how
    /// many `step`s it has done.
    pub fn current(&self) -> Option<&VmPageOrMarker> {
        if !self.valid() {
            return None;
        }
        let entry = self.get();
        // SAFETY: the cursor is valid, so `entry.node` is non-null and points at a live node that
        // outlives the borrow of `self`.
        let node = unsafe { entry.node.cast::<VmPageListNode>().as_ref_unchecked() };
        Some(node.lookup(self.index))
    }

    /// See `current`. Returns a `VmPageOrMarkerRef` that allows for limited mutation of the slot.
    pub fn current_ref(&mut self) -> Option<VmPageOrMarkerRef<'_>> {
        if !self.valid() {
            return None;
        }
        let entry = self.get();
        // SAFETY: the cursor is valid, so `entry.node` is non-null and points at a live node that
        // outlives the borrow of `self`. The cursor holds the only mutable borrow of the list.
        let node = unsafe { entry.node.cast::<VmPageListNode>().as_mut_unchecked() };
        Some(VmPageOrMarkerRef::new(node.lookup_mut(self.index)))
    }

    /// Move the cursor to the next entry. The next entry can then be retrieved by calling
    /// `current`, and if there is no next entry then `current` will return `None`.
    pub fn step(&mut self) {
        if self.valid() {
            self.index += 1;
            if self.index == VmPageListNode::PAGE_FAN_OUT {
                self.inc_node();
            }
        }
    }

    /// Calls the provided callback on every entry as long as they are contiguous. This is
    /// equivalent to a loop calling `step` and `current`, but can produce more optimal code gen
    /// with the internal loop.
    ///
    /// The callback can return `Status::NEXT` to continue, `Status::STOP` to cease iteration
    /// gracefully, or any other status to terminate with that status code.
    pub fn for_every_contiguous<F>(&mut self, mut func: F) -> Result<(), Status>
    where
        F: FnMut(&VmPageOrMarker) -> Status,
    {
        while self.valid() {
            let entry = self.get();
            // SAFETY: the cursor is valid, so `entry.node` is non-null and points at a live node.
            // The node is not mutated by the loop below, which only advances `self.index`.
            let node = unsafe { entry.node.cast::<VmPageListNode>().as_ref_unchecked() };
            while self.index < VmPageListNode::PAGE_FAN_OUT {
                let status = func(node.lookup(self.index));
                if status != Status::NEXT {
                    if status == Status::STOP {
                        return Ok(());
                    }
                    return Err(status);
                }
                self.index += 1;
            }
            if !self.inc_node() {
                return Ok(());
            }
        }
        Ok(())
    }

    /// Returns the offset of the `current` position of the cursor. This is invalid to call if
    /// `current` is returning `None`.
    pub fn offset(&self) -> u64 {
        debug_assert!(self.valid());
        self.get().offset + (self.index as u64) * (page::SIZE as u64)
    }

    /// Helper to increment the underlying node, testing for contiguity. Returns whether the cursor
    /// is still valid after the increment.
    fn inc_node(&mut self) -> bool {
        // Should only be incrementing if index is at the end, as otherwise we're not being
        // contiguous.
        debug_assert_eq!(self.index, VmPageListNode::PAGE_FAN_OUT);
        // `cursor_next` returns the entry that the iterator was pointing at prior to advancing it.
        // SAFETY: `self.cursor.get()` is a valid initialized cursor pointer.
        let prev = unsafe { btree_cursor_next(self.cursor.get()) };
        // SAFETY: `self.cursor.get()` is a valid initialized cursor pointer.
        let entry = unsafe { btree_cursor_get(self.cursor.get()) };
        // A null node means that the iterator has moved past the last node. Otherwise `entry`
        // follows `prev` in key order and both offsets are multiples of `NODE_SPAN_BYTES`, so the
        // sum is at most `entry.offset` and cannot overflow.
        if !entry.node.is_null() && entry.offset == prev.offset + VmPageListNode::NODE_SPAN_BYTES {
            // The node is valid and contiguous, reset the index to both remove the terminal
            // sentinel, and resume iteration from the beginning.
            self.index = 0;
            // TODO: Once cursor is in use benchmark the impact of validating that the node is not
            // empty.
            return true;
        }
        false
    }

    /// Helper to check if the node is valid or not by checking index for its sentinel value.
    fn valid(&self) -> bool {
        self.index < VmPageListNode::PAGE_FAN_OUT
    }

    /// Returns the entry that the underlying iterator points at. This must only be called while the
    /// cursor is `valid()`, which guarantees that the iterator points at a node.
    fn get(&self) -> VmPageListBtreeNodeEntry {
        // SAFETY: `self.cursor.get()` is a valid initialized cursor pointer.
        let entry = unsafe { btree_cursor_get(self.cursor.get()) };
        debug_assert!(!entry.node.is_null());
        entry
    }
}

/// Helper object for performing repeated `lookup_or_allocate` operations that are likely to be
/// close to each other. While using this object other (modifying) methods on this `VmPageList` must
/// not be performed, if they are the `reset` method needs to be used before continuing.
///
/// This is `#[repr(C)]` with the same two members, in the same order, as the C++
/// `VmPageList::BatchInserter`, so that the C++ `this` pointer can be reinterpreted as a
/// `BatchInserter` by `rust_vm_page_list_batch_inserter_lookup_or_allocate`. The layout is asserted
/// on both sides. The C++ member is a `VmPageListBtree&`, i.e. the address of `VmPageList::list_`;
/// it reads as a valid `&mut VmPageList` here because `list` is the sole, first member of the
/// `#[repr(C)]` `VmPageList`.
#[repr(C)]
pub struct BatchInserter<'a> {
    list: &'a mut VmPageList,
    node: Opaque<bindings::VmPageListBtreeCursor>,
}

zr::static_assert!(core::mem::offset_of!(VmPageList, list) == 0);
zr::static_assert!(
    core::mem::size_of::<BatchInserter<'_>>()
        == core::mem::size_of::<*mut VmPageList>()
            + core::mem::size_of::<bindings::VmPageListBtreeCursor>()
);
zr::static_assert!(
    core::mem::align_of::<BatchInserter<'_>>() == core::mem::align_of::<*mut VmPageList>()
);

impl<'a> BatchInserter<'a> {
    /// Construct a `BatchInserter` for the specified `VmPageList`. The list must be kept alive for
    /// the duration of this object.
    pub fn new(list: &'a mut VmPageList) -> Self {
        let cursor = Opaque::uninit();
        // SAFETY: `cursor.get()` is a valid pointer to be initialized to default invalid iterator.
        unsafe { bindings::cpp_vm_page_list_btree_cursor_default_init(cursor.get()) };
        Self { list, node: cursor }
    }

    /// Similar to `VmPageList::lookup_or_allocate` but is implicitly `NoIntervals`. If repeated
    /// offsets are 'near' each other (in the same node, or in following nodes) then this will be
    /// more efficient than `VmPageList::lookup_or_allocate`. However, regardless of the `offset`
    /// pattern this will always return correct results.
    pub fn lookup_or_allocate(&mut self, offset: u64) -> Option<&mut VmPageOrMarker> {
        let target_offset = VmPageListNode::node_offset(offset);
        let index = VmPageListNode::node_index(offset);

        // Assume we're going to need to search for a new iterator.
        let mut search = true;

        // First check if the currently saved iterator is valid and for the correct node.
        // SAFETY: `self.node.get()` is a valid pointer.
        let mut entry = unsafe { btree_cursor_get(self.node.get()) };
        if !entry.node.is_null() {
            if entry.offset == target_offset {
                let node_ptr = entry.node.cast::<VmPageListNode>();
                // SAFETY: `node_ptr` is non-null and points to a live node owned by `self.list`.
                // The slot is derived from the raw node pointer rather than through a
                // `&mut VmPageListNode` so that no reference is minted over slots the caller may
                // still hold pointers to from earlier calls.
                return Some(unsafe { &mut *VmPageListNode::slot_ptr(node_ptr, index) });
            }
            // Under the assumption of contiguous insertion, check if incrementing the iterator
            // helps.
            if entry.offset < target_offset {
                // Advance the cursor to the next node.
                // SAFETY: `self.node.get()` is a valid pointer.
                let _ = unsafe { btree_cursor_next(self.node.get()) };
                // Peek at the new node.
                // SAFETY: `self.node.get()` is a valid pointer.
                entry = unsafe { btree_cursor_get(self.node.get()) };
                if entry.node.is_null() {
                    // If we hit the end then we know a new node is needed, so skip the search and
                    // go straight to new node creation.
                    search = false;
                } else if entry.offset == target_offset {
                    let node_ptr = entry.node.cast::<VmPageListNode>();
                    // SAFETY: As above.
                    return Some(unsafe { &mut *VmPageListNode::slot_ptr(node_ptr, index) });
                }
            }
        }

        // Unless we know that the node we want isn't in the tree we must do a search for it to
        // avoid duplicate insertion.
        if search {
            // Even if this is not our target node, we stash the result to use to optimize the
            // insertion later.
            // SAFETY: `self.list.list.get()` and `self.node.get()` are valid pointers.
            entry = unsafe {
                btree_lower_bound(self.list.list.get(), target_offset, self.node.get())
            };
            if !entry.node.is_null() && entry.offset == target_offset {
                let node_ptr = entry.node.cast::<VmPageListNode>();
                // SAFETY: As above.
                return Some(unsafe { &mut *VmPageListNode::slot_ptr(node_ptr, index) });
            }
        }

        // SAFETY: `self.list.list.get()` and `self.node.get()` are valid pointers.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_insert(
                self.list.list.get(),
                target_offset,
                self.node.get(),
            )
        };
        if node_ptr.is_null() {
            return None;
        }
        let node_ptr = node_ptr.cast::<VmPageListNode>();
        // SAFETY: `node_ptr` was allocated by `VmPageListNode::Create()` and inserted into the
        // B-tree. See above for why the slot is derived from the raw node pointer.
        Some(unsafe { &mut *VmPageListNode::slot_ptr(node_ptr, index) })
    }

    /// Reset the batch inserter. This makes it safe to use again if other `VmPageList` operations
    /// had been performed.
    pub fn reset(&mut self) {
        // SAFETY: `self.node.get()` is a valid pointer to be reset to default invalid iterator.
        unsafe { bindings::cpp_vm_page_list_btree_cursor_default_init(self.node.get()) };
    }
}

/// The interval handling flag to be used by `lookup_or_allocate`. See comments near
/// `lookup_or_allocate`.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum IntervalHandling {
    NoIntervals,
    CheckForInterval,
    SplitInterval,
}

/// Behavior for checking and cleaning up empty nodes during range traversals.
trait NodeCheck {
    const CLEANUP_EMPTY: bool;
}

/// Traverse nodes without checking for or removing empty nodes.
enum SkipNodeCheck {}
impl NodeCheck for SkipNodeCheck {
    const CLEANUP_EMPTY: bool = false;
}

/// Check if nodes are empty after running the callback and erase them if so.
enum CleanupEmptyNodeCheck {}
impl NodeCheck for CleanupEmptyNodeCheck {
    const CLEANUP_EMPTY: bool = true;
}

const fn round_down(val: u64, align: u64) -> u64 {
    debug_assert!(align.is_power_of_two());
    val & !(align - 1)
}

/// An entry visited by `VmPageList::for_every_page_and_gap_in_range_internal`.
enum PageOrGap<'a> {
    /// A page/ref/marker or interval sentinel, and its offset.
    Page(&'a VmPageOrMarker, u64),
    /// A gap (unpopulated range), as `(gap_start, gap_end)`.
    Gap(u64, u64),
}

/// An entry visited by `VmPageList::for_every_page_and_gap_in_range_internal_mut`.
enum PageOrGapMut<'a> {
    /// A page/ref/marker or interval sentinel, and its offset.
    Page(&'a mut VmPageOrMarker, u64),
    /// A gap (unpopulated range), as `(gap_start, gap_end)`.
    Gap(u64, u64),
}

impl VmPageList {
    /// Allow the implementation to use a one-past-the-end for VmPageListNode offsets.
    pub const MAX_SIZE: u64 = round_down(u64::MAX, VmPageListNode::NODE_SPAN_BYTES);

    /// Constructor. Creates a new empty `VmPageList`.
    pub fn new() -> Self {
        let list = Self { list: Opaque::uninit() };
        // SAFETY: `list.list.get()` points to valid storage allocated for VmPageListBtree.
        unsafe {
            bindings::cpp_vm_page_list_btree_init(list.list.get().cast());
        }
        ltracef!("{:p}\n", core::ptr::from_ref(&list));
        list
    }

    fn nodes(&self) -> NodeIter<'_> {
        let cursor = Opaque::uninit();
        // SAFETY: `self.list.get()` is a valid initialized BTree, `cursor.get()` is uninitialized
        // storage.
        unsafe {
            bindings::cpp_vm_page_list_btree_const_cursor_init(cursor.get(), self.list.get());
        }
        NodeIter { cursor, _phantom: core::marker::PhantomData }
    }

    fn nodes_mut(&mut self) -> NodeIterMut<'_> {
        let cursor = Opaque::uninit();
        // SAFETY: `self.list.get()` is a valid initialized BTree, `cursor.get()` is uninitialized
        // storage.
        unsafe {
            bindings::cpp_vm_page_list_btree_cursor_init(cursor.get(), self.list.get());
        }
        NodeIterMut { cursor, _phantom: core::marker::PhantomData }
    }

    /// Returns true if there are no pages, references, markers, or intervals in the page list.
    pub fn is_empty(&self) -> bool {
        // SAFETY: `self.list.get()` is a valid initialized VmPageListBtree pointer.
        unsafe { bindings::cpp_vm_page_list_btree_is_empty(self.list.get()) }
    }

    /// Returns true if the page list does not own any pages or references. Meant to check whether
    /// the page list has any resource that needs to be returned.
    pub fn has_no_page_or_ref(&self) -> bool {
        self.nodes().all(|node| node.has_no_page_or_ref())
    }

    /// Similar to `has_no_page_or_ref` but returns false if there is a marker.
    pub fn has_no_page_ref_or_marker(&self) -> bool {
        self.nodes().all(|node| node.has_no_page_ref_or_marker())
    }

    /// Returns the number of bytes of heap allocated by this page list.
    ///
    /// This walks the entire tree and so is not a constant time operation.
    pub fn heap_allocation_bytes(&self) -> u64 {
        // SAFETY: `self.list.get()` is a valid initialized VmPageListBtree pointer.
        let utilization =
            unsafe { bindings::cpp_vm_page_list_btree_calculate_utilization_slow(self.list.get()) };
        utilization.nodes_in_bytes
            + utilization.stored_values * core::mem::size_of::<VmPageListNode>() as u64
    }

    /// Clears the tree of any remaining slots, leaving it in the initially allocated state. It is
    /// an error, and will trigger a panic, for any of the slots to hold pages or references, as
    /// clearing them would otherwise result in a memory leak.
    pub fn clear(&mut self) {
        // SAFETY: `self.list.get()` is a valid initialized VmPageListBtree pointer.
        unsafe {
            bindings::cpp_vm_page_list_btree_clear(self.list.get());
        }
    }

    /// Attempts to return a reference to the `VmPageOrMarker` at the specified offset. The returned
    /// pointer is valid until the `VmPageList` is destroyed or any of the Remove*/Take/Merge etc
    /// functions are called.
    ///
    /// Lookup may return None if there is no slot allocated for the given offset. If Some is
    /// returned it may still be the case that `is_empty` on the returned `VmPageOrMarker` is true.
    pub fn lookup(&self, offset: u64) -> Option<&VmPageOrMarker> {
        let node_offset = VmPageListNode::node_offset(offset);
        // SAFETY: `self.list.get()` is valid and `cpp_vm_page_list_btree_find_const` returns a
        // valid node or null.
        let node_ptr =
            unsafe { bindings::cpp_vm_page_list_btree_find_const(self.list.get(), node_offset) };
        if node_ptr.is_null() {
            return None;
        }
        // SAFETY: `node_ptr` is verified non-null and points to a valid `VmPageListNode`.
        let node_ref: &VmPageListNode =
            unsafe { node_ptr.cast::<VmPageListNode>().as_ref_unchecked() };
        Some(node_ref.lookup(VmPageListNode::node_index(offset)))
    }

    /// Similar to `lookup` but returns a `VmPageOrMarkerRef` that allows for limited mutation of
    /// the slot. General mutation requires calling `lookup_or_allocate`.
    pub fn lookup_mut(&mut self, offset: u64) -> Option<VmPageOrMarkerRef<'_>> {
        let slot_ptr = self.lookup_slot(offset);
        if slot_ptr.is_null() {
            None
        } else {
            // SAFETY: `slot_ptr` is verified non-null and points to a valid `VmPageOrMarker`.
            Some(VmPageOrMarkerRef::new(unsafe { &mut *slot_ptr }))
        }
    }

    /// Similar to `lookup_mut` but returns a `VmPageListCursor` that allows for iterating over any
    /// contiguous slots from the provided offset.
    pub fn lookup_mut_cursor(&mut self, offset: u64) -> VmPageListCursor<'_> {
        // Lookup the tree node that holds this offset.
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_find(
                self.list.get(),
                VmPageListNode::node_offset(offset),
                cursor.get(),
            )
        };
        if node_ptr.is_null() {
            return VmPageListCursor::default();
        }
        // SAFETY: the node was found, so `cursor` was initialized to point at it. The returned
        // cursor mutably borrows `self`, which owns the node.
        unsafe { VmPageListCursor::new(cursor, VmPageListNode::node_index(offset)) }
    }

    /// Similar to `lookup_mut_cursor` but does a lower bound search instead of a find, returning
    /// the first slot >= `offset`, if any exists.
    pub fn lookup_nearest_mut_cursor(&mut self, offset: u64) -> VmPageListCursor<'_> {
        // Lookup the tree node that holds this offset or a larger one.
        let node_offset = VmPageListNode::node_offset(offset);
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let entry = unsafe { btree_lower_bound(self.list.get(), node_offset, cursor.get()) };
        if entry.node.is_null() {
            return VmPageListCursor::default();
        }
        let index =
            if entry.offset == node_offset { VmPageListNode::node_index(offset) } else { 0 };
        // SAFETY: a node was found, so `cursor` points at it. The returned cursor mutably borrows
        // `self`, which owns the node.
        unsafe { VmPageListCursor::new(cursor, index) }
    }

    /// Similar to `lookup` but only returns None if a slot cannot be allocated either due to out
    /// of memory, due to offset being invalid, or `interval_handling` not allowing for a slot to
    /// be safely returned.
    ///
    /// The returned slot, if not None, may generally be freely manipulated with the exception
    /// that if it started `!is_empty()`, then it is an error to set it to `is_empty()`. In this
    /// case the `remove_page` method must be used.
    ///
    /// If the returned slot started `is_empty()`, and is not made `!is_empty()`, then the slot must
    /// be returned with `return_empty_slot`, to ensure no empty nodes are retained.
    ///
    /// The bool in the return tuple returns whether the offset falls inside a sparse interval.
    /// And whether a valid `VmPageOrMarker` is returned in the return tuple depends on the
    /// specified `interval_handling`.
    ///  - NoIntervals: The page list does not contain any intervals, so there is no special
    ///    handling to check for or split intervals. In other words, each slot in the page list can
    ///    be manipulated independently.
    ///  - CheckForInterval: The page list can contain intervals, and the bool in the returned
    ///    tuple indicates whether the offset fell inside an interval. Note that this only checks
    ///    for intervals but does not allow manipulating them, so a valid `VmPageOrMarker` will be
    ///    returned only if the offset can safely be manipulated independently.
    ///  - SplitInterval: The page list can contain intervals and we are allowed to split
    ///    intervals to return the required slot. The returned `VmPageOrMarker` can be manipulated
    ///    freely. (See comments near `lookup_or_allocate_check_for_interval` for an explanation of
    ///    how splitting works.)
    pub fn lookup_or_allocate(
        &mut self,
        offset: u64,
        interval_handling: IntervalHandling,
    ) -> (Option<&mut VmPageOrMarker>, bool) {
        match interval_handling {
            IntervalHandling::NoIntervals => {
                let slot = self.lookup_or_allocate_internal(offset);
                // SAFETY: `lookup_or_allocate_internal` returns either null or a pointer to a live
                // slot in `self.list`, valid for the lifetime of `&mut self`.
                (unsafe { slot.as_mut() }, false)
            }
            IntervalHandling::CheckForInterval => {
                self.lookup_or_allocate_check_for_interval(offset, false)
            }
            IntervalHandling::SplitInterval => {
                self.lookup_or_allocate_check_for_interval(offset, true)
            }
        }
    }

    /// Internal helper for `lookup_or_allocate`.
    ///
    /// Returns a raw pointer rather than `Option<&mut VmPageOrMarker>` so callers that look up
    /// multiple slots across calls (e.g. `add_zero_interval_internal`, `overwrite_zero_interval`)
    /// can hold the first slot pointer across a second call without minting an intermediate `&mut`.
    fn lookup_or_allocate_internal(&mut self, offset: u64) -> *mut VmPageOrMarker {
        let node_offset = VmPageListNode::node_offset(offset);
        let index = VmPageListNode::node_index(offset);

        if node_offset >= Self::MAX_SIZE {
            return core::ptr::null_mut();
        }

        ltracef_level!(
            2,
            "{:p} offset {:#x} node_offset {:#x} index {}\n",
            core::ptr::from_ref(self),
            offset,
            node_offset,
            index
        );

        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // lookup the tree node that holds this page. Use lower_bound instead of find to optimize
        // later insertion in case of failed lookup.
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let entry = unsafe { btree_lower_bound(self.list.get(), node_offset, cursor.get()) };
        if !entry.node.is_null() && entry.offset == node_offset {
            let node_ptr = entry.node.cast::<VmPageListNode>();
            // SAFETY: `node_ptr` is non-null and points to a live node owned by `self.list`. The
            // slot is derived from the raw node pointer rather than through a
            // `&mut VmPageListNode` so that no reference is minted over slots the caller may still
            // hold pointers to from earlier calls.
            return unsafe { VmPageListNode::slot_ptr(node_ptr, index) };
        }

        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_insert(self.list.get(), node_offset, cursor.get())
        };
        if node_ptr.is_null() {
            return core::ptr::null_mut();
        }
        ltracef!("allocating new inner node {:p}\n", node_ptr);

        let node_ptr = node_ptr.cast::<VmPageListNode>();
        // SAFETY: `node_ptr` was allocated by `VmPageListNode::Create()` and inserted into the
        // B-tree. See above for why the slot is derived from the raw node pointer.
        unsafe { VmPageListNode::slot_ptr(node_ptr, index) }
    }

    /// Similar to `lookup_or_allocate_internal` but also checks if `offset` falls in a sparse page
    /// interval, returning true via the bool in the tuple if it does, along with the slot. Also
    /// splits the interval around `offset` if `split_interval` is set to true. This allows the
    /// caller to freely manipulate the slot at `offset` similar to `lookup_or_allocate`. If
    /// `offset` is found in an interval, but `split_interval` was false, no `VmPageOrMarker` is
    /// returned, as it is not safe to manipulate any slot in an interval without also splitting the
    /// interval around it.
    ///
    /// In other words, the return values fall into three categories:
    ///  1. `(Some(page), false)`: `offset` does not lie in an interval. `page` is the required
    ///     slot.
    ///  2. `(Some(page), true)`: `offset` lies in an interval and `split_interval` was true. `page`
    ///     is the required slot. The interval has been correctly split around the slot, so `page`
    ///     can be treated similar to any non-interval type.
    ///  3. `(None, true)`: `offset` lies in an interval but `split_interval` was false. No slot is
    ///     returned.
    ///
    /// Splitting the interval would look as follows. If the interval previously was:
    ///  `[start, end)` where `start < offset < end`.
    /// After the split we would have three intervals:
    ///  `[start, offset) [offset, offset + page::SIZE) [offset + page::SIZE, end)`
    /// The middle interval containing `offset` spans only a single page, i.e. `offset` is an
    /// `SentinelType::Slot`, which can now be manipulated independently.
    fn lookup_or_allocate_check_for_interval(
        &mut self,
        offset: u64,
        split_interval: bool,
    ) -> (Option<&mut VmPageOrMarker>, bool) {
        // Find the node that would contain this offset.
        let node_offset = VmPageListNode::node_offset(offset);
        let node_index = VmPageListNode::node_index(offset);
        if node_offset >= Self::MAX_SIZE {
            return (None, false);
        }

        // If the node containing offset is populated, the lower bound will return that node. If not
        // populated, it will return the next populated node.
        //
        // The overall intent with this function is to keep the number of tree lookups similar to
        // lookup_or_allocate for both empty and non-empty slots. So we hold on to the looked up
        // node and walk left or right in the tree only if required. The same principle is followed
        // for traversal within a node as well; the ordering of operations is chosen such that we
        // can exit the traversal as soon as possible or avoid it entirely.
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let entry = unsafe { btree_lower_bound(self.list.get(), node_offset, cursor.get()) };

        // The slot that will eventually hold offset.
        let mut slot: *mut VmPageOrMarker = core::ptr::null_mut();

        // If offset falls in an interval, this will hold the dirty state of the interval that
        // offset is found in. It will be used to mint new sentinel values if we were also asked to
        // split the interval. Only zero intervals are supported, so the dirty state is all that is
        // needed to mint a sentinel and it is preserved across the split; if other interval types
        // are added in the future, this will need to carry the full interval type as well.
        let mut found_interval_dirty_state = ZeroRangeDirtyState::Untracked;
        let mut is_in_interval = false;

        // For the offset to lie in an interval, it is going to have an interval end so we should
        // have found some valid node if the offset falls in an interval. If we could not find a
        // valid node, we know that offset cannot lie in an interval, so skip the check.
        if !entry.node.is_null() {
            let node_ptr = entry.node.cast::<VmPageListNode>();
            if entry.offset == node_offset {
                // We found the node containing offset. Get the slot.
                // SAFETY: `node_ptr` is non-null and points to a live node owned by `self.list`.
                slot = unsafe { VmPageListNode::slot_ptr(node_ptr, node_index) };
                // Short circuit the is_offset_in_interval_helper call below if the slot itself is
                // an interval sentinel. This is purely an optimization, and it would be okay to
                // call is_offset_in_interval_helper for this case too.
                // SAFETY: `slot` is non-null and valid.
                if unsafe { (*slot).is_interval() } {
                    is_in_interval = true;
                    found_interval_dirty_state = unsafe { (*slot).zero_interval_dirty_state() };
                }
            }

            if !is_in_interval {
                // SAFETY: `entry.node` is non-null and points to a valid `VmPageListNode`.
                let node = unsafe { entry.node.cast::<VmPageListNode>().as_ref_unchecked() };
                if let Some(interval) =
                    Self::is_offset_in_interval_helper(offset, entry.offset, node)
                {
                    is_in_interval = true;
                    // If we found an interval, we should have found a valid interval sentinel too.
                    debug_assert!(interval.is_interval());
                    found_interval_dirty_state = interval.zero_interval_dirty_state();
                }
            }

            // If we are in an interval but cannot split it, we cannot return a slot. The caller
            // should not be able to manipulate the slot freely without correctly handling the
            // interval(s) around it.
            if is_in_interval && !split_interval {
                return (None, true);
            }
        }

        // We won't have a valid slot if the node we looked up did not contain the required offset.
        if slot.is_null() {
            // Allocate the node that would contain offset and then get the slot.
            // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
            let node_ptr = unsafe {
                bindings::cpp_vm_page_list_btree_insert(self.list.get(), node_offset, cursor.get())
            };
            if node_ptr.is_null() {
                return (None, is_in_interval);
            }
            let node_ptr = node_ptr.cast::<VmPageListNode>();
            // SAFETY: `node_ptr` was just returned non-null by `cpp_vm_page_list_btree_insert`.
            slot = unsafe { VmPageListNode::slot_ptr(node_ptr, node_index) };
        }

        // If offset does not lie in an interval, or if the slot is already a single page interval,
        // there is nothing more to be done. Return the slot.
        // SAFETY: `slot` is non-null and valid.
        if !is_in_interval || unsafe { (*slot).is_interval_slot() } {
            // We currently only support zero intervals.
            debug_assert!(!is_in_interval || unsafe { (*slot).is_interval_zero() });
            // SAFETY: `slot` is non-null and valid for the lifetime of `&mut self`.
            return (Some(unsafe { &mut *slot }), is_in_interval);
        }

        // If we reached here, we know that we are in an interval and we need to split it in order
        // to return the required slot.
        debug_assert!(is_in_interval && split_interval);

        // Depending on whether the slot is empty or not, we might need to insert a new interval
        // start, a new interval end, or both. Figure out which slots are needed first.
        //  - If the slot is empty, we need to insert an end to the left, a start to the right and a
        //    single page interval slot at offset. So we need both a new start and a new end.
        //  - If the slot is populated, since we know that offset falls in an interval, it could
        //    only be an interval start or an interval end. We will either need a new start or a new
        //    end but not both.
        let mut need_new_end = true;
        let mut need_new_start = true;
        // SAFETY (both reads of `slot` below): `slot` was set non-null above and points into a live
        // node owned by `self.list`.
        if unsafe { (*slot).is_interval_start() } {
            // We can move the interval start to the right, and replace the old start with a slot.
            // Don't need a new end.
            need_new_end = false;
        } else if unsafe { (*slot).is_interval_end() } {
            // We can move the interval end to the left, and replace the old end with a slot. Don't
            // need a new start.
            need_new_start = false;
        }

        // Now find the previous and next slots as needed for the new end and new start
        // respectively.
        // Note that the node allocations below are mutually exclusive. If we allocate the previous
        // node, we won't need to allocate the next node and vice versa, since the allocation logic
        // depends on the value of node_index. So if the required node allocation fails, all the
        // cleanup that's required is returning the slot at offset if it is empty, as we might have
        // allocated a new node previously to hold offset.
        let mut new_end: *mut VmPageOrMarker = core::ptr::null_mut();
        if need_new_end {
            if node_index > 0 {
                // The previous slot is in the same node.
                // SAFETY: `slot` is non-null and node_index > 0, so slot.sub(1) points to a valid
                // slot in the same node.
                new_end = unsafe { slot.sub(1) };
            } else {
                // The previous slot is in the node to the left. We might need to allocate a new
                // node to the left if it does not exist. Try to walk left and see if we find the
                // previous node we're looking for.
                let prev_node_offset = node_offset - VmPageListNode::NODE_SPAN_BYTES;
                // SAFETY: `cursor.get()` is a valid pointer pointing to current node.
                let prev_entry = unsafe { btree_cursor_prev(cursor.get()) };
                // We are here because slot was either an empty slot inside an interval or it was an
                // interval end. Additionally, the slot was the left-most slot in its node, which
                // means we are guaranteed to find a node to the left which holds the start of the
                // interval.
                debug_assert!(!prev_entry.node.is_null());
                if prev_entry.offset == prev_node_offset {
                    let prev_node = prev_entry.node.cast::<VmPageListNode>();
                    // SAFETY: `prev_node` is non-null and points to a live node owned by
                    // `self.list`.
                    new_end = unsafe {
                        VmPageListNode::slot_ptr(prev_node, VmPageListNode::PAGE_FAN_OUT - 1)
                    };
                } else {
                    debug_assert!(prev_entry.offset < prev_node_offset);
                    // SAFETY: `self.list.get()` is a valid pointer. Passing null cursor inserts
                    // without a hint.
                    let node_ptr = unsafe {
                        bindings::cpp_vm_page_list_btree_insert(
                            self.list.get(),
                            prev_node_offset,
                            core::ptr::null_mut(),
                        )
                    };
                    if node_ptr.is_null() {
                        // SAFETY: `slot` is non-null.
                        if unsafe { (*slot).is_empty() } {
                            self.return_empty_slot(offset);
                        }
                        return (None, true);
                    }
                    let node_ptr = node_ptr.cast::<VmPageListNode>();
                    // SAFETY: `node_ptr` was just returned non-null by
                    // `cpp_vm_page_list_btree_insert`.
                    new_end = unsafe {
                        VmPageListNode::slot_ptr(node_ptr, VmPageListNode::PAGE_FAN_OUT - 1)
                    };
                }
            }
            debug_assert!(!new_end.is_null());
        }

        let mut new_start: *mut VmPageOrMarker = core::ptr::null_mut();
        if need_new_start {
            if node_index < VmPageListNode::PAGE_FAN_OUT - 1 {
                // The next slot is in the same node.
                // SAFETY: `slot` is non-null and node_index < PAGE_FAN_OUT - 1, so slot.add(1)
                // points to a valid slot in the same node.
                new_start = unsafe { slot.add(1) };
            } else {
                // The next slot is in the node to the right. We might need to allocate a new node
                // to the right if it does not exist. Try to walk right and see if we find the next
                // node we're looking for.
                let next_node_offset = node_offset + VmPageListNode::NODE_SPAN_BYTES;
                // SAFETY: `cursor.get()` is a valid pointer pointing to current node.
                let _ = unsafe { btree_cursor_next(cursor.get()) };
                let next_entry = unsafe { btree_cursor_get(cursor.get()) };
                // We are here because slot was either empty or it was an interval start.
                // Additionally, the slot was the right-most slot in its node, which means we are
                // guaranteed to find a node to the right which holds the end of the interval.
                debug_assert!(!next_entry.node.is_null());
                if next_entry.offset == next_node_offset {
                    let next_node = next_entry.node.cast::<VmPageListNode>();
                    // SAFETY: `next_node` is non-null and points to a live node owned by
                    // `self.list`.
                    new_start = unsafe { VmPageListNode::slot_ptr(next_node, 0) };
                } else {
                    debug_assert!(next_entry.offset > next_node_offset);
                    // SAFETY: `self.list.get()` is a valid pointer. Passing null cursor inserts
                    // without a hint.
                    let node_ptr = unsafe {
                        bindings::cpp_vm_page_list_btree_insert(
                            self.list.get(),
                            next_node_offset,
                            core::ptr::null_mut(),
                        )
                    };
                    if node_ptr.is_null() {
                        // SAFETY: `slot` is non-null.
                        if unsafe { (*slot).is_empty() } {
                            self.return_empty_slot(offset);
                        }
                        return (None, true);
                    }
                    let node_ptr = node_ptr.cast::<VmPageListNode>();
                    // SAFETY: `node_ptr` was just returned non-null by
                    // `cpp_vm_page_list_btree_insert`.
                    new_start = unsafe { VmPageListNode::slot_ptr(node_ptr, 0) };
                }
            }
            debug_assert!(!new_start.is_null());
        }

        // Now that we've looked up the relevant slots after performing any required allocations,
        // make the actual change. Install new end and start sentinels on the left and right of
        // offset respectively.
        if !new_start.is_null() {
            // SAFETY: `new_start` is non-null and valid.
            let new_start_ref = unsafe { &mut *new_start };
            if new_start_ref.is_interval_end() {
                // If an interval was ending at the next slot, change it into a Slot sentinel.
                new_start_ref.change_interval_sentinel(SentinelType::Slot);
            } else {
                debug_assert!(new_start_ref.is_empty());
                *new_start_ref =
                    VmPageOrMarker::zero_interval(SentinelType::Start, found_interval_dirty_state);
            }
        }
        if !new_end.is_null() {
            // SAFETY: `new_end` is non-null and valid.
            let end_slot_ref = unsafe { &mut *new_end };
            if end_slot_ref.is_interval_start() {
                // If an interval was starting at the previous slot, change it into a Slot sentinel.
                end_slot_ref.change_interval_sentinel(SentinelType::Slot);
            } else {
                debug_assert!(end_slot_ref.is_empty());
                *end_slot_ref =
                    VmPageOrMarker::zero_interval(SentinelType::End, found_interval_dirty_state);
            }
        }

        // Finally, install a slot sentinel at offset.
        // SAFETY: `slot` is non-null and valid for the lifetime of `&mut self`.
        let slot_ref = unsafe { &mut *slot };
        if slot_ref.is_empty() {
            *slot_ref =
                VmPageOrMarker::zero_interval(SentinelType::Slot, found_interval_dirty_state);
        } else {
            debug_assert!(slot_ref.is_interval_start() || slot_ref.is_interval_end());
            // If we're overwriting the start or end sentinel, carry over any relevant state
            // information to the rest of the interval that remains (if required).
            //
            // For zero intervals, this means preserving any non-zero awaiting_clean_length in the
            // start sentinel. We only need to do this if the zero interval is being split at the
            // start. This is an optimization to avoid having to potentially walk to another node
            // to find the relevant start to update. So the awaiting_clean_length can be larger
            // than the length of the resultant interval; the caller will take that into account
            // and carry over larger awaiting_clean_lengths across multiple intervals if they
            // exist. (See related comment in VmCowPages::WritebackEndLocked.)
            if slot_ref.is_interval_start() {
                let awaiting_clean_len = slot_ref.zero_interval_awaiting_clean_length();
                if awaiting_clean_len > page::SIZE as u64 {
                    debug_assert!(!new_start.is_null());
                    // SAFETY: `new_start` is non-null when `slot_ref.is_interval_start()`.
                    let new_start_ref = unsafe { &mut *new_start };
                    new_start_ref.set_zero_interval_awaiting_clean_length(
                        awaiting_clean_len - page::SIZE as u64,
                    );
                    slot_ref.set_zero_interval_awaiting_clean_length(page::SIZE as u64);
                }
            }
            slot_ref.change_interval_sentinel(SentinelType::Slot);
        }

        (Some(slot_ref), true)
    }

    /// Returns a slot that was empty after `lookup_or_allocate`, and that the caller did not end up
    /// filling.
    /// This ensures that if `lookup_or_allocate` allocated a new underlying list node, then that
    /// list node needs to be free'd otherwise it might not get cleaned up for the lifetime of the
    /// page list.
    ///
    /// This is only correct to call on an offset for which `lookup_or_allocate` had just returned a
    /// non-null slot, and that slot was Empty and is still Empty.
    pub fn return_empty_slot(&mut self, offset: u64) {
        let node_offset = VmPageListNode::node_offset(offset);
        let index = VmPageListNode::node_index(offset);

        ltracef_level!(
            2,
            "{:p} offset {:#x} node_offset {:#x} index {}\n",
            core::ptr::from_ref(self),
            offset,
            node_offset,
            index
        );

        let cursor = Opaque::uninit();
        // lookup the tree node that holds this offset
        // SAFETY: `self.list.get()` is a valid initialized VmPageListBtree pointer, `cursor.get()`
        // is uninitialized storage.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_find(self.list.get(), node_offset, cursor.get())
        };
        debug_assert!(!node_ptr.is_null());

        let node_ptr = node_ptr.cast::<VmPageListNode>();
        // SAFETY: `node_ptr` is verified non-null by debug_assert and function contract. Only
        // slot-local and shared references to the node are created, so slot pointers the caller
        // holds into this node remain valid across this call.
        // check that the slot was empty
        debug_assert!(unsafe { (*VmPageListNode::slot_ptr(node_ptr, index)).is_empty() });
        if unsafe { (*node_ptr).is_empty() } {
            // node is empty, erase it.
            // SAFETY: `self.list.get()` is valid and `cursor` was initialized by `btree_find`.
            unsafe {
                bindings::cpp_vm_page_list_btree_erase_at(self.list.get(), cursor.get());
            }
        }
    }

    /// Removes any item at `offset` from the list and returns it, or `VmPageOrMarker::empty()` if
    /// none.
    pub fn remove_content(&mut self, offset: u64) -> VmPageOrMarker {
        let node_offset = VmPageListNode::node_offset(offset);
        let index = VmPageListNode::node_index(offset);

        ltracef_level!(
            2,
            "{:p} offset {:#x} node_offset {:#x} index {}\n",
            core::ptr::from_ref(self),
            offset,
            node_offset,
            index
        );

        let cursor = Opaque::uninit();
        // lookup the tree node that holds this page
        // SAFETY: `self.list.get()` is a valid initialized VmPageListBtree pointer, `cursor.get()`
        // is uninitialized storage.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_find(self.list.get(), node_offset, cursor.get())
        };
        if node_ptr.is_null() {
            return VmPageOrMarker::empty();
        }

        let node_ptr = node_ptr.cast::<VmPageListNode>();
        // SAFETY: `node_ptr` is verified non-null and points to a valid `VmPageListNode`. Only
        // slot-local and shared references to the node are created, so slot pointers the caller
        // holds into this node remain valid across this call.
        // free this page
        let page = unsafe { &mut *VmPageListNode::slot_ptr(node_ptr, index) }
            .swap(VmPageOrMarker::empty());
        if !page.is_empty() && unsafe { (*node_ptr).is_empty() } {
            // if it was the last item in the node, remove the node from the tree
            ltracef_level!(2, "{:p} freeing the list node\n", core::ptr::from_ref(self));
            // SAFETY: `self.list.get()` is valid and `cursor` was initialized by `btree_find`.
            unsafe {
                bindings::cpp_vm_page_list_btree_erase_at(self.list.get(), cursor.get());
            }
        }
        page
    }

    /// Internal helper used when checking whether the offset falls in an interval.
    ///
    /// Determines whether `offset` falls in an interval involving `node` (which was returned by a
    /// lower-bound lookup). Returns any interval sentinel found, or `None`.
    fn is_offset_in_interval_helper(
        offset: u64,
        node_offset: u64,
        node: &VmPageListNode,
    ) -> Option<&VmPageOrMarker> {
        if offset < node_offset {
            node.node_starts_in_interval()
        } else {
            node.is_offset_in_interval(node_offset, offset)
        }
    }

    /// Internal helper to look up a node via `lower_bound` and check if `offset` falls in an
    /// interval. Returns any interval sentinel found, or `None`.
    fn find_interval_at_offset(&self, offset: u64) -> Option<&VmPageOrMarker> {
        // Find the node that would contain this offset.
        let node_offset = VmPageListNode::node_offset(offset);
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // If the node containing offset is populated, the lower bound will return that node. If not
        // populated, it will return the next populated node.
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let entry = unsafe { btree_lower_bound(self.list.get(), node_offset, cursor.get()) };
        // Could not find a valid node >= node_offset. So offset cannot be part of an interval, an
        // interval would have an end slot.
        if entry.node.is_null() {
            return None;
        }
        // The page list shouldn't have any empty nodes.
        // SAFETY: `entry.node` is non-null and valid for lifetime of `&self`.
        let node = unsafe { entry.node.cast::<VmPageListNode>().as_ref_unchecked() };
        debug_assert!(!node.is_empty());
        // Check if offset is in an interval also querying the associated sentinel.
        let interval = Self::is_offset_in_interval_helper(offset, entry.offset, node);
        debug_assert!(interval.is_none_or(|p| p.is_interval()));
        interval
    }

    /// Returns true if the specified offset falls in a sparse zero interval.
    pub fn is_offset_in_zero_interval(&self, offset: u64) -> bool {
        self.find_interval_at_offset(offset).is_some_and(|p| p.is_interval_zero())
    }

    /// Returns true if the specified offset falls in a sparse page interval.
    fn is_offset_in_interval(&self, offset: u64) -> bool {
        self.find_interval_at_offset(offset).is_some()
    }

    /// Internal helper to return the start of an interval given the end along with the
    /// corresponding offset.
    fn find_interval_start_for_end(&self, end_offset: u64) -> (&VmPageOrMarker, u64) {
        // Find the node that would contain the end offset.
        let node_offset = VmPageListNode::node_offset(end_offset);
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_find(self.list.get(), node_offset, cursor.get())
        };
        debug_assert!(!node_ptr.is_null());
        // SAFETY: `node_ptr` is non-null and points to a valid `VmPageListNode`.
        let node = unsafe { node_ptr.cast::<VmPageListNode>().as_ref_unchecked() };
        let node_index = VmPageListNode::node_index(end_offset);
        debug_assert!(node.lookup(node_index).is_interval_end());

        // The only populated slots in an interval are the start and the end. So the interval start
        // will either be in the same node as the interval end, or the previous populated node to
        // the left.
        for idx in (0..node_index).rev() {
            let slot = node.lookup(idx);
            if !slot.is_empty() {
                debug_assert!(slot.is_interval_start());
                return (slot, node_offset + (idx as u64) * (page::SIZE as u64));
            }
        }

        // We could not find the start in the same node. Check the previous one.
        // SAFETY: `cursor.get()` is initialized.
        let prev_entry = unsafe { btree_cursor_prev(cursor.get()) };
        debug_assert!(!prev_entry.node.is_null());
        // SAFETY: `prev_entry.node` is non-null and points to a valid `VmPageListNode`.
        let prev_node = unsafe { prev_entry.node.cast::<VmPageListNode>().as_ref_unchecked() };
        for idx in (0..VmPageListNode::PAGE_FAN_OUT).rev() {
            let slot = prev_node.lookup(idx);
            if !slot.is_empty() {
                debug_assert!(slot.is_interval_start());
                return (slot, prev_entry.offset + (idx as u64) * (page::SIZE as u64));
            }
        }

        // Should not reach here.
        unreachable!("sparse page interval is missing start sentinel");
    }

    /// Internal helper to return the end of an interval given the start along with the
    /// corresponding offset.
    fn find_interval_end_for_start(&self, start_offset: u64) -> (&VmPageOrMarker, u64) {
        // Find the node that would contain the start offset.
        let node_offset = VmPageListNode::node_offset(start_offset);
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_find(self.list.get(), node_offset, cursor.get())
        };
        debug_assert!(!node_ptr.is_null());
        // SAFETY: `node_ptr` is non-null and points to a valid `VmPageListNode`.
        let node = unsafe { node_ptr.cast::<VmPageListNode>().as_ref_unchecked() };
        let node_index = VmPageListNode::node_index(start_offset);
        debug_assert!(node.lookup(node_index).is_interval_start());

        // The only populated slots in an interval are the start and the end. So the interval end
        // will either be in the same node as the interval start, or the next populated node to the
        // right.
        for idx in (node_index + 1)..VmPageListNode::PAGE_FAN_OUT {
            let slot = node.lookup(idx);
            if !slot.is_empty() {
                debug_assert!(slot.is_interval_end());
                return (slot, node_offset + (idx as u64) * (page::SIZE as u64));
            }
        }

        // We could not find the end in the same node. Check the next one.
        // SAFETY: `cursor.get()` is initialized. Advance past current node and get next entry.
        let _ = unsafe { btree_cursor_next(cursor.get()) };
        let next_entry = unsafe { btree_cursor_get(cursor.get()) };
        debug_assert!(!next_entry.node.is_null());
        // SAFETY: `next_entry.node` is non-null and points to a valid `VmPageListNode`.
        let next_node = unsafe { next_entry.node.cast::<VmPageListNode>().as_ref_unchecked() };
        for idx in 0..VmPageListNode::PAGE_FAN_OUT {
            let slot = next_node.lookup(idx);
            if !slot.is_empty() {
                debug_assert!(slot.is_interval_end());
                return (slot, next_entry.offset + (idx as u64) * (page::SIZE as u64));
            }
        }

        // Should not reach here.
        unreachable!("sparse page interval is missing end sentinel");
    }

    /// Add a sparse zero interval spanning the range [start_offset, end_offset) with the specified
    /// dirty_state. The specified range must be previously unpopulated. This will try to merge the
    /// new zero interval with existing intervals to the left and/or right, if the dirty_state
    /// allows it.
    pub fn add_zero_interval(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        dirty_state: ZeroRangeDirtyState,
    ) -> Result<(), Status> {
        self.add_zero_interval_internal(start_offset, end_offset, dirty_state, 0, false)
    }

    /// Helper to look up a slot at an offset and return a mutable VmPageOrMarker pointer. Only
    /// finds an existing slot and does not perform any allocations.
    fn lookup_slot(&mut self, offset: u64) -> *mut VmPageOrMarker {
        let node_offset = VmPageListNode::node_offset(offset);
        let index = VmPageListNode::node_index(offset);
        // SAFETY: `self.list.get()` is a valid pointer. Passing null cursor finds without
        // populating an out_cursor.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_find(
                self.list.get(),
                node_offset,
                core::ptr::null_mut(),
            )
        };
        if node_ptr.is_null() {
            core::ptr::null_mut()
        } else {
            let node_ptr = node_ptr.cast::<VmPageListNode>();
            // SAFETY: `node_ptr` is verified non-null and points to a valid `VmPageListNode`.
            unsafe { VmPageListNode::slot_ptr(node_ptr, index) }
        }
    }

    /// Internal helper for AddZeroInterval.
    /// |replace_existing_slot| can optionally be set to true if a zero interval spanning a single
    /// page is being added, and the slot at that offset is already populated (but Empty) and can be
    /// reused.
    fn add_zero_interval_internal(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        dirty_state: ZeroRangeDirtyState,
        awaiting_clean_len: u64,
        replace_existing_slot: bool,
    ) -> Result<(), Status> {
        debug_assert!(start_offset.is_multiple_of(page::SIZE as u64));
        debug_assert!(end_offset.is_multiple_of(page::SIZE as u64));
        debug_assert!(start_offset < end_offset);
        debug_assert!(!replace_existing_slot || end_offset == start_offset + page::SIZE as u64);
        // If replace_existing_slot is true, then we might have the slot in an empty node, which is
        // not expected by any kind of page list traversal. So we cannot safely walk the specified
        // range. Instead, we will assert later that the slot being replaced is indeed empty. If we
        // don't end up using the slot, we will return the empty node.
        debug_assert!(
            replace_existing_slot
                || !self.any_pages_or_intervals_in_range(start_offset, end_offset)
        );
        debug_assert!(awaiting_clean_len == 0 || dirty_state == ZeroRangeDirtyState::Dirty);

        let page_size = page::SIZE as u64;
        let interval_start = start_offset;
        let interval_end = end_offset - page_size;
        let prev_offset = if interval_start > 0 { interval_start - page_size } else { 0 };
        let next_offset = interval_end + page_size;

        // Check if we can merge this zero interval with a preceding one.
        let mut merge_with_prev = false;
        let mut prev_slot: *mut VmPageOrMarker = core::ptr::null_mut();
        // The final start slot and offset after the merge. Used for awaiting_clean_length updates.
        let mut final_start: *mut VmPageOrMarker = core::ptr::null_mut();
        let mut final_start_offset: u64 = 0;
        if interval_start > 0 {
            prev_slot = self.lookup_slot(prev_offset);
            // We can merge to the left if we find a zero interval end or slot, and the dirty state
            // matches.
            // SAFETY: `prev_slot` if non-null points to a valid slot in a node in `self.list`.
            if !prev_slot.is_null()
                && unsafe { (*prev_slot).is_interval_zero() }
                && unsafe { (*prev_slot).is_interval_end() || (*prev_slot).is_interval_slot() }
                && unsafe { (*prev_slot).zero_interval_dirty_state() == dirty_state }
            {
                merge_with_prev = true;

                // Later we will also try to merge the new awaiting_clean_len into the interval
                // with which we're merging on the left. So stash the start sentinel for that
                // update later. We need to compute this before we start making changes to the
                // page list.
                if awaiting_clean_len > 0 {
                    if unsafe { (*prev_slot).is_interval_slot() } {
                        final_start = prev_slot;
                        final_start_offset = prev_offset;
                    } else {
                        let (_, off) = self.find_interval_start_for_end(prev_offset);
                        final_start_offset = off;
                        // This redundant lookup is so we can get a mutable reference to update
                        // the awaiting_clean_length. It is fine to be inefficient here as this
                        // case (i.e. awaiting_clean_len > 0) is unlikely.
                        final_start = self.lookup_slot(final_start_offset);
                    }
                }
            }
        }

        // Check if we can merge this zero interval with a following one.
        let mut merge_with_next = false;
        let next_slot = self.lookup_slot(next_offset);
        // We can merge to the right if we find a zero interval start or slot, and the dirty
        // state matches.
        // SAFETY: `next_slot` if non-null points to a valid slot in a node in `self.list`.
        if !next_slot.is_null()
            && unsafe { (*next_slot).is_interval_zero() }
            && unsafe { (*next_slot).is_interval_start() || (*next_slot).is_interval_slot() }
            && unsafe { (*next_slot).zero_interval_dirty_state() == dirty_state }
        {
            merge_with_next = true;
        }

        // First allocate any slots that might be needed to insert the interval.
        let mut new_start: *mut VmPageOrMarker = core::ptr::null_mut();
        let mut new_end: *mut VmPageOrMarker = core::ptr::null_mut();
        // If we could not merge with an interval to the left, we're going to need a new start
        // sentinel.
        if !merge_with_prev {
            new_start = self.lookup_or_allocate_internal(interval_start);
            if new_start.is_null() {
                debug_assert!(!replace_existing_slot);
                return Err(Status::NO_MEMORY);
            }
            // SAFETY: `new_start` is non-null.
            debug_assert!(unsafe { (*new_start).is_empty() });
        }
        // If we could not merge with an interval to the right, we're going to need a new end
        // sentinel.
        if !merge_with_next {
            new_end = self.lookup_or_allocate_internal(interval_end);
            if new_end.is_null() {
                debug_assert!(!replace_existing_slot);
                // Clean up any slot we allocated for new_start before returning.
                if !new_start.is_null() {
                    // SAFETY: `new_start` is non-null and empty.
                    debug_assert!(unsafe { (*new_start).is_empty() });
                    self.return_empty_slot(interval_start);
                }
                return Err(Status::NO_MEMORY);
            }
            // SAFETY: `new_end` is non-null.
            debug_assert!(unsafe { (*new_end).is_empty() });
        }
        // If we were replacing an existing slot, but are able to merge both to the left and the
        // right, we won't need the slot anymore. So return it. Note that this is not strictly
        // needed but we want to be explicit for clarity. We know that the existing slot is in the
        // same node as the previous or the next slot (or both). We will either end up freeing one
        // or both of those slots, or retaining one or both of them. So the node the existing slot
        // shares with those slots will either be freed, or won't need freeing.
        if replace_existing_slot && merge_with_prev && merge_with_next {
            self.return_empty_slot(interval_start);
        }

        // Now that we've checked for all error conditions perform the actual update.
        if merge_with_prev {
            // Try to merge the new awaiting_clean_len into the previous interval.
            if awaiting_clean_len > 0 {
                // SAFETY: `final_start` is non-null when `awaiting_clean_len > 0` and
                // `merge_with_prev`.
                let final_start_ref = unsafe { &mut *final_start };
                let old_len = final_start_ref.zero_interval_awaiting_clean_length();
                // Can only merge the new awaiting_clean_len if there is no gap between the range
                // described by final_start's awaiting_clean_length and the start of the new
                // interval we're trying to add.
                if final_start_offset + old_len >= interval_start {
                    final_start_ref.set_zero_interval_awaiting_clean_length(
                        core::cmp::max(
                            final_start_offset + old_len,
                            interval_start + awaiting_clean_len,
                        ) - final_start_offset,
                    );
                }
            }
            // SAFETY: `prev_slot` is non-null when `merge_with_prev`.
            let prev_slot_ref = unsafe { &mut *prev_slot };
            if prev_slot_ref.is_interval_end() {
                // If the prev_slot was an interval end, we can simply extend that interval to
                // include the new interval. Free up the old interval end.
                *prev_slot_ref = VmPageOrMarker::empty();
            } else {
                // If the prev_slot was interval slot, we can extend the interval in that case too.
                // Change the old interval slot into an interval start.
                debug_assert!(prev_slot_ref.is_interval_slot());
                debug_assert!(prev_slot_ref.zero_interval_dirty_state() == dirty_state);
                prev_slot_ref.change_interval_sentinel(SentinelType::Start);
            }
        } else {
            // We could not merge with an interval to the left. Start a new interval.
            // SAFETY: `new_start` is non-null when `!merge_with_prev`.
            let new_start_ref = unsafe { &mut *new_start };
            debug_assert!(new_start_ref.is_empty());
            *new_start_ref = VmPageOrMarker::zero_interval(SentinelType::Start, dirty_state);
            if awaiting_clean_len > 0 {
                final_start = new_start;
                final_start_offset = interval_start;
                new_start_ref.set_zero_interval_awaiting_clean_length(awaiting_clean_len);
            }
        }

        if merge_with_next {
            // First see if we can merge the awaiting_clean_len of the interval we're merging with
            // the interval we have constructed so far on the left.
            // Note that it might still be possible to merge the awaiting_clean_len's of the left
            // and right intervals even if the specified awaiting_clean_len is 0, due to the
            // interval being inserted in the middle bridging the gap. We choose to skip that case
            // however to keep things more efficient; we don't want to needlessly lookup the
            // final_start unless we're also updating awaiting_clean_len for the interval being
            // added. Instead we choose to lose the awaiting_clean_len for the interval on the
            // right - this is also consistent with not being able to retain the awaiting_clean_len
            // if an interval is simply extended on the left.
            if awaiting_clean_len > 0 {
                // SAFETY: `next_slot` and `final_start` are non-null when `awaiting_clean_len > 0`.
                let next_slot_ref = unsafe { &mut *next_slot };
                let final_start_ref = unsafe { &mut *final_start };
                let len = next_slot_ref.zero_interval_awaiting_clean_length();
                let old_len = final_start_ref.zero_interval_awaiting_clean_length();
                // Can only merge the new awaiting_clean_len if there is no gap between the range
                // described by final_start's awaiting_clean_length and the start of the next
                // interval.
                if len > 0 && final_start_offset + old_len >= next_offset {
                    final_start_ref.set_zero_interval_awaiting_clean_length(
                        core::cmp::max(final_start_offset + old_len, next_offset + len)
                            - final_start_offset,
                    );
                }
            }

            // SAFETY: `next_slot` is non-null when `merge_with_next`.
            let next_slot_ref = unsafe { &mut *next_slot };
            if next_slot_ref.is_interval_start() {
                // If the next_slot was an interval start, we can move back the start to include the
                // new interval. Free up the old start.
                *next_slot_ref = VmPageOrMarker::empty();
            } else {
                // If the next_slot was an interval slot, we can move back the start in that case
                // too. Change the old interval slot into an interval end.
                debug_assert!(next_slot_ref.is_interval_slot());
                debug_assert!(next_slot_ref.zero_interval_dirty_state() == dirty_state);
                next_slot_ref.set_zero_interval_awaiting_clean_length(0);
                next_slot_ref.change_interval_sentinel(SentinelType::End);
            }
        } else {
            // We could not merge with an interval to the right. Install an interval end sentinel.
            // If the new zero interval spans a single page, we will already have installed a start
            // above, so change it to a slot sentinel.
            // SAFETY: `new_end` is non-null when `!merge_with_next`.
            let end_slot_ref = unsafe { &mut *new_end };
            if end_slot_ref.is_interval_start() {
                debug_assert!(end_slot_ref.zero_interval_dirty_state() == dirty_state);
                end_slot_ref.change_interval_sentinel(SentinelType::Slot);
            } else {
                debug_assert!(end_slot_ref.is_empty());
                *end_slot_ref = VmPageOrMarker::zero_interval(SentinelType::End, dirty_state);
            }
        }

        // If we ended up removing the prev_slot or next_slot, return the now empty slots.
        // SAFETY: `prev_slot` and `next_slot` are non-null when their respective merge is true.
        let return_prev_slot = merge_with_prev && unsafe { (*prev_slot).is_empty() };
        let mut return_next_slot = merge_with_next && unsafe { (*next_slot).is_empty() };
        if return_prev_slot {
            self.return_empty_slot(prev_offset);
            // next_slot and prev_slot could have come from the same node, in which case we've
            // already freed up the node containing next_slot when returning prev_slot.
            if return_next_slot
                && VmPageListNode::node_offset(prev_offset)
                    == VmPageListNode::node_offset(next_offset)
            {
                return_next_slot = false;
            }
        }
        if return_next_slot {
            debug_assert!(
                !return_prev_slot
                    || VmPageListNode::node_offset(prev_offset)
                        != VmPageListNode::node_offset(next_offset)
            );
            self.return_empty_slot(next_offset);
        }

        Ok(())
    }

    /// Populates individual interval slots in the range [start_offset, end_offset) that falls
    /// inside a sparse interval. The intent of this function is to allow the caller to prepare the
    /// range for overwriting (replacing with pages) by populating the required slots upfront, so
    /// that slot lookup does not fail after this call. Essentially simulates interval splits
    /// (lookup_or_allocate_check_for_interval) for every offset in the specified range, but does so
    /// more efficiently, instead of having to search the tree repeatedly for every single offset.
    pub fn populate_slots_in_interval(
        &mut self,
        start_offset: u64,
        end_offset: u64,
    ) -> Result<(), Status> {
        debug_assert!(start_offset.is_multiple_of(page::SIZE as u64));
        debug_assert!(end_offset.is_multiple_of(page::SIZE as u64));
        debug_assert!(end_offset > start_offset);
        let page_size = page::SIZE as u64;
        // Change the end_offset to an inclusive offset for convenience.
        let end_offset = end_offset - page_size;

        if cfg!(debug_assertions) {
            // The start_offset and end_offset should lie in an interval.
            debug_assert!(self.is_offset_in_interval(start_offset));
            debug_assert!(self.is_offset_in_interval(end_offset));
            // All the remaining offsets should be empty and lie in the same interval. So we should
            // find no pages or gaps in the range:
            // [start_offset + page_size, end_offset - page_size].
            if start_offset + page_size < end_offset {
                let status = self.for_every_page_and_gap_in_range(
                    start_offset + page_size,
                    end_offset,
                    |_, _| Status::BAD_STATE,
                    |_, _| Status::BAD_STATE,
                );
                debug_assert!(status.is_ok());
            }
        }

        // First allocate slots at start_offset and end_offset, splitting the interval around them
        // if required. If any of the subsequent operations fail, we should return these interval
        // slots. This function should either be able to populate all the slots requested, or the
        // interval should be returned to its original state before the call.
        let (start_slot, is_start_in_interval) =
            self.lookup_or_allocate_check_for_interval(start_offset, true);
        if start_slot.is_none() {
            return Err(Status::NO_MEMORY);
        }
        debug_assert!(is_start_in_interval);
        let start_dirty_state = {
            let start_slot_ref = start_slot.unwrap();
            debug_assert!(start_slot_ref.is_interval_slot());
            start_slot_ref.zero_interval_dirty_state()
        };
        // If only asked to populate single slot, nothing more to do.
        if start_offset == end_offset {
            return Ok(());
        }

        let (end_slot, is_end_in_interval) =
            self.lookup_or_allocate_check_for_interval(end_offset, true);
        if end_slot.is_none() {
            // Return the start slot before returning.
            self.return_interval_slot(start_offset);
            return Err(Status::NO_MEMORY);
        }
        debug_assert!(is_end_in_interval);
        {
            let end_slot_ref = end_slot.unwrap();
            debug_assert!(end_slot_ref.is_interval_slot());
            // We only support zero intervals and the start and end dirty state should match.
            debug_assert!(start_dirty_state == end_slot_ref.zero_interval_dirty_state());
        }

        // If there are no more empty slots to consider between start and end, return early.
        if end_offset == start_offset + page_size {
            return Ok(());
        }

        // Now we need to walk all page offsets from start_offset to end_offset and convert them all
        // to interval slots. Before we can do that, we will first allocate any page list nodes
        // required in the middle. After splitting the interval around start_offset, we know that
        // the node containing |start_offset + page_size| will be populated in order for it to hold
        // the interval start sentinel at that offset. Similarly, we know that the node containing
        // |end_offset - page_size| will be populated. So all the unpopulated nodes (if any) will
        // lie between these two nodes.
        let first_node_offset = VmPageListNode::node_offset(start_offset + page_size);
        let first_node_index = VmPageListNode::node_index(start_offset + page_size);
        let last_node_offset = VmPageListNode::node_offset(end_offset - page_size);
        let last_node_index = VmPageListNode::node_index(end_offset - page_size);
        debug_assert!(last_node_offset >= first_node_offset);
        if last_node_offset > first_node_offset + VmPageListNode::NODE_SPAN_BYTES {
            let first_unpopulated = first_node_offset + VmPageListNode::NODE_SPAN_BYTES;
            let last_unpopulated = last_node_offset - VmPageListNode::NODE_SPAN_BYTES;
            let mut node_off = first_unpopulated;
            while node_off <= last_unpopulated {
                // SAFETY: `self.list.get()` is a valid pointer. Passing null cursor inserts
                // without a hint.
                let node_ptr = unsafe {
                    bindings::cpp_vm_page_list_btree_insert(
                        self.list.get(),
                        node_off,
                        core::ptr::null_mut(),
                    )
                };
                if node_ptr.is_null() {
                    // If allocating a new node fails, clean up all the new nodes we might have
                    // installed until this point, which is all the empty nodes starting at
                    // first_unpopulated to before the node that failed.
                    let mut cleanup_off = first_unpopulated;
                    while cleanup_off < node_off {
                        let cleanup_cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
                        // SAFETY: `self.list.get()` and `cleanup_cursor.get()` are valid pointers.
                        let found = unsafe {
                            bindings::cpp_vm_page_list_btree_find(
                                self.list.get(),
                                cleanup_off,
                                cleanup_cursor.get(),
                            )
                        };
                        // Every node from first_unpopulated up to the one that failed was inserted
                        // above, so the find cannot fail.
                        debug_assert!(!found.is_null());
                        // SAFETY: `self.list.get()` is valid, and `cleanup_cursor` was initialized
                        // by the successful `btree_find` above.
                        unsafe {
                            bindings::cpp_vm_page_list_btree_erase_at(
                                self.list.get(),
                                cleanup_cursor.get(),
                            );
                        }
                        cleanup_off += VmPageListNode::NODE_SPAN_BYTES;
                    }
                    // Also return the start and end slots that we split above.
                    self.return_interval_slot(start_offset);
                    self.return_interval_slot(end_offset);
                    return Err(Status::NO_MEMORY);
                }
                node_off += VmPageListNode::NODE_SPAN_BYTES;
            }
        }

        // Now that all allocations have succeeded, we know that the rest of the operation cannot
        // fail. Walk all offsets after start_offset and before end_offset, overwriting all the
        // slots as interval slots.
        let mut node_offset = first_node_offset;
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let node_ptr = unsafe {
            bindings::cpp_vm_page_list_btree_find(self.list.get(), node_offset, cursor.get())
        };
        // This has to emulate calls to lookup_or_allocate_check_for_interval for all slots in the
        // range, which includes retaining awaiting_clean_length. After the start_slot split, the
        // slot following it might contain a non-zero awaiting_clean_length for the interval
        // following it, this needs to be "shifted" to the slot after the last one we populate,
        // adjusting for all the populated slots we encounter in the middle.
        //
        // For example, if we were populating 3 slots starting at the interval start, whose
        // awaiting_clean_length was 5 pages, the awaiting_clean_lengths for the 3 slots and the
        // remaining interval at the end of the call should be (in pages): [1, 1, 1, 2]
        // If awaiting_clean_length had initially been 2, we would instead have: [1, 1, 0, 0]
        let mut awaiting_clean_len = {
            // SAFETY: `node_ptr` is non-null and valid.
            let first_node = unsafe { node_ptr.cast::<VmPageListNode>().as_ref_unchecked() };
            first_node.lookup(first_node_index).zero_interval_awaiting_clean_length()
        };

        while node_offset <= last_node_offset {
            // SAFETY: `cursor.get()` is a valid pointer.
            let entry = unsafe { btree_cursor_get(cursor.get()) };
            debug_assert!(!entry.node.is_null());
            debug_assert_eq!(entry.offset, node_offset);
            // SAFETY: `entry.node` is non-null and valid.
            let node = unsafe { entry.node.cast::<VmPageListNode>().as_mut_unchecked() };
            let start_idx = if node_offset == first_node_offset { first_node_index } else { 0 };
            let end_idx = if node_offset == last_node_offset {
                last_node_index
            } else {
                VmPageListNode::PAGE_FAN_OUT - 1
            };
            for index in start_idx..=end_idx {
                let cur = node.lookup_mut(index);
                *cur = VmPageOrMarker::zero_interval(SentinelType::Slot, start_dirty_state);
                if awaiting_clean_len > 0 {
                    cur.set_zero_interval_awaiting_clean_length(page_size);
                    awaiting_clean_len -= page_size;
                }
            }
            // Advance cursor to next node.
            // SAFETY: `cursor.get()` is a valid pointer.
            let _ = unsafe { btree_cursor_next(cursor.get()) };
            node_offset += VmPageListNode::NODE_SPAN_BYTES;
        }

        if awaiting_clean_len > 0 {
            // Set awaiting_clean_length for the last populated slot too.
            self.lookup_mut(end_offset)
                .expect("end_offset was populated above")
                .set_zero_interval_awaiting_clean_length(page_size);
            awaiting_clean_len -= page_size;
            // If there is still a remaining awaiting_clean_length, carry it over to the interval
            // next to the last slot, if there is one.
            if awaiting_clean_len > 0
                && let Some(mut next_ref) = self.lookup_mut(end_offset + page_size)
                && (next_ref.is_interval_start() || next_ref.is_interval_slot())
            {
                let old_len = next_ref.zero_interval_awaiting_clean_length();
                next_ref.set_zero_interval_awaiting_clean_length(core::cmp::max(
                    old_len,
                    awaiting_clean_len,
                ));
            }
        }

        if cfg!(debug_assertions) {
            // All offsets in the range [start_offset, end_offset] should contain interval slots.
            let mut next_off = start_offset;
            let status =
                self.for_every_page_in_range(start_offset, end_offset + page_size, |p, off| {
                    if off != next_off || !p.is_interval_slot() {
                        return Status::BAD_STATE;
                    }
                    next_off += page_size;
                    Status::NEXT
                });
            debug_assert!(status.is_ok());
        }

        Ok(())
    }

    /// Helper to return an unused interval slot so that it can be merged back into the interval it
    /// was populated/split from.
    pub fn return_interval_slot(&mut self, offset: u64) {
        // We should be able to lookup a pre-existing interval slot.
        let slot = self.lookup_or_allocate_internal(offset);
        debug_assert!(!slot.is_null());
        // SAFETY: `slot` is non-null and points to a valid slot in `self.list`; `slot` is not used
        // after `add_zero_interval_internal` below.
        let slot = unsafe { &mut *slot };
        debug_assert!(slot.is_interval_slot());

        // We only support zero intervals for now. If more interval types are added in the future,
        // handle them here.
        debug_assert!(slot.is_interval_zero());
        let dirty_state = slot.zero_interval_dirty_state();
        let awaiting_clean_len = slot.zero_interval_awaiting_clean_length();
        // Temporarily empty the slot and then add a zero interval back in at the same spot using
        // add_zero_interval, which will ensure that the slot is merged to the left and/or right as
        // applicable. We don't need to return the empty slot here because we're asking
        // add_zero_interval_internal to reuse the existing slot.
        *slot = VmPageOrMarker::empty();
        let status = self.add_zero_interval_internal(
            offset,
            offset + page::SIZE as u64,
            dirty_state,
            awaiting_clean_len,
            true,
        );
        // We are reusing an existing slot, so we cannot fail with Status::NO_MEMORY.
        debug_assert!(status.is_ok());
    }

    /// Clips an interval from the start by `len`, i.e. moves the start from `interval_start` to
    /// `interval_start + len`. The total length of the interval must be larger than `len`.
    ///
    /// Any remaining awaiting clean length is carried over to the new start.
    pub fn clip_interval_start(&mut self, interval_start: u64, len: u64) -> Result<(), Status> {
        let page_size = page::SIZE as u64;
        debug_assert!(interval_start.is_multiple_of(page_size));
        debug_assert!(len.is_multiple_of(page_size));
        if len == 0 {
            return Ok(());
        }
        let new_interval_start =
            interval_start.checked_add(len).expect("clipped interval start overflowed");

        // Capture the state of the old start upfront, before we make any changes to the list.
        let (old_dirty_state, old_awaiting_clean_len) = {
            let old_start =
                self.lookup(interval_start).expect("interval start sentinel must be populated");
            debug_assert!(old_start.is_interval_start());
            // We only support zero intervals for now.
            debug_assert!(old_start.is_interval_zero());
            (old_start.zero_interval_dirty_state(), old_start.zero_interval_awaiting_clean_length())
        };

        if cfg!(debug_assertions) {
            // There should only be empty slots between the old and the new start.
            let status = self.for_every_page_and_gap_in_range(
                interval_start + page_size,
                new_interval_start,
                |_, _| Status::BAD_STATE,
                |_, _| Status::BAD_STATE,
            );
            debug_assert!(status.is_ok());
        }

        // SAFETY: `lookup_or_allocate_internal` returns either null or a valid slot in `self.list`;
        // `new_start` is not used after `remove_content` below.
        let Some(new_start) =
            (unsafe { self.lookup_or_allocate_internal(new_interval_start).as_mut() })
        else {
            return Err(Status::NO_MEMORY);
        };

        // It is possible that we are moving the start all the way to the end, leaving behind a
        // single interval slot.
        if new_start.is_interval_end() {
            new_start.change_interval_sentinel(SentinelType::Slot);
        } else {
            debug_assert!(new_start.is_empty());
            // We only support zero intervals for now.
            *new_start = VmPageOrMarker::zero_interval(SentinelType::Start, old_dirty_state);
        }

        // Now that the new start has been created, carry over any remaining awaiting_clean_length
        // from the old start.
        if old_awaiting_clean_len > len {
            new_start.set_zero_interval_awaiting_clean_length(old_awaiting_clean_len - len);
        }

        // Free up the old start.
        self.remove_content(interval_start);
        Ok(())
    }

    /// Clips an interval from the end by `len`, i.e. moves the end from `interval_end` to
    /// `interval_end - len`. The total length of the interval must be larger than `len`.
    pub fn clip_interval_end(&mut self, interval_end: u64, len: u64) -> Result<(), Status> {
        let page_size = page::SIZE as u64;
        debug_assert!(interval_end.is_multiple_of(page_size));
        debug_assert!(len.is_multiple_of(page_size));
        if len == 0 {
            return Ok(());
        }
        let new_interval_end =
            interval_end.checked_sub(len).expect("clipped interval end underflowed");

        // Capture the state of the old end upfront, before we make any changes to the list.
        let old_dirty_state = {
            let old_end =
                self.lookup(interval_end).expect("interval end sentinel must be populated");
            debug_assert!(old_end.is_interval_end());
            // We only support zero intervals for now.
            debug_assert!(old_end.is_interval_zero());
            old_end.zero_interval_dirty_state()
        };

        if cfg!(debug_assertions) {
            // There should only be empty slots between the new and the old end.
            let status = self.for_every_page_and_gap_in_range(
                new_interval_end + page_size,
                interval_end,
                |_, _| Status::BAD_STATE,
                |_, _| Status::BAD_STATE,
            );
            debug_assert!(status.is_ok());
        }

        // SAFETY: `lookup_or_allocate_internal` returns either null or a valid slot in `self.list`;
        // `new_end` is not used after `remove_content` below.
        let Some(new_end) =
            (unsafe { self.lookup_or_allocate_internal(new_interval_end).as_mut() })
        else {
            return Err(Status::NO_MEMORY);
        };

        // It is possible that we are moving the end all the way to the start, leaving behind a
        // single interval slot.
        if new_end.is_interval_start() {
            new_end.change_interval_sentinel(SentinelType::Slot);
        } else {
            debug_assert!(new_end.is_empty());
            // We only support zero intervals for now.
            *new_end = VmPageOrMarker::zero_interval(SentinelType::End, old_dirty_state);
        }

        // Free up the old end.
        self.remove_content(interval_end);
        Ok(())
    }

    /// Replace an existing page at `offset` with a zero interval, and return the released page.
    /// The caller takes ownership of the released page and is responsible for freeing it.
    pub fn replace_page_with_zero_interval(
        &mut self,
        offset: u64,
        dirty_state: ZeroRangeDirtyState,
    ) -> VmPagePtr {
        // We are guaranteed to find the slot as we're replacing an existing page.
        let slot = self.lookup_or_allocate_internal(offset);
        debug_assert!(!slot.is_null());
        // SAFETY: `slot` is non-null and points to a valid slot in `self.list`; `slot` is not used
        // after `add_zero_interval_internal` below.
        let slot = unsafe { &mut *slot };
        // Release the page at the offset, but hold on to the empty slot so it can be reused by
        // `add_zero_interval_internal`.
        let page = slot.release_page();
        let status = self.add_zero_interval_internal(
            offset,
            offset + page::SIZE as u64,
            dirty_state,
            0,
            true,
        );
        // The only error `add_zero_interval_internal` can encounter is Status::NO_MEMORY, but we
        // know that cannot happen because we are reusing an existing slot, so we don't need to
        // allocate a new node.
        debug_assert!(status.is_ok());
        // Return the page we released.
        page
    }

    /// Overwrite a zero interval either fully or partially with a new zero interval, breaking off
    /// the old interval into two if required. `old_start_offset` and `old_end_offset` specify the
    /// start and end sentinels of the old interval that is being overwritten; either one of these
    /// or both can be specified, with the other set to `None`. The new zero interval that
    /// overwrites the old spans `[new_start_offset, new_end_offset]` with its state set to
    /// `new_dirty_state`.
    ///  - For full overwrites, both `old_start_offset` and `old_end_offset` must be provided, and
    ///    should be equal to `new_start_offset` and `new_end_offset` respectively. At the end of
    ///    the call, the old interval will have been completely replaced by the new one.
    ///  - For partial overwrites from the start, `old_start_offset` must be provided and be equal
    ///    to `new_start_offset`, and `old_end_offset` must be `None`. At the end of this call, the
    ///    start of the old interval will have been overwritten by the new interval, with the
    ///    remainder of the old interval now starting at `new_end_offset + page::SIZE`.
    ///  - For partial overwrites from the end, `old_end_offset` must be provided and be equal to
    ///    `new_end_offset`, and `old_start_offset` must be `None`. At the end of this call, the end
    ///    of the old interval will have been overwritten by the new interval, with the remainder of
    ///    the old interval now ending at `new_start_offset - page::SIZE`.
    ///  - Partial overwrites in the middle are not allowed. In other words, either
    ///    `old_start_offset` must be the same as `new_start_offset`, or `old_end_offset` must be
    ///    the same as `new_end_offset`, or both.
    pub fn overwrite_zero_interval(
        &mut self,
        old_start_offset: Option<u64>,
        old_end_offset: Option<u64>,
        new_start_offset: u64,
        new_end_offset: u64,
        new_dirty_state: ZeroRangeDirtyState,
    ) -> Result<(), Status> {
        let page_size = page::SIZE as u64;
        debug_assert!(old_start_offset.is_none_or(|off| off.is_multiple_of(page_size)));
        debug_assert!(old_end_offset.is_none_or(|off| off.is_multiple_of(page_size)));
        debug_assert!(new_start_offset.is_multiple_of(page_size));
        debug_assert!(new_end_offset.is_multiple_of(page_size));
        // We only support dirty or untracked zero intervals.
        debug_assert!(
            new_dirty_state == ZeroRangeDirtyState::Dirty
                || new_dirty_state == ZeroRangeDirtyState::Untracked
        );

        let old_start = old_start_offset.map_or(core::ptr::null_mut(), |off| self.lookup_slot(off));
        let old_end = old_end_offset.map_or(core::ptr::null_mut(), |off| self.lookup_slot(off));
        // We should have been able to find either the old start or end sentinel (or both).
        debug_assert!(!old_start.is_null() || !old_end.is_null());
        // If found, the old start and end sentinels are as expected.
        // SAFETY: `old_start` and `old_end`, if non-null, point to valid slots in `self.list`.
        debug_assert!(
            old_start.is_null()
                || unsafe {
                    (*old_start).is_interval_zero()
                        && ((*old_start).is_interval_start() || (*old_start).is_interval_slot())
                }
        );
        // SAFETY: `old_end`, if non-null, points to a valid slot in `self.list`.
        debug_assert!(
            old_end.is_null()
                || unsafe {
                    (*old_end).is_interval_zero()
                        && ((*old_end).is_interval_end() || (*old_end).is_interval_slot())
                }
        );

        // Both of these are assigned by every branch below before they are read.
        let new_start: *mut VmPageOrMarker;
        let new_end: *mut VmPageOrMarker;
        let mut try_merge_left = false;
        let mut try_merge_right = false;

        // Now that we've performed the initial checks, do the actual changes. The rest of this
        // function is structured such that any allocations for node slots are done before making
        // any changes to the page list, so that we don't leave the list in an inconsistent state.
        // Any unused empty slots should be returned so that they can be freed up.
        if !old_start.is_null() && !old_end.is_null() {
            // Overwriting existing slots.
            debug_assert_eq!(old_start_offset, Some(new_start_offset));
            debug_assert_eq!(old_end_offset, Some(new_end_offset));
            // SAFETY: `old_start` and `old_end` are non-null in this branch.
            unsafe {
                // The new interval has a different dirty state.
                debug_assert!((*old_start).zero_interval_dirty_state() != new_dirty_state);
                debug_assert!(
                    (*old_start).zero_interval_dirty_state()
                        == (*old_end).zero_interval_dirty_state()
                );
            }
            new_start = old_start;
            new_end = old_end;
            // We have a new dirty state, so we can try merging the new interval both to the left
            // and the right.
            try_merge_left = true;
            try_merge_right = true;
        } else if !old_start.is_null() {
            // We need to clip at the start.
            debug_assert_eq!(old_start_offset, Some(new_start_offset));
            // SAFETY: `old_start` is non-null in this branch.
            let old_dirty_state = unsafe {
                // The new interval has a different dirty state.
                debug_assert!((*old_start).zero_interval_dirty_state() != new_dirty_state);
                (*old_start).zero_interval_dirty_state()
            };

            new_end = self.lookup_or_allocate_internal(new_end_offset);
            if new_end.is_null() {
                return Err(Status::NO_MEMORY);
            }
            // SAFETY: `new_end` is non-null.
            debug_assert!(new_start_offset == new_end_offset || unsafe { (*new_end).is_empty() });

            // SAFETY: `lookup_or_allocate_internal` returns either null or a valid slot at
            // `new_end_offset + page_size`, which is distinct from `old_start` and `new_end`.
            let Some(clipped_start) =
                (unsafe { self.lookup_or_allocate_internal(new_end_offset + page_size).as_mut() })
            else {
                if new_start_offset != new_end_offset {
                    self.return_empty_slot(new_end_offset);
                }
                return Err(Status::NO_MEMORY);
            };
            if clipped_start.is_interval_end() {
                clipped_start.change_interval_sentinel(SentinelType::Slot);
            } else {
                debug_assert!(clipped_start.is_empty());
                *clipped_start =
                    VmPageOrMarker::zero_interval(SentinelType::Start, old_dirty_state);
            }

            // Now that the clipped start has been created, carry over any remaining
            // awaiting_clean_length from the old start.
            // SAFETY: `old_start` is non-null in this branch.
            let old_len = unsafe { (*old_start).zero_interval_awaiting_clean_length() };
            // `old_start` is only non-null if `old_start_offset` was provided.
            let len = new_end_offset + page_size - old_start_offset.unwrap();
            if old_len > len {
                clipped_start.set_zero_interval_awaiting_clean_length(old_len - len);
            }

            new_start = old_start;
            // We can try merging the new interval to the left since it has a different dirty state
            // from the old interval.
            try_merge_left = true;
        } else {
            // We need to clip at the end.
            debug_assert_eq!(old_end_offset, Some(new_end_offset));
            // SAFETY: `old_end` is non-null in this branch.
            let old_dirty_state = unsafe {
                // The new interval has a different dirty state.
                debug_assert!((*old_end).zero_interval_dirty_state() != new_dirty_state);
                (*old_end).zero_interval_dirty_state()
            };

            new_start = self.lookup_or_allocate_internal(new_start_offset);
            if new_start.is_null() {
                return Err(Status::NO_MEMORY);
            }
            // SAFETY: `new_start` is non-null.
            debug_assert!(new_start_offset == new_end_offset || unsafe { (*new_start).is_empty() });

            // SAFETY: `lookup_or_allocate_internal` returns either null or a valid slot at
            // `new_start_offset - page_size`, which is distinct from `old_end` and `new_start`.
            let Some(clipped_end) =
                (unsafe { self.lookup_or_allocate_internal(new_start_offset - page_size).as_mut() })
            else {
                if new_start_offset != new_end_offset {
                    self.return_empty_slot(new_start_offset);
                }
                return Err(Status::NO_MEMORY);
            };
            if clipped_end.is_interval_start() {
                clipped_end.change_interval_sentinel(SentinelType::Slot);
            } else {
                debug_assert!(clipped_end.is_empty());
                *clipped_end = VmPageOrMarker::zero_interval(SentinelType::End, old_dirty_state);
            }

            new_end = old_end;
            // We can try merging the new interval to the right since it has a different dirty state
            // from the old interval.
            try_merge_right = true;
        }

        // SAFETY: `new_start` and `new_end` are non-null after the branches above.
        unsafe {
            if new_start == new_end {
                *new_start = VmPageOrMarker::zero_interval(SentinelType::Slot, new_dirty_state);
            } else {
                *new_start = VmPageOrMarker::zero_interval(SentinelType::Start, new_dirty_state);
                *new_end = VmPageOrMarker::zero_interval(SentinelType::End, new_dirty_state);
            }
        }

        // See if we can merge left. Note that an interval starting at offset 0 has nothing to merge
        // with on the left.
        if try_merge_left && let Some(left_offset) = new_start_offset.checked_sub(page_size) {
            let left = self.lookup_slot(left_offset);
            // SAFETY: `left`, if non-null, points to a valid slot in `self.list`.
            if !left.is_null()
                && unsafe { (*left).is_interval_zero() }
                && unsafe { (*left).zero_interval_dirty_state() == new_dirty_state }
            {
                // SAFETY: `left` is non-null. `left_ref` is not used after the
                // `return_empty_slot` call below, which reaches the same slot through `self`.
                let left_ref = unsafe { &mut *left };
                if left_ref.is_interval_slot() {
                    left_ref.change_interval_sentinel(SentinelType::Start);
                } else {
                    debug_assert!(left_ref.is_interval_end());
                    *left_ref = VmPageOrMarker::empty();
                    // The node holding `new_start` is still populated at this point, so returning
                    // the left slot cannot free the node `new_start` lives in.
                    self.return_empty_slot(left_offset);
                }

                // SAFETY: `new_start` is non-null and its node is still live (see above).
                // `new_start_ref` is not used after the `return_empty_slot` call below.
                let new_start_ref = unsafe { &mut *new_start };
                if new_start_ref.is_interval_slot() {
                    new_start_ref.change_interval_sentinel(SentinelType::End);
                } else {
                    debug_assert!(new_start_ref.is_interval_start());
                    *new_start_ref = VmPageOrMarker::empty();
                    self.return_empty_slot(new_start_offset);
                }
            }
        }

        // See if we can merge right.
        if try_merge_right && let Some(right_offset) = new_end_offset.checked_add(page_size) {
            let right = self.lookup_slot(right_offset);
            // SAFETY: `right`, if non-null, points to a valid slot in `self.list`.
            if !right.is_null()
                && unsafe { (*right).is_interval_zero() }
                && unsafe { (*right).zero_interval_dirty_state() == new_dirty_state }
            {
                // SAFETY: `right` is non-null. `right_ref` is not used after the
                // `return_empty_slot` call below, which reaches the same slot through `self`.
                let right_ref = unsafe { &mut *right };
                if right_ref.is_interval_slot() {
                    right_ref.change_interval_sentinel(SentinelType::End);
                } else {
                    debug_assert!(right_ref.is_interval_start());
                    *right_ref = VmPageOrMarker::empty();
                    // The node holding `new_end` is still populated at this point, so returning the
                    // right slot cannot free the node `new_end` lives in.
                    self.return_empty_slot(right_offset);
                }

                // SAFETY: `new_end` is non-null and its node is still live (see above).
                // `new_end_ref` is not used after the `return_empty_slot` call below.
                let new_end_ref = unsafe { &mut *new_end };
                if new_end_ref.is_interval_slot() {
                    new_end_ref.change_interval_sentinel(SentinelType::Start);
                } else {
                    debug_assert!(new_end_ref.is_interval_end());
                    *new_end_ref = VmPageOrMarker::empty();
                    self.return_empty_slot(new_end_offset);
                }
            }
        }

        Ok(())
    }

    /// Walk the page tree, calling the passed in function on every tree node.
    pub fn for_every_page<F>(&self, per_page_func: F) -> Result<(), Status>
    where
        F: FnMut(&VmPageOrMarker, u64) -> Status,
    {
        self.for_every_page_in_range(0, Self::MAX_SIZE, per_page_func)
    }

    /// Similar to `for_every_page`, but the `per_page_func` gets called with a `VmPageOrMarkerRef`
    /// instead of a `&VmPageOrMarker`, allowing for limited mutation.
    pub fn for_every_page_ref<F>(&mut self, per_page_func: F) -> Result<(), Status>
    where
        F: FnMut(VmPageOrMarkerRef<'_>, u64) -> Status,
    {
        self.for_every_page_in_range_ref(0, Self::MAX_SIZE, per_page_func)
    }

    /// Similar to `for_every_page`, but the `per_page_func` gets called with a
    /// `&mut VmPageOrMarker`, allowing for mutation.
    pub fn for_every_page_mut<F>(&mut self, per_page_func: F) -> Result<(), Status>
    where
        F: FnMut(&mut VmPageOrMarker, u64) -> Status,
    {
        self.for_every_page_in_range_mut(0, Self::MAX_SIZE, per_page_func)
    }

    /// Internal helper for immutable range traversals.
    ///
    /// Calls the provided callback for every page in the given range.
    fn for_every_page_in_range_internal<F>(
        cursor: &Opaque<bindings::VmPageListBtreeCursor>,
        start_offset: u64,
        end_offset: u64,
        mut per_page_func: F,
    ) -> Status
    where
        F: FnMut(&VmPageOrMarker, u64) -> Status,
    {
        debug_assert!(start_offset.is_multiple_of(page::SIZE as u64));
        debug_assert!(end_offset.is_multiple_of(page::SIZE as u64));

        loop {
            // SAFETY: `cursor.get()` is a valid pointer.
            let entry = unsafe { btree_cursor_next(cursor.get()) };
            if entry.node.is_null() || entry.offset >= end_offset {
                break;
            }
            // SAFETY: `entry.node` is non-null and valid for the lifetime of the borrow of the list
            // held by the caller.
            let node = unsafe { entry.node.cast::<VmPageListNode>().as_ref_unchecked() };
            let start = core::cmp::max(start_offset, entry.offset);
            let end = core::cmp::min(VmPageListNode::end_offset(entry.offset), end_offset);
            let status = node.for_every_page_in_range(entry.offset, start, end, &mut per_page_func);
            if status != Status::NEXT {
                return status;
            }
        }

        Status::NEXT
    }

    /// Walk the page tree, calling the passed in function on every tree node in the given range.
    pub fn for_every_page_in_range<F>(
        &self,
        start_offset: u64,
        end_offset: u64,
        per_page_func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(&VmPageOrMarker, u64) -> Status,
    {
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // Position cursor at lower bound.
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        unsafe {
            let _ = btree_lower_bound(
                self.list.get(),
                VmPageListNode::node_offset(start_offset),
                cursor.get(),
            );
        }
        let status = Self::for_every_page_in_range_internal(
            &cursor,
            start_offset,
            end_offset,
            per_page_func,
        );
        if status != Status::NEXT {
            if status == Status::STOP {
                return Ok(());
            }
            return Err(status);
        }
        Ok(())
    }

    /// Similar to `for_every_page_in_range` but uses a valid `cursor` as the starting point.
    pub fn for_every_page_in_cursor_range<F>(
        cursor: VmPageListCursor<'_>,
        end_offset: u64,
        per_page_func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(&VmPageOrMarker, u64) -> Status,
    {
        let start_offset = cursor.offset();
        if start_offset >= end_offset {
            return Ok(());
        }
        let status = Self::for_every_page_in_range_internal(
            &cursor.cursor,
            start_offset,
            end_offset,
            per_page_func,
        );
        if status != Status::NEXT {
            if status == Status::STOP {
                return Ok(());
            }
            return Err(status);
        }
        Ok(())
    }

    /// Iterates over all pages and gaps in the specified range `[start_offset, end_offset)`.
    ///
    /// `per_page_func` is called for every page/ref/marker or interval sentinel in the range with
    /// `(&VmPageOrMarker, offset)`.
    /// `per_gap_func` is called for every gap (unpopulated range) with `(gap_start, gap_end)`.
    pub fn for_every_page_and_gap_in_range<PageFunc, GapFunc>(
        &self,
        start_offset: u64,
        end_offset: u64,
        mut per_page_func: PageFunc,
        mut per_gap_func: GapFunc,
    ) -> Result<(), Status>
    where
        PageFunc: FnMut(&VmPageOrMarker, u64) -> Status,
        GapFunc: FnMut(u64, u64) -> Status,
    {
        self.for_every_page_and_gap_in_range_internal(start_offset, end_offset, |e| match e {
            PageOrGap::Page(p, off) => per_page_func(p, off),
            PageOrGap::Gap(gap_start, gap_end) => per_gap_func(gap_start, gap_end),
        })
    }

    /// Internal version of `for_every_page_and_gap_in_range` that calls a single `func` for both
    /// pages and gaps, in the same order that `for_every_page_and_gap_in_range` calls
    /// `per_page_func` and `per_gap_func`. This allows a caller to track state across both pages
    /// and gaps in a single closure.
    fn for_every_page_and_gap_in_range_internal<F>(
        &self,
        start_offset: u64,
        end_offset: u64,
        mut func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(PageOrGap<'_>) -> Status,
    {
        let page_size = page::SIZE as u64;
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // Position cursor at lower bound.
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let first_entry = unsafe {
            btree_lower_bound(
                self.list.get(),
                VmPageListNode::node_offset(start_offset),
                cursor.get(),
            )
        };

        let mut expected_next_off = start_offset;
        // Set to true when we encounter an interval start but haven't yet encountered the end.
        let mut in_interval = false;

        let status =
            Self::for_every_page_in_range_internal(&cursor, start_offset, end_offset, |p, off| {
                // Update our interval tracking first. Should the callbacks later request an early
                // exit then this work is wasted, but doing it first, and unconditionally, lets the
                // compiler perform better common expression elimination with the per_gap_func check
                // next.
                if p.is_interval_start() {
                    // We should not already have been tracking an interval.
                    debug_assert!(!in_interval);
                    // Start and end sentinel interval types should match. Since we only support
                    // zero intervals currently, we can simply check for that.
                    debug_assert!(p.is_interval_zero());
                    in_interval = true;
                } else if p.is_interval_end() {
                    // If this is not the first populated slot we encountered, we should have been
                    // tracking a valid interval.
                    debug_assert!(in_interval || expected_next_off == start_offset);
                    // Start and end sentinel interval types should match. Since we only support
                    // zero intervals currently, we can simply check for that.
                    debug_assert!(p.is_interval_zero());
                    // Reset interval tracking.
                    in_interval = false;
                }

                let mut status = Status::NEXT;
                // We can move ahead of expected_next_off in the case of an interval too, which
                // represents a run of pages. Make sure this is not an interval before calling the
                // per_gap_func.
                if expected_next_off != off && !p.is_interval_end() {
                    status = func(PageOrGap::Gap(expected_next_off, off));
                }
                expected_next_off = off + page_size;
                if status == Status::NEXT {
                    status = func(PageOrGap::Page(p, off));
                }
                status
            });

        if status != Status::NEXT {
            if status == Status::STOP {
                return Ok(());
            }
            return Err(status);
        }

        // Handle the last gap after checking that we are not in an interval. Note that simply
        // checking for in_interval is not sufficient, as it is possible to have started the
        // traversal partway into an interval, in which case we would not have seen the interval
        // start and in_interval would be false. So we perform a quick check for in_interval first
        // and if that fails perform the more expensive is_offset_in_interval_helper() check. The
        // is_offset_in_interval_helper() call is further gated by whether we encountered any page
        // at all in the traversal above. If we saw at least one page in the traversal, we know that
        // we could not be in an interval without in_interval being true because we would have seen
        // the interval start.
        if expected_next_off != end_offset {
            // Traversal ended in an interval if in_interval was true, OR if the traversal did not
            // see any page at all and the start_offset is in an interval (Note that in this latter
            // case all offsets in the range [start_offset, end_offset) would lie in the same
            // interval, so we can just check one of them).
            let ended_in_interval = in_interval
                || (expected_next_off == start_offset
                    && !first_entry.node.is_null()
                    && Self::is_offset_in_interval_helper(
                        start_offset,
                        first_entry.offset,
                        // SAFETY: `first_entry.node` is non-null.
                        unsafe { first_entry.node.cast::<VmPageListNode>().as_ref_unchecked() },
                    )
                    .is_some());
            if !ended_in_interval {
                let status = func(PageOrGap::Gap(expected_next_off, end_offset));
                if status != Status::NEXT && status != Status::STOP {
                    return Err(status);
                }
            }
        }

        Ok(())
    }

    /// Similar to `for_every_page_and_gap_in_range`, but the `per_page_func` gets called with a
    /// `VmPageOrMarkerRef` instead of a `&VmPageOrMarker`, allowing for limited mutation.
    pub fn for_every_page_and_gap_in_range_ref<PageFunc, GapFunc>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        mut per_page_func: PageFunc,
        mut per_gap_func: GapFunc,
    ) -> Result<(), Status>
    where
        PageFunc: FnMut(VmPageOrMarkerRef<'_>, u64) -> Status,
        GapFunc: FnMut(u64, u64) -> Status,
    {
        self.for_every_page_and_gap_in_range_internal_mut::<SkipNodeCheck, _>(
            start_offset,
            end_offset,
            |e| match e {
                PageOrGapMut::Page(slot, off) => per_page_func(VmPageOrMarkerRef::new(slot), off),
                PageOrGapMut::Gap(gap_start, gap_end) => per_gap_func(gap_start, gap_end),
            },
        )
    }

    /// Mutable version of `for_every_page_and_gap_in_range_internal`, which calls `func` with a
    /// `PageOrGapMut::Page(&mut VmPageOrMarker, offset)` for every page and
    /// `PageOrGapMut::Gap(gap_start, gap_end)` for every gap. A single closure allows a caller to
    /// track state across both pages and gaps.
    ///
    /// If `N::CLEANUP_EMPTY` is true, then it is assumed `func` may remove pages and page nodes
    /// will be checked to see if they are empty and can be cleaned up.
    fn for_every_page_and_gap_in_range_internal_mut<N: NodeCheck, F>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        mut func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(PageOrGapMut<'_>) -> Status,
    {
        let page_size = page::SIZE as u64;
        let mut expected_next_off = start_offset;
        // Set to true when we encounter an interval start but haven't yet encountered the end.
        let mut in_interval = false;

        let status = self.for_every_page_in_range_internal_mut::<N, _>(
            start_offset,
            end_offset,
            |p, off| {
                // Update our interval tracking first. Should the callbacks later request an early
                // exit then this work is wasted, but doing it first, and unconditionally, lets the
                // compiler perform better common expression elimination with the per_gap_func check
                // next.
                if p.is_interval_start() {
                    // We should not already have been tracking an interval.
                    debug_assert!(!in_interval);
                    // Start and end sentinel interval types should match. Since we only support
                    // zero intervals currently, we can simply check for that.
                    debug_assert!(p.is_interval_zero());
                    in_interval = true;
                } else if p.is_interval_end() {
                    // If this is not the first populated slot we encountered, we should have been
                    // tracking a valid interval.
                    debug_assert!(in_interval || expected_next_off == start_offset);
                    // Start and end sentinel interval types should match. Since we only support
                    // zero intervals currently, we can simply check for that.
                    debug_assert!(p.is_interval_zero());
                    // Reset interval tracking.
                    in_interval = false;
                }

                let mut status = Status::NEXT;
                // We can move ahead of expected_next_off in the case of an interval too, which
                // represents a run of pages. Make sure this is not an interval before calling the
                // gap callback.
                if expected_next_off != off && !p.is_interval_end() {
                    status = func(PageOrGapMut::Gap(expected_next_off, off));
                }
                expected_next_off = off + page_size;
                if status == Status::NEXT {
                    status = func(PageOrGapMut::Page(p, off));
                }
                status
            },
        );

        if status != Status::NEXT {
            if status == Status::STOP {
                return Ok(());
            }
            return Err(status);
        }

        // Handle the last gap after checking that we are not in an interval. Note that simply
        // checking for in_interval is not sufficient, as it is possible to have started the
        // traversal partway into an interval, in which case we would not have seen the interval
        // start and in_interval would be false. So we perform a quick check for in_interval first
        // and if that fails perform the more expensive is_offset_in_interval() check. The
        // is_offset_in_interval() call is further gated by whether we encountered any page at all
        // in the traversal above. If we saw at least one page in the traversal, we know that we
        // could not be in an interval without in_interval being true because we would have seen the
        // interval start.
        //
        // `for_every_page_in_range_internal_mut` does not expose the node the traversal started
        // from, so is_offset_in_interval() performs its own lookup. It is only reached if no
        // callback ran, in which case the page list is unchanged since the traversal started.
        if expected_next_off != end_offset {
            // Traversal ended in an interval if in_interval was true, OR if the traversal did not
            // see any page at all and the start_offset is in an interval (Note that in this latter
            // case all offsets in the range [start_offset, end_offset) would lie in the same
            // interval, so we can just check one of them).
            let ended_in_interval = in_interval
                || (expected_next_off == start_offset && self.is_offset_in_interval(start_offset));
            if !ended_in_interval {
                let status = func(PageOrGapMut::Gap(expected_next_off, end_offset));
                if status != Status::NEXT && status != Status::STOP {
                    return Err(status);
                }
            }
        }

        Ok(())
    }

    /// Walk the page tree, calling `per_page_func` on every page/marker/interval that fulfills
    /// (returns true) the `compare_func`. Also call `contiguous_run_func` on every contiguous range
    /// of such pages/markers/intervals encountered, whose signature is:
    /// `contiguous_run_func(start: u64, end: u64, is_interval: bool) -> Status`
    ///
    /// Intervals are treated as distinct contiguous runs, i.e. they won't be merged into a
    /// contiguous run of pages/markers for invocation of `contiguous_run_func`. For intervals,
    /// `contiguous_run_func` will be called with `is_interval` set to true; for other page types it
    /// will be false. Additionally, the entire interval should fulfill `compare_func` for
    /// `contiguous_run_func` to be called on the portion that falls in
    /// `[start_offset, end_offset)`.
    pub fn for_every_page_and_contiguous_run_in_range<CompareFunc, PageFunc, ContiguousRunFunc>(
        &self,
        start_offset: u64,
        end_offset: u64,
        mut compare_func: CompareFunc,
        mut per_page_func: PageFunc,
        mut contiguous_run_func: ContiguousRunFunc,
    ) -> Result<(), Status>
    where
        CompareFunc: FnMut(&VmPageOrMarker, u64) -> bool,
        PageFunc: FnMut(&VmPageOrMarker, u64) -> Status,
        ContiguousRunFunc: FnMut(u64, u64, bool) -> Status,
    {
        if start_offset == end_offset {
            return Ok(());
        }
        let page_size = page::SIZE as u64;

        // Track contiguous range of pages fulfilling compare_func.
        let mut contiguous_run_start = start_offset;
        let mut contiguous_run_len = 0;

        // Tracks whether we enter the for_every_page_and_gap_in_range traversal at all.
        let mut found_page_or_gap = false;
        // Tracks information if we encounter an interval start, to be used when we encounter the
        // corresponding end.
        struct IntervalTracker {
            interval_start_offset: u64,
            start_compare_status: bool,
            started_interval: bool,
        }
        let mut interval_tracker = IntervalTracker {
            interval_start_offset: 0,
            start_compare_status: false,
            started_interval: false,
        };

        self.for_every_page_and_gap_in_range_internal(
            start_offset,
            end_offset,
            |entry| match entry {
                PageOrGap::Page(p, off) => {
                    found_page_or_gap = true;
                    let mut st = Status::NEXT;
                    let compare_result = compare_func(p, off);

                    // Handle interval types first.
                    if p.is_interval() {
                        // If we are going to start an interval, end any contiguous run being
                        // tracked, and call contiguous_run_func on it. This is because intervals
                        // are treated as contiguous ranges distinct from pages or markers. Do this
                        // before per_page_func because we don't want to have processed extra pages
                        // if contiguous_run_func on the range prior would have failed. Also do this
                        // irrespective of whether this interval passes the compare_func or not,
                        // since we are processing pages prior to the interval.
                        if (p.is_interval_start() || p.is_interval_slot()) && contiguous_run_len > 0
                        {
                            st = contiguous_run_func(
                                contiguous_run_start,
                                contiguous_run_start + contiguous_run_len,
                                /*is_interval=*/ false,
                            );
                            // Reset contiguous range tracking.
                            contiguous_run_len = 0;
                            if st != Status::NEXT {
                                return st;
                            }
                        }
                        debug_assert_eq!(contiguous_run_len, 0);

                        // Run the per-page function on the interval sentinel first. Then proceed to
                        // the more complicated logic for the contiguous function.
                        if compare_result {
                            st = per_page_func(p, off);
                            if st != Status::NEXT && st != Status::STOP {
                                return st;
                            }
                        }

                        // A slot is a contiguous run of a single page.
                        if p.is_interval_slot() {
                            // We should not have been already tracking an interval.
                            debug_assert!(!interval_tracker.started_interval);
                            if compare_result {
                                return contiguous_run_func(
                                    off,
                                    off + page_size,
                                    /*is_interval=*/ true,
                                );
                            }
                            return Status::NEXT;
                        }

                        if p.is_interval_start() {
                            // Start tracking a new run. We should not have been already tracking an
                            // interval.
                            debug_assert!(!interval_tracker.started_interval);
                            interval_tracker.started_interval = true;
                            interval_tracker.interval_start_offset = off;
                            // Stash the comparison result for the interval start.
                            interval_tracker.start_compare_status = compare_result;
                            return Status::NEXT;
                        }

                        debug_assert!(p.is_interval_end());
                        // If the interval end does not pass the check, there is nothing more to be
                        // done.
                        if !compare_result {
                            interval_tracker.started_interval = false;
                            return Status::NEXT;
                        }

                        // If this is the end of an interval, call contiguous_run_func on the
                        // interval if the compare_func passes for *both* the start and the end, and
                        // proceed. It is possible that we don't have the interval start if we
                        // started the traversal partway inside an interval. Find the start and
                        // evaluate compare_func on it.
                        if !interval_tracker.started_interval {
                            let (start, interval_start_offset) =
                                self.find_interval_start_for_end(off);
                            debug_assert!(start.is_interval_start());
                            debug_assert!(interval_start_offset < start_offset);
                            interval_tracker.started_interval = true;
                            interval_tracker.start_compare_status =
                                compare_func(start, interval_start_offset);
                            // Pretend that the interval begins at start_offset since we're not
                            // considering the range before it.
                            interval_tracker.interval_start_offset = start_offset;
                        }
                        debug_assert!(interval_tracker.started_interval);
                        interval_tracker.started_interval = false;
                        if interval_tracker.start_compare_status {
                            return contiguous_run_func(
                                interval_tracker.interval_start_offset,
                                off + page_size,
                                /*is_interval=*/ true,
                            );
                        }
                        return Status::NEXT;
                    }

                    // Handle any non-interval types.
                    debug_assert!(!p.is_interval());
                    debug_assert!(!interval_tracker.started_interval);

                    if compare_result {
                        st = per_page_func(p, off);
                        // Return any errors early before considering this page for
                        // contiguous_run_func.
                        if st != Status::NEXT && st != Status::STOP {
                            // If there was an outstanding contiguous run, process it since it had
                            // to have ended before the failing offset.
                            if contiguous_run_len > 0 {
                                let prev_range_status = contiguous_run_func(
                                    contiguous_run_start,
                                    contiguous_run_start + contiguous_run_len,
                                    /*is_interval=*/ false,
                                );
                                contiguous_run_len = 0;
                                // If there was an error encountered, surface that instead of st, as
                                // it occurred on a range prior to this offset.
                                if prev_range_status != Status::NEXT
                                    && prev_range_status != Status::STOP
                                {
                                    return prev_range_status;
                                }
                            }
                            return st;
                        }

                        // Start tracking a contiguous run if none was being tracked.
                        if contiguous_run_len == 0 {
                            contiguous_run_start = off;
                        }
                        // Append this page to the contiguous range being tracked.
                        contiguous_run_len += page_size;
                        // In the case that st is Status::STOP, we will include this page in the
                        // contiguous run and stop traversal *after* this page.
                        return st;
                    }
                    // We were already tracking a contiguous range when we encountered this page
                    // that does not fulfill compare_func. Invoke contiguous_run_func on the range
                    // so far and start tracking a new one skipping over this page.
                    if contiguous_run_len > 0 {
                        st = contiguous_run_func(
                            contiguous_run_start,
                            contiguous_run_start + contiguous_run_len,
                            /*is_interval=*/ false,
                        );
                        // Reset contiguous_run_len to zero to track a new range later if required.
                        // Do this irrespective of the return status to ensure we don't erroneously
                        // have a remaining range to process below after exiting the traversal.
                        contiguous_run_len = 0;
                    }
                    st
                }
                PageOrGap::Gap(_, _) => {
                    found_page_or_gap = true;
                    // We should not encounter any gaps in the midst of an interval we were
                    // tracking.
                    debug_assert!(!interval_tracker.started_interval);
                    let mut st = Status::NEXT;
                    // We were already tracking a contiguous range when we encountered this gap.
                    // Invoke contiguous_run_func on the range so far and start tracking a new one
                    // skipping over this gap.
                    if contiguous_run_len > 0 {
                        st = contiguous_run_func(
                            contiguous_run_start,
                            contiguous_run_start + contiguous_run_len,
                            /*is_interval=*/ false,
                        );
                        // Reset contiguous_run_len to zero to track a new range later if required.
                        // Do this irrespective of the return status to ensure we don't erroneously
                        // have a remaining range to process below after exiting the traversal.
                        contiguous_run_len = 0;
                    }
                    st
                }
            },
        )?;

        // If we did not execute either the per-page or per-gap function, we could only have been
        // inside an interval. In that case, we need to find both the start and the end of this
        // interval and evaluate compare_func on them.
        if !found_page_or_gap {
            debug_assert!(self.is_offset_in_interval(start_offset));
            debug_assert!(self.is_offset_in_interval(end_offset - page_size));

            let mut interval_end_offset = u64::MAX;
            let mut end_compare_status = false;
            let status = self.for_every_page_in_range(end_offset, Self::MAX_SIZE, |p, off| {
                // The first populated slot should be an interval end.
                debug_assert!(p.is_interval_end());
                interval_end_offset = off;
                end_compare_status = compare_func(p, off);
                Status::STOP
            });
            debug_assert!(status.is_ok());

            if end_compare_status {
                let (start, interval_start_offset) =
                    self.find_interval_start_for_end(interval_end_offset);
                debug_assert!(start.is_interval_start());
                debug_assert!(interval_start_offset < start_offset);
                if compare_func(start, interval_start_offset) {
                    let status =
                        contiguous_run_func(start_offset, end_offset, /*is_interval=*/ true);
                    if status != Status::NEXT && status != Status::STOP {
                        return Err(status);
                    }
                }
            }
            return Ok(());
        }

        // Process the last contiguous range if there is one, or an interval that we started
        // tracking but did not end.
        if contiguous_run_len > 0 {
            let status = contiguous_run_func(
                contiguous_run_start,
                contiguous_run_start + contiguous_run_len,
                /*is_interval=*/ false,
            );
            if status != Status::NEXT && status != Status::STOP {
                return Err(status);
            }
        } else if interval_tracker.started_interval && interval_tracker.start_compare_status {
            let (end, interval_end_offset) =
                self.find_interval_end_for_start(interval_tracker.interval_start_offset);
            debug_assert!(end.is_interval_end());
            if compare_func(end, interval_end_offset) {
                let status = contiguous_run_func(
                    interval_tracker.interval_start_offset,
                    end_offset,
                    /*is_interval=*/ true,
                );
                if status != Status::NEXT && status != Status::STOP {
                    return Err(status);
                }
            }
        }

        Ok(())
    }

    /// Similar to `for_every_page_in_range`, but the `per_page_func` gets called with a
    /// `VmPageOrMarkerRef` instead of a `&VmPageOrMarker`, allowing for limited mutation.
    pub fn for_every_page_in_range_ref<F>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        mut per_page_func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(VmPageOrMarkerRef<'_>, u64) -> Status,
    {
        self.for_every_page_in_range_mut(start_offset, end_offset, |slot, offset| {
            per_page_func(VmPageOrMarkerRef::new(slot), offset)
        })
    }

    /// Similar to `for_every_page_in_range`, but the `per_page_func` gets called with a
    /// `&mut VmPageOrMarker`, allowing for mutation.
    pub fn for_every_page_in_range_mut<F>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        per_page_func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(&mut VmPageOrMarker, u64) -> Status,
    {
        let status = self.for_every_page_in_range_internal_mut::<SkipNodeCheck, _>(
            start_offset,
            end_offset,
            per_page_func,
        );
        if status != Status::NEXT {
            if status == Status::STOP {
                return Ok(());
            }
            return Err(status);
        }
        Ok(())
    }

    /// Internal helper for mutable range traversals with optional empty-node cleanup.
    ///
    /// Calls the provided callback for every page in the given range. If `N::CLEANUP_EMPTY` is
    /// true, then it is assumed the `per_page_func` may remove pages and page nodes will be checked
    /// to see if they are empty and can be cleaned up.
    ///
    /// Returns `Status::NEXT` if the whole range was traversed, otherwise the first status other
    /// than `Status::NEXT` returned by `per_page_func`.
    fn for_every_page_in_range_internal_mut<N: NodeCheck, F>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        mut per_page_func: F,
    ) -> Status
    where
        F: FnMut(&mut VmPageOrMarker, u64) -> Status,
    {
        debug_assert!(start_offset.is_multiple_of(page::SIZE as u64));
        debug_assert!(end_offset.is_multiple_of(page::SIZE as u64));

        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let mut entry = unsafe {
            btree_lower_bound(
                self.list.get(),
                VmPageListNode::node_offset(start_offset),
                cursor.get(),
            )
        };
        while !entry.node.is_null() {
            if entry.offset >= end_offset {
                break;
            }
            // SAFETY: `entry.node` is non-null and valid for the node's scope.
            let node = unsafe { entry.node.cast::<VmPageListNode>().as_mut_unchecked() };
            let start = core::cmp::max(start_offset, entry.offset);
            let end = core::cmp::min(VmPageListNode::end_offset(entry.offset), end_offset);
            let status =
                node.for_every_page_in_range_mut(entry.offset, start, end, &mut per_page_func);
            let is_empty = node.is_empty();
            if N::CLEANUP_EMPTY {
                if is_empty {
                    // SAFETY: `cursor` is positioned at this valid node.
                    // `cpp_vm_page_list_btree_erase_at` erases the node and updates `cursor`
                    // to point to the next node in the tree.
                    unsafe {
                        bindings::cpp_vm_page_list_btree_erase_at(self.list.get(), cursor.get());
                    }
                } else {
                    // SAFETY: `cursor.get()` is a valid pointer.
                    let _ = unsafe { btree_cursor_next(cursor.get()) };
                }
            } else {
                // SAFETY: `cursor.get()` is a valid pointer.
                let _ = unsafe { btree_cursor_next(cursor.get()) };
            }
            if status != Status::NEXT {
                return status;
            }
            // Peek at the new node after step or erase.
            // SAFETY: `cursor.get()` is a valid pointer.
            entry = unsafe { btree_cursor_get(cursor.get()) };
        }
        Status::NEXT
    }

    /// Calls the provided callback for every page or marker in the range
    /// [start_offset, end_offset). The callback can modify the `VmPageOrMarker` and take ownership
    /// of any pages, or leave them in place. The difference between this and `for_every_page` is
    /// that because this allows for modifying the underlying pages, any intermediate data
    /// structures can be checked and potentially freed if no longer needed.
    pub fn remove_pages<F>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        per_page_fn: F,
    ) -> Result<(), Status>
    where
        F: FnMut(&mut VmPageOrMarker, u64) -> Status,
    {
        let status = self.for_every_page_in_range_internal_mut::<CleanupEmptyNodeCheck, _>(
            start_offset,
            end_offset,
            per_page_fn,
        );
        if status != Status::NEXT {
            if status == Status::STOP {
                return Ok(());
            }
            return Err(status);
        }
        Ok(())
    }

    /// Similar to `remove_pages` but also takes a `per_gap_fn` callback to allow for iterating over
    /// any gaps encountered as well. This can be used when the intent is to modify the underlying
    /// pages and/or gaps, while checking any intermediate data structures to potentially free ones
    /// that are no longer needed.
    pub fn remove_pages_and_iterate_gaps<PageFunc, GapFunc>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        mut per_page_fn: PageFunc,
        mut per_gap_fn: GapFunc,
    ) -> Result<(), Status>
    where
        PageFunc: FnMut(&mut VmPageOrMarker, u64) -> Status,
        GapFunc: FnMut(u64, u64) -> Status,
    {
        self.remove_pages_and_iterate_gaps_internal(start_offset, end_offset, |e| match e {
            PageOrGapMut::Page(slot, off) => per_page_fn(slot, off),
            PageOrGapMut::Gap(gap_start, gap_end) => per_gap_fn(gap_start, gap_end),
        })
    }

    /// Internal version of `remove_pages_and_iterate_gaps` that calls a single `func` for both
    /// pages and gaps, in the same order that `remove_pages_and_iterate_gaps` calls `per_page_fn`
    /// and `per_gap_fn`. This allows a caller to track state across both pages and gaps in a single
    /// closure.
    fn remove_pages_and_iterate_gaps_internal<F>(
        &mut self,
        start_offset: u64,
        end_offset: u64,
        func: F,
    ) -> Result<(), Status>
    where
        F: FnMut(PageOrGapMut<'_>) -> Status,
    {
        self.for_every_page_and_gap_in_range_internal_mut::<CleanupEmptyNodeCheck, _>(
            start_offset,
            end_offset,
            func,
        )
    }

    /// Merges the pages in the specified range in `self` onto `other`, with `offset` in `self`
    /// mapping to the offset of 0 in `other`.
    ///
    /// For any offset in `self` that is not empty then the given `migrate_fn` is called with a
    /// reference to `self` and the corresponding slot in `other` and has the signature of:
    /// `fn(this_slot: &mut VmPageOrMarker, other_slot: &mut VmPageOrMarker, other_offset: u64)`.
    ///
    /// If this returns true then `migrate_fn` was called on everything in range of `offset` to
    /// `end_offset`. If false is returned then merging did not complete due to inability to
    /// allocate nodes in `other`. The partial merge is not rolled back and is up to the caller to
    /// deal with.
    pub fn merge_range_onto<F>(
        &mut self,
        mut migrate_fn: F,
        other: &mut VmPageList,
        offset: u64,
        end_offset: u64,
    ) -> bool
    where
        F: FnMut(&mut VmPageOrMarker, &mut VmPageOrMarker, u64),
    {
        let cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
        // Iterate the range in `self` we are merging.
        // SAFETY: `self.list.get()` and `cursor.get()` are valid pointers.
        let mut entry = unsafe {
            btree_lower_bound(self.list.get(), VmPageListNode::node_offset(offset), cursor.get())
        };
        while !entry.node.is_null() && entry.offset < end_offset {
            let node_obj_offset = entry.offset;
            // SAFETY: `entry.node` is non-null and points to a valid `VmPageListNode` owned by
            // `self`'s tree. `self` is exclusively borrowed, and the node is only erased after the
            // last use of `node`, at the end of this iteration.
            let node = unsafe { entry.node.cast::<VmPageListNode>().as_mut_unchecked() };
            debug_assert!(node.has_no_interval_sentinel());

            // Calculate start and end in `node`.
            let mut node_start_off = core::cmp::max(node_obj_offset, offset);
            let node_end_off =
                core::cmp::min(VmPageListNode::end_offset(node_obj_offset), end_offset);

            // If offset is not a multiple of `VmPageListNode::NODE_SPAN_BYTES` then items in this
            // node will need to be split across two different nodes in `other`.
            while node_start_off < node_end_off {
                // Translate the start in `node` to an offset in `other`.
                let other_start_off = node_start_off - offset;

                // Find the node in `other` that contains the start address.
                let other_obj_offset = VmPageListNode::node_offset(other_start_off);
                let other_cursor = Opaque::<bindings::VmPageListBtreeCursor>::uninit();
                // SAFETY: `other.list.get()` and `other_cursor.get()` are valid pointers.
                let other_entry = unsafe {
                    btree_lower_bound(other.list.get(), other_obj_offset, other_cursor.get())
                };

                // Allocate a new node if none was found.
                let other_node_ptr =
                    if other_entry.node.is_null() || other_entry.offset != other_obj_offset {
                        // SAFETY: `other.list.get()` and `other_cursor.get()` are valid pointers.
                        // `cpp_vm_page_list_btree_insert` uses the cursor as an insertion hint and
                        // updates it to point at the newly inserted node.
                        let new_node = unsafe {
                            bindings::cpp_vm_page_list_btree_insert(
                                other.list.get(),
                                other_obj_offset,
                                other_cursor.get(),
                            )
                        };
                        if new_node.is_null() {
                            return false;
                        }
                        new_node
                    } else {
                        other_entry.node
                    };
                // SAFETY: `other_node_ptr` is non-null and points to a valid `VmPageListNode` owned
                // by `other`'s tree, which is exclusively borrowed. It is distinct from `node`, as
                // `self` and `other` are distinct `&mut` borrows, and it is only erased after the
                // last use of `other_node` below.
                let other_node =
                    unsafe { other_node_ptr.cast::<VmPageListNode>().as_mut_unchecked() };

                // Cap the range to the end of `other_node`, which could be less than length of the
                // range in `node`.
                let other_end_off = core::cmp::min(
                    VmPageListNode::end_offset(other_obj_offset),
                    node_end_off - offset,
                );
                let len = other_end_off - other_start_off;

                node.merge_range_onto(
                    node_obj_offset,
                    other_obj_offset,
                    &mut migrate_fn,
                    other_node,
                    node_start_off,
                    node_start_off + len,
                    other_start_off,
                );

                // If either nothing was transferred, or all existing content was removed, then
                // remove `other_node`.
                if other_node.is_empty() {
                    // SAFETY: `other.list.get()` is valid and `other_cursor` is positioned at the
                    // node being erased, either by the `lower_bound` above or by the insert.
                    unsafe {
                        bindings::cpp_vm_page_list_btree_erase_at(
                            other.list.get(),
                            other_cursor.get(),
                        );
                    }
                }

                node_start_off += len;
            }
            // If all the content was moved out of `node` then remove it. `node` could still have
            // content if `migrate_fn` chose to leave it, or if the range being processed only
            // partially covered `node`.
            if node.is_empty() {
                // SAFETY: `self.list.get()` is valid and `cursor` is positioned at this node.
                // `cpp_vm_page_list_btree_erase_at` erases the node and updates `cursor` to point
                // to the next node in the tree.
                unsafe {
                    bindings::cpp_vm_page_list_btree_erase_at(self.list.get(), cursor.get());
                }
            } else {
                // SAFETY: `cursor.get()` is a valid pointer.
                let _ = unsafe { btree_cursor_next(cursor.get()) };
            }
            // Peek at the new node after the step or erase.
            // SAFETY: `cursor.get()` is a valid pointer.
            entry = unsafe { btree_cursor_get(cursor.get()) };
        }
        true
    }

    /// Returns true if any pages (actual pages, references, or markers) are in the given range, or
    /// if the range forms a part of a sparse page interval.
    pub fn any_pages_or_intervals_in_range(&self, start_offset: u64, end_offset: u64) -> bool {
        let mut found_page = false;
        let _ = self.for_every_page_in_range(start_offset, end_offset, |_, _| {
            found_page = true;
            Status::STOP
        });
        // It is possible that the range forms a part of an interval even if no nodes in the range
        // have populated slots. We can determine that by checking to see if the start offset in
        // the range falls in an interval (we could technically perform this check for any inclusive
        // offset in the range since the range is entirely unpopulated and hence would only fall in
        // the same interval if applicable).
        if found_page { true } else { self.is_offset_in_interval(start_offset) }
    }

    /// Similar to `any_pages_or_intervals_in_range` but skips over any ParentContent markers, as
    /// these do not represent content owned by this page list, but rather content owned by a
    /// parent.
    pub fn any_owned_pages_or_intervals_in_range(
        &self,
        start_offset: u64,
        end_offset: u64,
    ) -> bool {
        let mut found_page = false;
        let _ = self.for_every_page_in_range(start_offset, end_offset, |page, _| {
            if page.is_parent_content() {
                return Status::NEXT;
            }
            found_page = true;
            Status::STOP
        });
        // It is possible that the range forms a part of an interval even if no nodes in the range
        // have populated slots. We can determine that by checking to see if the start offset in
        // the range falls in an interval (we could technically perform this check for any inclusive
        // offset in the range since the range is entirely unpopulated and hence would only fall in
        // the same interval if applicable).
        if found_page { true } else { self.is_offset_in_interval(start_offset) }
    }

    /// Release and call `free_content_fn` on every item in the page list. Gives `free_content_fn`
    /// ownership of the content. After calling this method, all slots in the page list are empty.
    pub fn remove_all_content<F>(&mut self, mut free_content_fn: F)
    where
        F: FnMut(VmPageOrMarker),
    {
        // walk the tree in order, freeing all the pages on every node
        for node in self.nodes_mut() {
            // per page get a reference to the page pointer inside the page list node
            for slot in &mut node.pages {
                if !slot.is_empty() {
                    free_content_fn(slot.swap(VmPageOrMarker::empty()));
                }
            }
        }
        // empty the tree
        self.clear();
    }
}

impl Default for VmPageList {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for VmPageList {
    fn drop(&mut self) {
        ltracef!("{:p}\n", core::ptr::from_ref(self));
        debug_assert!(
            self.has_no_page_ref_or_marker(),
            "VmPageList dropped with live pages, refs, or markers"
        );
        // SAFETY: `self.list.get()` is a valid initialized VmPageListBtree pointer to be destroyed.
        unsafe {
            bindings::cpp_vm_page_list_btree_destroy(self.list.get());
        }
    }
}

impl core::fmt::Debug for VmPageList {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmPageList")
            .field("is_empty", &self.is_empty())
            .field("has_no_page_or_ref", &self.has_no_page_or_ref())
            .field("has_no_page_ref_or_marker", &self.has_no_page_ref_or_marker())
            .finish()
    }
}

/// States used to represent the life cycle of the `VmPageSpliceList`, as per the comment at the
/// declaration.
///
/// The discriminants match the C++ `VmPageSpliceList::State` enum (asserted below), since both
/// languages operate on the same object. C++ only ever stores these four values, so reading `state`
/// from an instance that C++ has mutated is sound.
#[repr(u8)]
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    Constructed = 0,
    Initialized = 1,
    Finalized = 2,
    Processed = 3,
}

zr::static_assert!(
    core::mem::size_of::<State>() == core::mem::size_of::<bindings::VmPageSpliceList_State>()
);
zr::static_assert!(State::Constructed as u8 == bindings::VmPageSpliceList_State_Constructed);
zr::static_assert!(State::Initialized as u8 == bindings::VmPageSpliceList_State_Initialized);
zr::static_assert!(State::Finalized as u8 == bindings::VmPageSpliceList_State_Finalized);
zr::static_assert!(State::Processed as u8 == bindings::VmPageSpliceList_State_Processed);

/// Class which holds the list of vm_page structs removed from a VmPageList by `add_pages_from`. The
/// list includes information about uncommitted pages and markers. Every splice list is expected to
/// go through the following series of states:
/// 1. The splice list is created.
/// 2. List is Initialized with the desired range.
/// 3. Pages are added to the splice list.
/// 4. The list is `finalize`d, meaning that it can no longer be modified by `insert`.
/// 5. Pages are then `pop`ped from the list. Once all the pages are popped, the list is considered
///    "processed".
/// 6. The list is then considered `Processed` and can be destroyed.
///
/// The field layout must match the C++ `VmPageSpliceList` exactly: the C++ non-template members
/// forward to this implementation, and C++ operates on instances created here through the pointer
/// handed out by `as_raw`.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VmPageSpliceList {
    length: u64,
    pos: u64,
    state: State,
    page_list: VmPageList,
    #[pin]
    _pinned: core::marker::PhantomPinned,
}

zr::static_assert!(
    core::mem::size_of::<VmPageSpliceList>() == core::mem::size_of::<bindings::VmPageSpliceList>()
);
zr::static_assert!(
    core::mem::align_of::<VmPageSpliceList>()
        == core::mem::align_of::<bindings::VmPageSpliceList>()
);

impl VmPageSpliceList {
    /// Returns an in-place initializer for stack-pinning a `VmPageSpliceList`.
    pub fn new() -> impl pin_init::PinInit<Self> {
        pin_init::pin_init!(Self {
            length: 0,
            pos: 0,
            state: State::Constructed,
            page_list: VmPageList::new(),
            _pinned: core::marker::PhantomPinned,
        })
    }

    /// Convenience initializer equivalent to `new` followed by `initialize(length)`.
    pub fn with_length(length: u64) -> impl pin_init::PinInit<Self> {
        pin_init::pin_init!(Self {
            length,
            pos: 0,
            state: State::Initialized,
            page_list: VmPageList::new(),
            _pinned: core::marker::PhantomPinned,
        })
    }

    /// Initialize the list with the given range. Can only be done once, and must be done before
    /// providing pages.
    pub fn initialize(self: Pin<&mut Self>, length: u64) {
        debug_assert!(self.is_empty() && self.state == State::Constructed);
        let this = self.project();
        *this.length = length;
        *this.state = State::Initialized;
    }

    /// Pops the next page off of the splice list. It is invalid to pop a page from a non-finalized
    /// splice list.
    pub fn pop(self: Pin<&mut Self>) -> VmPageOrMarker {
        if !self.is_finalized() {
            debug_assert!(false, "attempted to Pop from a non-finalized splice list");
            return VmPageOrMarker::empty();
        }

        let this = self.project();
        let res = this.page_list.remove_content(*this.pos);
        *this.pos += page::SIZE as u64;
        if *this.pos >= *this.length {
            *this.state = State::Processed;
        }
        res
    }

    /// Takes the content out of `source` and places them in this splice list, which must have
    /// already been initialized but still be empty. The range to be taken is determined by the
    /// provided `offset` and the length of this splice list. May return `Status::NO_MEMORY`, in
    /// which case an unspecified number of pages will have been moved into this splice list.
    ///
    /// Takes a function with the signature
    /// `fn(src: &mut VmPageOrMarker, dst: &mut VmPageOrMarker, offset: u64)`, which is expected to
    /// move content from `src` to `dst`.
    pub fn add_pages_from<F>(
        mut self: Pin<&mut Self>,
        merge_func: F,
        source: &mut VmPageList,
        offset: u64,
    ) -> Result<(), Status>
    where
        F: FnMut(&mut VmPageOrMarker, &mut VmPageOrMarker, u64),
    {
        let this = self.as_mut().project();
        let end = offset + *this.length;
        let result = source.merge_range_onto(merge_func, this.page_list, offset, end);
        self.finalize();
        if result { Ok(()) } else { Err(Status::NO_MEMORY) }
    }

    /// Iterates all pages in this splice list without taking ownership. Pages are not considered
    /// processed and can not be removed using this iterator. The callback is expected to have the
    /// signature: `fn page_func(slot: VmPageOrMarkerRef<'_>, offset: u64) -> Status`.
    /// The iterator will start at `start_offset` and iterate until the end of the
    /// `VmPageSpliceList`. `start_offset` must be page aligned and less than the length of the
    /// list.
    pub fn mutate_pages<F>(
        self: Pin<&mut Self>,
        page_func: F,
        start_offset: u64,
    ) -> Result<(), Status>
    where
        F: FnMut(VmPageOrMarkerRef<'_>, u64) -> Status,
    {
        debug_assert!(self.is_finalized());
        debug_assert!(start_offset < self.length);
        debug_assert!(start_offset.is_multiple_of(page::SIZE as u64));
        let this = self.project();
        this.page_list.for_every_page_in_range_ref(start_offset, *this.length, page_func)
    }

    /// Iterates all the pages and gaps in this splice list. `page_func` is given ownership of each
    /// page. It is invalid to process a non-finalized splice list. The two callbacks are expected
    /// to have the signatures:
    /// `fn page_func(slot: VmPageOrMarker, splice_offset: u64) -> Status`
    /// `fn gap_func(gap_start: u64, gap_end: u64) -> Status`
    ///
    /// Because `page_func` always takes ownership of the content, if it returns any kind of error,
    /// either a graceful request to stop iteration via `Status::STOP` or an explicit error, then
    /// that offset is considered processed and will not be repeated in future calls. In contrast,
    /// if iteration is stopped during `gap_func`, that gap will not be considered processed and can
    /// be returned in future calls.
    pub fn remove_pages_and_iterate_gaps<PageFunc, GapFunc>(
        self: Pin<&mut Self>,
        mut page_func: PageFunc,
        mut gap_func: GapFunc,
    ) -> Result<(), Status>
    where
        PageFunc: FnMut(VmPageOrMarker, u64) -> Status,
        GapFunc: FnMut(u64, u64) -> Status,
    {
        debug_assert!(self.is_finalized());
        let this = self.project();
        let length = *this.length;
        // Assume we will successfully process the whole range. This will get trimmed should any of
        // the callbacks terminate early.
        let mut processed = length;
        let status =
            this.page_list.remove_pages_and_iterate_gaps_internal(*this.pos, length, |e| match e {
                PageOrGapMut::Page(slot, src_offset) => {
                    // Move the content out of the slot before passing it to `page_func` to ensure
                    // it *must* deal with it and it cannot be left in the slot.
                    let content = slot.swap(VmPageOrMarker::empty());
                    let status = page_func(content, src_offset);
                    if status != Status::NEXT {
                        processed = src_offset + page::SIZE as u64;
                    }
                    status
                }
                PageOrGapMut::Gap(gap_start, gap_end) => {
                    let status = gap_func(gap_start, gap_end);
                    if status != Status::NEXT {
                        processed = gap_start;
                    }
                    status
                }
            });
        *this.pos = processed;
        if *this.pos == length {
            debug_assert!(this.page_list.is_empty());
            *this.state = State::Processed;
        }
        status
    }

    /// Inserts `content` into the splice list at the specified `offset`. The splice list takes
    /// ownership of `content` after this call. It is invalid to append to a finalized splice list.
    pub fn insert(
        self: Pin<&mut Self>,
        offset: u64,
        mut content: VmPageOrMarker,
    ) -> Result<(), Status> {
        assert!(offset < self.length);
        assert!(self.is_initialized());
        assert!(!content.is_interval());

        let this = self.project();
        let (slot, _) = this.page_list.lookup_or_allocate(offset, IntervalHandling::NoIntervals);
        let Some(slot) = slot else {
            // If the allocation failed, we need to free content.
            Self::free_content(&mut content);
            return Err(Status::NO_MEMORY);
        };
        *slot = content;
        Ok(())
    }

    /// Returns true after the whole collection has been processed by Pop.
    pub fn is_processed(&self) -> bool {
        self.state == State::Processed
    }

    /// Returns true if the collection has been Initialized.
    pub fn is_initialized(&self) -> bool {
        self.state == State::Initialized
    }

    /// Returns true if this list is empty.
    pub fn is_empty(&self) -> bool {
        self.page_list.is_empty()
    }

    /// Marks the list as finalized.
    ///
    /// See the comment at `VmPageSpliceList`'s declaration for more info on what this means and
    /// when to call it. Note that it is invalid to call `finalize` twice on the same list.
    pub fn finalize(self: Pin<&mut Self>) {
        debug_assert!(self.is_initialized());
        *self.project().state = State::Finalized;
    }

    /// Returns true if the splice list is finalized.
    ///
    /// See the comment at [`VmPageSpliceList`]'s declaration for more info on what this means.
    pub fn is_finalized(&self) -> bool {
        self.state == State::Finalized
    }

    /// Returns the current position in the list.
    pub fn position(&self) -> u64 {
        self.pos
    }

    /// For use by PhysicalPageProvider. The user-pager path doesn't use this. This returns a
    /// finalized list.
    pub fn create_from_page_list(
        length: u64,
        mut pages: Pin<&mut VmPageDoublyLinkedList>,
        mut splice: Pin<&mut Self>,
    ) -> Result<(), Status> {
        // TODO(https://fxbug.dev/42170136): This method needs coverage in vmpl_unittests.
        debug_assert!(pages.iter().count() as u64 == length / page::SIZE as u64);
        splice.as_mut().initialize(length);
        let mut offset = 0;
        while let Some(page) = pages.as_mut().pop_front() {
            splice.as_mut().insert(offset, VmPageOrMarker::from_page(VmPagePtr::new(page)))?;
            offset += page::SIZE as u64;
        }
        splice.finalize();
        Ok(())
    }

    /// Frees the page or reference held by `content`, leaving it empty.
    fn free_content(content: &mut VmPageOrMarker) {
        if content.is_page() {
            let page = content.release_page();
            // SAFETY: The splice list owns this page, so nothing else can be concurrently linking
            // it into or out of a list.
            debug_assert!(!unsafe { (*page.as_ref().queue_node.get()).in_container() });
            // SAFETY: The splice list owns this page and is giving up ownership of it.
            unsafe { pmm::free_page(page) };
        } else if content.is_reference() {
            let compression = pmm_node()
                .get_page_compression()
                .expect("a reference cannot exist without a compression instance");
            // SAFETY: The reference was produced by the global compression instance, the splice
            // list owns it and is relinquishing ownership, and it is not a temporary reference so
            // no VMO lock is required.
            unsafe { compression.free(content.release_reference()) };
        }
    }

    /// Frees every page and reference still owned by the splice list.
    fn free_all_pages(self: Pin<&mut Self>) {
        let this = self.project();
        // Free any pages owned by the splice list.
        this.page_list.remove_all_content(|mut content| Self::free_content(&mut content));
        *this.state = State::Processed;
    }

    /// Returns a raw pointer to the underlying `VmPageSpliceList`.
    ///
    /// Callers must not use the returned raw pointer to move the object in memory.
    pub fn as_raw(self: Pin<&mut Self>) -> *mut bindings::VmPageSpliceList {
        // SAFETY: Obtaining a raw pointer does not move the pinned object. The layout of this type
        // matches the C++ `VmPageSpliceList`, as asserted above.
        unsafe { core::ptr::from_mut(self.get_unchecked_mut()).cast() }
    }

    /// Frees any pages still owned by the list, as appropriate for its current state.
    ///
    /// This is the body of the destructor, shared between the Rust `PinnedDrop` and the C++
    /// `~VmPageSpliceList()`. It deliberately does not drop `page_list`: for a Rust-owned list the
    /// field drop glue runs after `PinnedDrop`, and for a C++-owned list the C++ member destructor
    /// runs after the trampoline returns.
    fn destroy(mut self: Pin<&mut Self>) {
        match self.state {
            State::Constructed => {}
            State::Initialized => {
                // Finalize the list so any remaining pages can be freed, then fall through to
                // freeing them.
                *self.as_mut().project().state = State::Finalized;
                self.free_all_pages();
            }
            State::Finalized => self.free_all_pages(),
            State::Processed => {}
        }
    }
}

#[pin_init::pinned_drop]
impl PinnedDrop for VmPageSpliceList {
    fn drop(self: Pin<&mut Self>) {
        self.destroy();
    }
}

// FFI trampolines for C++ calling into the Rust `VmPageSpliceList`.
//
// The C++ `VmPageSpliceList` has the same layout as this one, so C++ passes its `this` pointer
// straight through. C++ owns the object and does not move it for the duration of the call, so it is
// pinned here.

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_splice_list_destroy(list: &mut VmPageSpliceList) {
    // SAFETY: C++ owns `list` and does not move it.
    unsafe { Pin::new_unchecked(list) }.destroy();
}

/// `out` must refer to an empty `VmPageOrMarker` that is not part of the list; the popped content
/// is moved into it.
#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_splice_list_pop(list: &mut VmPageSpliceList, out: &mut VmPageOrMarker) {
    // SAFETY: C++ owns `list` and does not move it.
    *out = unsafe { Pin::new_unchecked(list) }.pop();
}

/// `content` is C++'s by-value parameter; ownership of its contents is taken and it is left empty
/// for the C++ destructor.
#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_splice_list_insert(
    list: &mut VmPageSpliceList,
    offset: u64,
    content: &mut VmPageOrMarker,
) -> Result<(), Status> {
    let content = content.swap(VmPageOrMarker::empty());
    // SAFETY: C++ owns `list` and does not move it.
    unsafe { Pin::new_unchecked(list) }.insert(offset, content)
}

// FFI trampolines for C++ calling into the Rust `VmPageList`.
//
// The C++ `VmPageList` has the same layout as this one, so C++ passes its `this` pointer straight
// through and it is reinterpreted here. The trampolines take it as a reference rather than a raw
// pointer: C++ only ever passes a non-null pointer to an initialized list and does not otherwise
// touch the list or its nodes for the duration of the call, so the reference's validity and
// aliasing requirements are upheld by the caller. The slot pointers returned to C++ derive from the
// node allocations rather than from the reference, so they stay valid after the reference is gone.

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_has_no_page_or_ref(list: &VmPageList) -> bool {
    list.has_no_page_or_ref()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_has_no_page_ref_or_marker(list: &VmPageList) -> bool {
    list.has_no_page_ref_or_marker()
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_return_empty_slot(list: &mut VmPageList, offset: u64) {
    list.return_empty_slot(offset)
}

/// `out` must refer to an empty `VmPageOrMarker` that is not part of the list; the removed content
/// is moved into it.
#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_remove_content(
    list: &mut VmPageList,
    offset: u64,
    out: &mut VmPageOrMarker,
) {
    *out = list.remove_content(offset);
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_is_offset_in_zero_interval(list: &VmPageList, offset: u64) -> bool {
    list.is_offset_in_zero_interval(offset)
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_is_offset_in_interval(list: &VmPageList, offset: u64) -> bool {
    list.is_offset_in_interval(offset)
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_return_interval_slot(list: &mut VmPageList, offset: u64) {
    list.return_interval_slot(offset)
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_populate_slots_in_interval(
    list: &mut VmPageList,
    start_offset: u64,
    end_offset: u64,
) -> Result<(), Status> {
    list.populate_slots_in_interval(start_offset, end_offset)
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_clip_interval_start(
    list: &mut VmPageList,
    interval_start: u64,
    len: u64,
) -> Result<(), Status> {
    list.clip_interval_start(interval_start, len)
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_clip_interval_end(
    list: &mut VmPageList,
    interval_end: u64,
    len: u64,
) -> Result<(), Status> {
    list.clip_interval_end(interval_end, len)
}

#[repr(C)]
struct VmPageOrMarkerSlotAndBool {
    slot: *mut VmPageOrMarker,
    is_interval: bool,
}

#[repr(C)]
struct VmPageOrMarkerConstSlotAndOffset {
    slot: *const VmPageOrMarker,
    offset: u64,
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_lookup_or_allocate_internal(
    list: &mut VmPageList,
    offset: u64,
) -> *mut VmPageOrMarker {
    list.lookup_or_allocate_internal(offset)
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_lookup_or_allocate_check_for_interval(
    list: &mut VmPageList,
    offset: u64,
    split_interval: bool,
) -> VmPageOrMarkerSlotAndBool {
    let (slot, is_interval) = list.lookup_or_allocate_check_for_interval(offset, split_interval);
    VmPageOrMarkerSlotAndBool {
        slot: slot.map_or(core::ptr::null_mut(), core::ptr::from_mut),
        is_interval,
    }
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_find_interval_start_for_end(
    list: &VmPageList,
    end_offset: u64,
) -> VmPageOrMarkerConstSlotAndOffset {
    let (slot, offset) = list.find_interval_start_for_end(end_offset);
    VmPageOrMarkerConstSlotAndOffset { slot: core::ptr::from_ref(slot), offset }
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_find_interval_end_for_start(
    list: &VmPageList,
    start_offset: u64,
) -> VmPageOrMarkerConstSlotAndOffset {
    let (slot, offset) = list.find_interval_end_for_start(start_offset);
    VmPageOrMarkerConstSlotAndOffset { slot: core::ptr::from_ref(slot), offset }
}

// The three trampolines below take the dirty state as a plain `u32` rather than as a
// `ZeroRangeDirtyState`, so that an out of range discriminant from C++ is a panic rather than
// undefined behavior.

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_add_zero_interval_internal(
    list: &mut VmPageList,
    start_offset: u64,
    end_offset: u64,
    dirty_state: u32,
    awaiting_clean_len: u64,
    replace_existing_slot: bool,
) -> Result<(), Status> {
    list.add_zero_interval_internal(
        start_offset,
        end_offset,
        ZeroRangeDirtyState::from_raw(dirty_state),
        awaiting_clean_len,
        replace_existing_slot,
    )
}

#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_replace_page_with_zero_interval(
    list: &mut VmPageList,
    offset: u64,
    dirty_state: u32,
) -> VmPagePtr {
    list.replace_page_with_zero_interval(offset, ZeroRangeDirtyState::from_raw(dirty_state))
}

/// `old_start_offset` and `old_end_offset` use the C++ `UINT64_MAX` sentinel to mean "not
/// specified"; it is translated to `None` here.
#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_overwrite_zero_interval(
    list: &mut VmPageList,
    old_start_offset: u64,
    old_end_offset: u64,
    new_start_offset: u64,
    new_end_offset: u64,
    new_dirty_state: u32,
) -> Result<(), Status> {
    let old_start = (old_start_offset != u64::MAX).then_some(old_start_offset);
    let old_end = (old_end_offset != u64::MAX).then_some(old_end_offset);
    list.overwrite_zero_interval(
        old_start,
        old_end,
        new_start_offset,
        new_end_offset,
        ZeroRangeDirtyState::from_raw(new_dirty_state),
    )
}

/// The C++ `VmPageList::BatchInserter` has the same layout as the Rust `BatchInserter`, so, as for
/// `VmPageList` above, C++ passes its `this` pointer straight through.
#[unsafe(no_mangle)]
extern "C" fn rust_vm_page_list_batch_inserter_lookup_or_allocate(
    inserter: &mut BatchInserter<'_>,
    offset: u64,
) -> *mut VmPageOrMarker {
    inserter.lookup_or_allocate(offset).map_or(core::ptr::null_mut(), core::ptr::from_mut)
}

#[cfg(ktest)]
#[unittest::suite]
/// Unit tests for VmPageOrMarker.
mod vm_page_list_rs {
    use super::{
        ReferenceValue, SentinelType, VmPageListNode, VmPageOrMarker, VmPageOrMarkerRef,
        ZeroRangeDirtyState,
    };
    use unittest::{expect_eq, expect_false, expect_true};

    /// Tests empty state creation and predicate checks.
    #[test]
    fn test_page_or_marker_empty() {
        let pm = VmPageOrMarker::empty();
        expect_true!(pm.is_empty());
        expect_false!(pm.is_page());
        expect_false!(pm.is_reference());
        expect_false!(pm.is_page_or_ref());
    }

    /// Tests release behavior on an empty VmPageOrMarker.
    #[test]
    fn test_page_or_marker_release() {
        let mut pm = VmPageOrMarker::empty();
        let raw = pm.release();
        expect_eq!(raw, VmPageOrMarker::PAGE_TYPE);
        expect_true!(pm.is_empty());
    }

    /// Tests swapping two empty VmPageOrMarker instances.
    #[test]
    fn test_page_or_marker_swap() {
        let mut pm1 = VmPageOrMarker::empty();
        let pm2 = VmPageOrMarker::empty();
        let prev = pm1.swap(pm2);
        expect_true!(prev.is_empty());
        expect_true!(pm1.is_empty());
    }

    /// Tests reference value creation, alignment mask, and getter properties.
    #[test]
    fn test_reference_value() {
        expect_eq!(ReferenceValue::ALIGN_BITS, 3);
        expect_eq!(ReferenceValue::ALIGN_MASK, 0b111);

        let ref_zero = ReferenceValue::new(0x0);
        expect_eq!(ref_zero.value(), 0x0);

        let ref_aligned = ReferenceValue::new(0x1000);
        expect_eq!(ref_aligned.value(), 0x1000);

        let ref_max = ReferenceValue::new(0xFFFFFFF8);
        expect_eq!(ref_max.value(), 0xFFFFFFF8);
    }

    /// Tests reference packing in VmPageOrMarker, predicates, and release behavior.
    #[test]
    fn test_page_or_marker_reference() {
        let ref_val = ReferenceValue::new(0x1000); // 8-byte aligned
        let mut pm = VmPageOrMarker::from_reference(ref_val);
        expect_true!(pm.is_reference());
        expect_false!(pm.is_page());
        expect_false!(pm.is_empty());
        expect_true!(pm.is_page_or_ref());

        expect_true!(pm.reference() == ref_val);
        expect_eq!(pm.reference().value(), ref_val.value());

        let released_ref = pm.release_reference();
        expect_true!(released_ref == ref_val);
        expect_true!(pm.is_empty());
    }

    /// Tests swapping references and swap_reference_for_reference method.
    #[test]
    fn test_page_or_marker_reference_swap() {
        let ref_val1 = ReferenceValue::new(0x1000);
        let ref_val2 = ReferenceValue::new(0x2000);
        let mut pm1 = VmPageOrMarker::from_reference(ref_val1);
        let pm2 = VmPageOrMarker::from_reference(ref_val2);

        // Test general swap of two reference instances
        let mut old = pm1.swap(pm2);
        expect_true!(old.reference() == ref_val1);
        expect_true!(pm1.reference() == ref_val2);

        // Test swap_reference_for_reference method directly
        let ref_val3 = ReferenceValue::new(0x3000);
        let old_ref = pm1.swap_reference_for_reference(ref_val3);
        expect_true!(old_ref == ref_val2);
        expect_true!(pm1.reference() == ref_val3);

        let _ = old.release_reference();
        let _ = pm1.release_reference();
    }

    /// Tests marker state creation and share count modifications.
    #[test]
    fn test_page_or_marker_marker() {
        let mut pm = VmPageOrMarker::marker();
        expect_true!(pm.is_marker());
        expect_false!(pm.is_page());
        expect_false!(pm.is_reference());
        expect_false!(pm.is_parent_content());
        expect_eq!(pm.marker_share_count(), 0);

        pm.increment_marker_share_count();
        expect_eq!(pm.marker_share_count(), 1);

        pm.increment_marker_share_count();
        expect_eq!(pm.marker_share_count(), 2);

        pm.decrement_marker_share_count();
        expect_eq!(pm.marker_share_count(), 1);

        pm.set_marker_share_count(100);
        expect_eq!(pm.marker_share_count(), 100);

        let pm_shared = VmPageOrMarker::marker_with_share_count(5);
        expect_true!(pm_shared.is_marker());
        expect_eq!(pm_shared.marker_share_count(), 5);
    }

    /// Tests parent content state properties.
    #[test]
    fn test_page_or_marker_parent_content() {
        let pm = VmPageOrMarker::parent_content();
        expect_true!(pm.is_parent_content());
        expect_false!(pm.is_page());
        expect_false!(pm.is_reference());
        expect_false!(pm.is_marker());
    }

    /// Tests ZeroRange basic value initialization and boundary limits.
    #[test]
    fn test_zero_range_basic() {
        let zr = ZeroRange::new(0);
        expect_eq!(zr.value(), 0);
        expect_eq!(zr.dirty_state(), ZeroRangeDirtyState::Untracked);
        expect_eq!(zr.awaiting_clean_length(), 0);

        let zr2 = ZeroRange::new(0x80); // 128 (aligned to 7 bits)
        expect_eq!(zr2.value(), 0x80);
    }

    /// Tests ZeroRange dirty state transitions.
    #[test]
    fn test_zero_range_dirty_state() {
        let mut zr = ZeroRange::new(0);
        expect_eq!(zr.dirty_state(), ZeroRangeDirtyState::Untracked);

        zr.set_dirty_state(ZeroRangeDirtyState::Dirty);
        expect_eq!(zr.dirty_state(), ZeroRangeDirtyState::Dirty);

        zr.set_dirty_state(ZeroRangeDirtyState::Untracked);
        expect_eq!(zr.dirty_state(), ZeroRangeDirtyState::Untracked);
    }

    /// Tests ZeroRange awaiting clean length encoding.
    #[test]
    fn test_zero_range_awaiting_clean_length() {
        let mut zr = ZeroRange::new(0);
        // Can only set length if state is Dirty.
        zr.set_dirty_state(ZeroRangeDirtyState::Dirty);
        expect_eq!(zr.awaiting_clean_length(), 0);

        // awaiting_clean_length is page aligned (4096).
        zr.set_awaiting_clean_length(4096);
        expect_eq!(zr.awaiting_clean_length(), 4096);

        zr.set_awaiting_clean_length(8192);
        expect_eq!(zr.awaiting_clean_length(), 8192);

        zr.set_awaiting_clean_length(0);
        expect_eq!(zr.awaiting_clean_length(), 0);
    }

    /// Tests interval creation, properties, sentinel transitions, and dirty states.
    #[test]
    fn test_page_or_marker_interval() {
        let mut pm =
            VmPageOrMarker::zero_interval(SentinelType::Slot, ZeroRangeDirtyState::Untracked);
        expect_true!(pm.is_interval());
        expect_true!(pm.is_interval_zero());
        expect_true!(pm.is_interval_slot());
        expect_false!(pm.is_interval_start());
        expect_false!(pm.is_interval_end());
        expect_true!(pm.is_zero_interval_untracked());
        expect_false!(pm.is_zero_interval_clean());
        expect_false!(pm.is_zero_interval_dirty());

        pm.change_interval_sentinel(SentinelType::Start);
        expect_true!(pm.is_interval_start());
        expect_false!(pm.is_interval_slot());

        let mut pm_dirty =
            VmPageOrMarker::zero_interval(SentinelType::Start, ZeroRangeDirtyState::Dirty);
        expect_true!(pm_dirty.is_zero_interval_dirty());
        expect_eq!(pm_dirty.zero_interval_awaiting_clean_length(), 0);

        pm_dirty.set_zero_interval_awaiting_clean_length(8192);
        expect_eq!(pm_dirty.zero_interval_awaiting_clean_length(), 8192);
    }

    /// Tests VmPageOrMarkerRef deref, mutations, and swap_content.
    #[test]
    fn test_page_or_marker_ref() {
        let mut marker = VmPageOrMarker::marker();
        {
            let mut marker_ref = VmPageOrMarkerRef::new(&mut marker);
            expect_true!(marker_ref.is_marker());
            marker_ref.increment_marker_share_count();
            expect_eq!(marker_ref.marker_share_count(), 1);
            marker_ref.decrement_marker_share_count();
            expect_eq!(marker_ref.marker_share_count(), 0);
        }

        let mut slot = VmPageOrMarker::from_reference(ReferenceValue::new(0x1000));
        {
            let mut slot_ref = VmPageOrMarkerRef::new(&mut slot);
            let prev_ref = slot_ref.swap_reference_for_reference(ReferenceValue::new(0x2000));
            expect_eq!(prev_ref.value(), 0x1000);
            expect_eq!(slot_ref.reference().value(), 0x2000);

            let mut prev = slot_ref.swap_content(VmPageOrMarker::marker());
            expect_true!(prev.is_reference());
            expect_true!(slot_ref.is_marker());
            let _ = prev.release_reference();
        }

        let mut dirty_zero =
            VmPageOrMarker::zero_interval(SentinelType::Start, ZeroRangeDirtyState::Dirty);
        {
            let mut dirty_ref = VmPageOrMarkerRef::new(&mut dirty_zero);
            dirty_ref.set_zero_interval_awaiting_clean_length(8192);
            expect_eq!(dirty_ref.zero_interval_awaiting_clean_length(), 8192);
        }
    }

    /// Tests VmPageListNode layout size, offset/index math, and slot lookups.
    #[test]
    fn test_node_basic() {
        let mut node = VmPageListNode::new();
        expect_eq!(core::mem::size_of::<VmPageListNode>(), 64);
        expect_eq!(VmPageListNode::PAGE_FAN_OUT, 16);
        expect_eq!(VmPageListNode::NODE_SPAN_BYTES, 65536);
        expect_true!(node.is_empty());
        expect_true!(node.has_no_page_or_ref());
        expect_true!(node.has_no_page_ref_or_marker());
        expect_true!(node.has_no_interval_sentinel());

        expect_eq!(VmPageListNode::node_offset(4096), 0);
        expect_eq!(VmPageListNode::node_offset(65536), 65536);
        expect_eq!(VmPageListNode::node_index(4096), 1);
        expect_eq!(VmPageListNode::end_offset(0), 65536);

        *node.lookup_mut(3) = VmPageOrMarker::marker();
        expect_false!(node.is_empty());
        expect_true!(node.has_no_page_or_ref());
        expect_false!(node.has_no_page_ref_or_marker());
        expect_true!(node.has_no_interval_sentinel());
        expect_true!(node.lookup(3).is_marker());
        *node.lookup_mut(3) = VmPageOrMarker::empty();
        expect_true!(node.is_empty());
    }

    /// Tests in-node interval queries.
    #[test]
    fn test_node_interval_queries() {
        let mut node = VmPageListNode::new();
        *node.lookup_mut(3) =
            VmPageOrMarker::zero_interval(SentinelType::End, ZeroRangeDirtyState::Untracked);
        expect_true!(node.node_starts_in_interval().is_some());
        // slot 1 (before end)
        expect_true!(node.is_offset_in_interval(0, 4096).is_some());
        // slot 4 (after end)
        expect_true!(node.is_offset_in_interval(0, 16384).is_none());

        *node.lookup_mut(3) = VmPageOrMarker::empty();
    }

    /// Tests in-node range iteration and merge_range_onto helper.
    #[test]
    fn test_node_range_helpers() {
        let mut node1 = VmPageListNode::new();
        let mut node2 = VmPageListNode::new();
        *node1.lookup_mut(1) = VmPageOrMarker::marker();

        let mut visited_offset = 0;
        let mut count = 0;
        let _ = node1.for_every_page(0, |_slot, offset| {
            visited_offset = offset;
            count += 1;
            Status::NEXT
        });
        expect_eq!(count, 1);
        expect_eq!(visited_offset, 4096);

        let mut ref_count = 0;
        let mut ref_offset = 0;
        let mut is_marker = false;
        let mut share_count = 0;
        let _ = node1.for_every_page_ref(0, |mut slot_ref, offset| {
            ref_offset = offset;
            is_marker = slot_ref.is_marker();
            slot_ref.increment_marker_share_count();
            share_count = slot_ref.marker_share_count();
            ref_count += 1;
            Status::NEXT
        });
        expect_eq!(ref_count, 1);
        expect_eq!(ref_offset, 4096);
        expect_true!(is_marker);
        expect_eq!(share_count, 1);
        expect_eq!(node1.lookup(1).marker_share_count(), 1);

        node1.merge_range_onto(
            0,
            65536,
            |src, dst, _off| *dst = src.swap(VmPageOrMarker::empty()),
            &mut node2,
            0,
            65536,
            65536,
        );
        expect_true!(node1.is_empty());
        expect_true!(node2.lookup(1).is_marker());
        *node2.lookup_mut(1) = VmPageOrMarker::empty();
    }
}
