// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::PAddr;
use crate::vm::page::VmPagePtr;
use core::marker::{PhantomData, PhantomPinned};
use pmm_node_bindings as bindings;
use zr::Opaque;

pub use bindings::PmmOptDelayReuse;

/// Facade for physical memory manager operations.
#[repr(C)]
pub struct PmmNode {
    raw: Opaque<bindings::PmmNode>,
    phantom: PhantomData<PhantomPinned>,
}

unsafe impl Sync for PmmNode {}
unsafe impl Send for PmmNode {}

impl PmmNode {
    /// Domain-specific conversion: returns raw pointer for `PmmNode`.
    pub fn as_raw(&self) -> *mut bindings::PmmNode {
        self.raw.get()
    }

    /// Converts a page index back to a `VmPagePtr`.
    ///
    /// # Safety
    ///
    /// The `index` must be a valid PMM page index.
    pub unsafe fn index_to_page(&self, index: u32) -> Option<VmPagePtr> {
        // SAFETY: The caller guarantees `index` is a valid page index.
        let ptr = unsafe { bindings::cpp_pmm_node_index_to_page(self.as_raw(), index) };
        // SAFETY: `ptr` is guaranteed to be a valid pointer to a kernel page if it is not null
        // because `index` was valid.
        unsafe { VmPagePtr::from_ffi(ptr) }
    }

    /// Converts a `VmPagePtr` to a page index.
    pub fn page_to_index(&self, page: VmPagePtr) -> u32 {
        // SAFETY: `page.as_raw()` is guaranteed to be a valid pointer to a kernel page.
        unsafe { bindings::cpp_pmm_node_page_to_index(self.as_raw(), page.as_ffi()) }
    }

    /// Converts a page index to a physical address.
    ///
    /// # Safety
    ///
    /// The `index` must be a valid PMM page index.
    pub unsafe fn index_to_paddr(&self, index: u32) -> PAddr {
        // SAFETY: The caller guarantees `index` is a valid page index.
        unsafe { PAddr(bindings::cpp_pmm_node_index_to_paddr(self.as_raw(), index)) }
    }
}
