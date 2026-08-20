// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::PAddr;
use crate::vm::page::VmPagePtr;
use crate::vm::pmm_node::pmm_node;
use vm_constants_rs::{kPmmNodeIndexZeroBits, kVmPageListPageType, kVmPageListTypeBits};

/// RAII helper for representing content in a page list node. This supports being in one of these
/// states:
///  * Empty       - Contains nothing.
///  * Page p      - Contains a `VmPagePtr` 'p'. This 'p' is considered owned by this wrapper and
///    `release_page` must be called to give up ownership.
///
/// Note on Preconditions & Safety Contracts:
/// `VmPageOrMarker` uses manual bit-packing rather than a native Rust `enum` to maintain
/// C++ memory layout parity and zero-overhead performance. Accessors use `debug_assert!`
/// to validate variant preconditions in debug builds, matching C++ `DEBUG_ASSERT` behavior.
#[repr(transparent)]
pub struct VmPageOrMarker {
    raw: u32,
}

impl VmPageOrMarker {
    // The low 3 bits of raw are reserved to represent the type, any other data has to fit into
    // the remaining high bits. Note that there is no explicit Empty type, rather a PAGE_TYPE with a
    // zero pointer is used to represent Empty.
    const TYPE_BITS: u32 = kVmPageListTypeBits;
    const TYPE_MASK: u32 = (1 << Self::TYPE_BITS) - 1;
    const PAGE_TYPE: u32 = kVmPageListPageType;

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

    /// Swaps content with another `VmPageOrMarker`, returning the previous value of `self`.
    pub fn swap(&mut self, mut other: Self) -> Self {
        let ret = self.raw;
        self.raw = other.release();
        Self { raw: ret }
    }
}

impl Drop for VmPageOrMarker {
    fn drop(&mut self) {
        debug_assert!(!self.is_page(), "VmPageOrMarker dropped while containing page");
    }
}

#[cfg(ktest)]
#[unittest::suite]
/// Unit tests for VmPageOrMarker.
mod vm_page_list_rs {
    use super::VmPageOrMarker;
    use unittest::{expect_eq, expect_false, expect_true};

    /// Tests empty state creation and predicate checks.
    #[test]
    fn test_page_or_marker_empty() {
        let pm = VmPageOrMarker::empty();
        expect_true!(pm.is_empty());
        expect_false!(pm.is_page());
    }

    /// Tests release behavior on an empty VmPageOrMarker.
    #[test]
    fn test_page_or_marker_release() {
        let mut pm = VmPageOrMarker::empty();
        let raw = pm.release();
        expect_eq!(raw, VmPageOrMarker::PAGE_TYPE);
        expect_true!(pm.is_empty());
    }

    /// Tests swapping two VmPageOrMarker instances.
    #[test]
    fn test_page_or_marker_swap() {
        let mut pm1 = VmPageOrMarker::empty();
        let pm2 = VmPageOrMarker::empty();
        let prev = pm1.swap(pm2);
        expect_true!(prev.is_empty());
        expect_true!(pm1.is_empty());
    }
}
