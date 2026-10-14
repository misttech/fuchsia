// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::VirtualAlloc;
use crate::vm::page_state::{VmPageState, bindings as page_bindings};
use core::mem::MaybeUninit;
use zx_status::Status;
use zx_types::zx_status_t;

// C++ FFI entry points for `VirtualAlloc` in `<lib/virtual_alloc.h>`.

/// Constructs a `VirtualAlloc` in-place at `alloc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_new(
    alloc: &mut MaybeUninit<VirtualAlloc>,
    allocated_page_state: page_bindings::vm_page_state,
) {
    alloc.write(VirtualAlloc::new(VmPageState(allocated_page_state)));
}

/// Drops a `VirtualAlloc` in-place at `alloc`.
///
/// # Safety
///
/// `alloc` must point to a valid, initialized `VirtualAlloc` that has not yet been dropped.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_virtual_alloc_drop(alloc: *mut VirtualAlloc) {
    debug_assert!(!alloc.is_null());
    // SAFETY: Caller guarantees `alloc` is a valid, initialized `VirtualAlloc`.
    unsafe {
        core::ptr::drop_in_place(alloc);
    }
}

/// Initializes `alloc` with the given virtual address range, guard page count, and alignment.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_init(
    alloc: &mut VirtualAlloc,
    base: usize,
    size: usize,
    alloc_guard: usize,
    align_log2: usize,
) -> zx_status_t {
    Status::result_into_raw(alloc.init(base, size, alloc_guard, align_log2))
}

/// Allocates `pages` virtually contiguous pages from `alloc` and writes the virtual address to
/// `out_vaddr`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_alloc_pages(
    alloc: &mut VirtualAlloc,
    pages: usize,
    out_vaddr: &mut usize,
) -> zx_status_t {
    match alloc.alloc_pages(pages) {
        Ok(vaddr) => {
            *out_vaddr = vaddr;
            zx_status::sys::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Frees `pages` starting at `vaddr` back to `alloc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_free_pages(
    alloc: &mut VirtualAlloc,
    vaddr: usize,
    pages: usize,
) {
    alloc.free_pages(vaddr, pages);
}

/// Returns whether `alloc` can attempt contiguous mappings.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_can_attempt_contiguous_mappings(alloc: &VirtualAlloc) -> bool {
    alloc.can_attempt_contiguous_mappings()
}

/// Returns the number of pages used by `alloc`'s tracking bitmap.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_debug_bitmap_pages(alloc: &VirtualAlloc) -> usize {
    alloc.debug_bitmap_pages()
}

/// Frees all outstanding allocations in `alloc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_debug_free_all_allocations(alloc: &mut VirtualAlloc) {
    alloc.debug_free_all_allocations();
}

/// Force-allocates the range `[vaddr, vaddr + pages * PAGE_SIZE)` in `alloc`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_virtual_alloc_debug_allocate_vaddr_range(
    alloc: &mut VirtualAlloc,
    vaddr: usize,
    pages: usize,
) {
    alloc.debug_allocate_vaddr_range(vaddr, pages);
}
