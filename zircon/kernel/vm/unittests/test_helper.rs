// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::page::{VmPagePtr, vm_page_t};
use crate::vm::vm_object_paged::VmObjectPaged;
use fbl::RefPtr;
use test_helper_bindings as bindings;
use zx_status::Status;

/// Creates a committed pager-backed VMO with `N` pages, returning `(vmo, initialized_pages_array)`.
pub fn make_committed_pager_vmo<const N: usize>(
    trap_dirty: bool,
    resizable: bool,
) -> Result<(RefPtr<VmObjectPaged>, [VmPagePtr; N]), Status> {
    let mut raw_vmo = core::ptr::null_mut();
    let mut page_ptrs: [*mut vm_page_t; N] = [core::ptr::null_mut(); N];

    // SAFETY: page_ptrs.as_mut_ptr() is valid for writing N pointers, and raw_vmo is a valid out-pointer.
    let status = unsafe {
        bindings::cpp_make_committed_pager_vmo(
            page_ptrs.len(),
            trap_dirty,
            resizable,
            page_ptrs.as_mut_ptr(),
            &mut raw_vmo,
        )
    };
    Status::ok(status)?;

    // SAFETY: When cpp_make_committed_pager_vmo returns ZX_OK, raw_vmo is a valid exported
    // VmObjectPaged pointer.
    let vmo = unsafe { VmObjectPaged::from_raw(raw_vmo) };
    // Based on cpp_make_committed_pager_vmo returning ZX_OK, raw_vmo is guaranteed to be non-null
    // and valid.
    let vmo = vmo.unwrap();

    let pages = page_ptrs.map(|ptr| {
        // SAFETY: When cpp_make_committed_pager_vmo returns ZX_OK, ptr is a valid pointer to a
        //kernel page.
        let ptr = unsafe { VmPagePtr::from_raw(ptr) };
        // Based on cpp_make_committed_pager_vmo returning ZX_OK, all page pointers are guaranteed
        // to be non-null and available.
        ptr.unwrap()
    });

    Ok((vmo, pages))
}

/// Creates a partially committed pager-backed VMO with `num_pages` virtual
/// pages and `C` committed pages.
pub fn make_partially_committed_pager_vmo<const C: usize>(
    num_pages: usize,
    trap_dirty: bool,
    resizable: bool,
    ignore_requests: bool,
) -> Result<(RefPtr<VmObjectPaged>, [VmPagePtr; C]), Status> {
    assert!(C <= num_pages, "committed_pages ({C}) cannot exceed num_pages ({num_pages})");
    let mut raw_vmo = core::ptr::null_mut();
    let mut page_ptrs: [*mut vm_page_t; C] = [core::ptr::null_mut(); C];

    // SAFETY: The page pointer array and output VMO reference are valid for
    // their respective writes.
    let status = unsafe {
        bindings::cpp_make_partially_committed_pager_vmo(
            num_pages,
            C,
            trap_dirty,
            resizable,
            ignore_requests,
            page_ptrs.as_mut_ptr(),
            &mut raw_vmo,
        )
    };
    Status::ok(status)?;

    // SAFETY: When cpp_make_partially_committed_pager_vmo returns ZX_OK, raw_vmo is a valid
    // exported VmObjectPaged pointer.
    let vmo = unsafe { VmObjectPaged::from_raw(raw_vmo) }.expect("vmo pointer is non-null");
    let pages = page_ptrs.map(|ptr| {
        // SAFETY: When cpp_make_partially_committed_pager_vmo returns ZX_OK, ptr is a valid
        // pointer to a page.
        unsafe { VmPagePtr::from_raw(ptr) }.expect("page pointer is non-null")
    });

    Ok((vmo, pages))
}
