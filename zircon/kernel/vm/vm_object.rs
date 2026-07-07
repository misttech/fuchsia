// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::{PhantomData, PhantomPinned};
use core::ptr::NonNull;
use kalloc::AllocError;

#[repr(C)]
/// The base vm object that holds a range of bytes of data
///
/// Can be created without mapping and used as a container of data, or mappable
/// into an address space via VmAddressRegion::CreateVmMapping
pub struct VmObject {
    _data: [u8; 0],
    _marker: PhantomData<PhantomPinned>,
}

unsafe extern "C" {
    fn cpp_vm_object_get_ref_counted(vmo: *const VmObject) -> *mut fbl::RefCounted;
    fn cpp_vm_object_free(vmo: *mut VmObject);
}

impl fbl::HasRefCount for VmObject {
    fn ref_count(&self) -> &fbl::RefCounted {
        unsafe { &*cpp_vm_object_get_ref_counted(self as *const _) }
    }
}

unsafe impl fbl::Recyclable for VmObject {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_vm_object_free(ptr.as_ptr());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}
