// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::PAddr;
use crate::vm::page::VmPagePtr;
use crate::vm::pmm_node::pmm_node;
use vm_constants_rs::{
    kPmmNodeIndexZeroBits, kVmPageListIntervalBits, kVmPageListPageType,
    kVmPageListParentContentType, kVmPageListReferenceType, kVmPageListTypeBits,
    kVmPageListZeroMarkerType,
};

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
///
/// Note on Preconditions & Safety Contracts:
/// `VmPageOrMarker` uses manual bit-packing rather than a native Rust `enum` to maintain
/// C++ memory layout parity and zero-overhead performance. Accessors use `debug_assert!`
/// to validate variant preconditions in debug builds, matching C++ `DEBUG_ASSERT` behavior.
#[repr(transparent)]
pub struct VmPageOrMarker {
    raw: u32,
}

/// The various dirty states that a zero interval can be in. Refer to VmCowPages::DirtyState for
/// an explanation of the states. Note that an AwaitingClean state is not encoded in the interval
/// state bits. This information is instead stored using the AwaitingCleanLength for convenience,
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
        match state_bits {
            0 => ZeroRangeDirtyState::Untracked,
            1 => ZeroRangeDirtyState::Clean,
            2 => ZeroRangeDirtyState::Dirty,
            _ => panic!("invalid dirty state"),
        }
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
    const PARENT_CONTENT_TYPE: u32 = kVmPageListParentContentType;

    const MARKER_SHARE_COUNT_MASK: u32 = !Self::TYPE_MASK;
    const MARKER_SHARE_COUNT_STEP: u32 = 1 << Self::TYPE_BITS;

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

    /// Returns the underlying page. Is only valid to call if `is_page` is true.
    ///
    /// Do not need to mask any bits out of raw, since PAGE_TYPE has 0's for the type anyway.
    pub fn page(&self) -> VmPagePtr {
        debug_assert!(self.is_page());
        // SAFETY: `self.raw` is guaranteed to be a valid page index when `self` is a page.
        unsafe {
            pmm_node().index_to_page(self.raw).expect("VmPageOrMarker contains invalid page index")
        }
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
        unsafe {
            pmm_node()
                .index_to_page(raw)
                .expect("VmPageOrMarker release contains invalid page index")
        }
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

#[cfg(ktest)]
#[unittest::suite]
/// Unit tests for VmPageOrMarker.
mod vm_page_list_rs {
    use super::{ReferenceValue, VmPageOrMarker};
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

        // AwaitingCleanLength is page aligned (4096).
        zr.set_awaiting_clean_length(4096);
        expect_eq!(zr.awaiting_clean_length(), 4096);

        zr.set_awaiting_clean_length(8192);
        expect_eq!(zr.awaiting_clean_length(), 8192);

        zr.set_awaiting_clean_length(0);
        expect_eq!(zr.awaiting_clean_length(), 0);
    }
}
