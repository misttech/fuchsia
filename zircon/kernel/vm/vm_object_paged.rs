// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::vm_object::VmObject;
use core::marker::{PhantomData, PhantomPinned};
use core::ptr::NonNull;
use fbl::RefPtr;
use kalloc::AllocError;
use zx_status::Status;

#[repr(C)]
/// VMO representing a paged range of copy-on-write memory.
pub struct VmObjectPaged {
    _data: [u8; 0],
    _marker: PhantomData<PhantomPinned>,
}

unsafe extern "C" {
    fn cpp_vm_object_paged_get_ref_counted(vmo: *const VmObjectPaged) -> *mut fbl::RefCounted;
    fn cpp_vm_object_paged_free(vmo: *mut VmObjectPaged);
    fn cpp_vm_object_paged_create(
        pmm_alloc_flags: u32,
        options: u32,
        size: u64,
        out_status: *mut i32,
    ) -> *mut VmObjectPaged;
    fn cpp_vm_object_paged_create_contiguous(
        pmm_alloc_flags: u32,
        size: u64,
        alignment_log2: u8,
        out_status: *mut i32,
    ) -> *mut VmObjectPaged;
    fn cpp_vm_object_paged_as_vm_object(vmo: *mut VmObjectPaged) -> *mut VmObject;
}

impl VmObjectPaged {
    /// Create a new paged VMO.
    pub fn create(
        pmm_alloc_flags: u32,
        options: u32,
        size: u64,
    ) -> Result<RefPtr<VmObjectPaged>, Status> {
        let mut status = 0;
        let raw =
            unsafe { cpp_vm_object_paged_create(pmm_alloc_flags, options, size, &mut status) };
        Status::ok(status)?;
        unsafe { RefPtr::try_from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Create a contiguous paged VMO.
    pub fn create_contiguous(
        pmm_alloc_flags: u32,
        size: u64,
        alignment_log2: u8,
    ) -> Result<RefPtr<VmObjectPaged>, Status> {
        let mut status = 0;
        let raw = unsafe {
            cpp_vm_object_paged_create_contiguous(
                pmm_alloc_flags,
                size,
                alignment_log2,
                &mut status,
            )
        };
        Status::ok(status)?;
        unsafe { RefPtr::try_from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Cast a pointer to a VmObjectPaged to its base VmObject.
    pub fn cast(vmo: NonNull<VmObjectPaged>) -> NonNull<VmObject> {
        unsafe { NonNull::new_unchecked(cpp_vm_object_paged_as_vm_object(vmo.as_ptr())) }
    }
}

impl fbl::HasRefCount for VmObjectPaged {
    fn ref_count(&self) -> &fbl::RefCounted {
        unsafe { &*cpp_vm_object_paged_get_ref_counted(self as *const _) }
    }
}

unsafe impl fbl::Recyclable for VmObjectPaged {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_vm_object_paged_free(ptr.as_ptr());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}
