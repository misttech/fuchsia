// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::PAddr;
use crate::vm::page::VmPagePtr;
use pmm_bindings as bindings;

/// Facade for physical memory manager operations.
///
/// `PmmNode` is currently a zero-sized facade struct. Its methods delegate directly
/// to global C++ PMM shims (`bindings::cpp_pmm_*`). When the PMM is migrated to Rust,
/// `PmmNode` will be updated internally without requiring changes to callers across the kernel.
pub struct PmmNode;

impl PmmNode {
    /// Converts a page index back to a `VmPagePtr`.
    ///
    /// # Safety
    ///
    /// The `index` must be a valid PMM page index.
    pub unsafe fn index_to_page(&self, index: u32) -> Option<VmPagePtr> {
        // SAFETY: The caller guarantees `index` is a valid page index.
        let ptr = unsafe { bindings::cpp_pmm_index_to_page(index) };
        // SAFETY: `ptr` is guaranteed to be a valid pointer to a kernel page if it is not null
        // because `index` was valid.
        unsafe { VmPagePtr::from_ffi(ptr) }
    }

    /// Converts a `VmPagePtr` to a page index.
    pub fn page_to_index(&self, page: VmPagePtr) -> u32 {
        // SAFETY: `page.as_raw()` is guaranteed to be a valid pointer to a kernel page.
        unsafe { bindings::cpp_pmm_page_to_index(page.as_ffi()) }
    }

    /// Converts a page index to a physical address.
    ///
    /// # Safety
    ///
    /// The `index` must be a valid PMM page index.
    pub unsafe fn index_to_paddr(&self, index: u32) -> PAddr {
        // SAFETY: The caller guarantees `index` is a valid page index.
        unsafe { PAddr(bindings::cpp_pmm_index_to_paddr(index)) }
    }
}

/// Returns a reference to the global `PmmNode` instance.
///
/// Note: As `PmmNode` is currently a zero-sized type, this returns a promoted
/// static reference (`&'static PmmNode`) that delegates to global C++ PMM FFI shims.
pub fn pmm_node() -> &'static PmmNode {
    &PmmNode
}
