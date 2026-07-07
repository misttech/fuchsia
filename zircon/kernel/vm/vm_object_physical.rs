// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::vm_object::VmObject;
use core::marker::{PhantomData, PhantomPinned};
use core::ptr::NonNull;
use fbl::RefPtr;
use kalloc::AllocError;
use kernel::types::PAddr;
use zx_status::Status;

#[repr(C)]
/// VMO representing a physical range of memory
pub struct VmObjectPhysical {
    _data: [u8; 0],
    _marker: PhantomData<PhantomPinned>,
}

unsafe extern "C" {
    fn cpp_vm_object_physical_get_ref_counted(vmo: *const VmObjectPhysical)
    -> *mut fbl::RefCounted;
    fn cpp_vm_object_physical_free(vmo: *mut VmObjectPhysical);
    fn cpp_vm_object_physical_create(
        base: PAddr,
        size: usize,
        out_status: *mut i32,
    ) -> *mut VmObjectPhysical;
    fn cpp_vm_object_physical_as_vm_object(vmo: *mut VmObjectPhysical) -> *mut VmObject;
}

impl VmObjectPhysical {
    /// Create a new physical VMO for the given physical region.
    pub fn create(base: PAddr, size: usize) -> Result<RefPtr<VmObjectPhysical>, Status> {
        let mut status = 0;
        let raw = unsafe { cpp_vm_object_physical_create(base, size, &mut status) };
        Status::ok(status)?;
        unsafe { RefPtr::try_from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Cast a pointer to a VmObjectPhysical to its base VmObject.
    pub fn cast(vmo: NonNull<VmObjectPhysical>) -> NonNull<VmObject> {
        unsafe { NonNull::new_unchecked(cpp_vm_object_physical_as_vm_object(vmo.as_ptr())) }
    }
}

impl fbl::HasRefCount for VmObjectPhysical {
    fn ref_count(&self) -> &fbl::RefCounted {
        unsafe { &*cpp_vm_object_physical_get_ref_counted(self as *const _) }
    }
}

unsafe impl fbl::Recyclable for VmObjectPhysical {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_vm_object_physical_free(ptr.as_ptr());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}
