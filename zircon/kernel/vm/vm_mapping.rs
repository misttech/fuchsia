// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::arch_vm_aspace::ArchMmuFlags;
use crate::vm_object::VmObject;
use core::marker::{PhantomData, PhantomPinned};
use core::ptr::NonNull;
use fbl::RefPtr;
use kalloc::AllocError;
use zr::ToMutPtr;
use zx_status::Status;

unsafe extern "C" {
    fn cpp_vm_mapping_get_ref_counted(mapping: *mut VmMapping) -> *mut fbl::RefCounted;
    fn cpp_vm_mapping_free(mapping: *mut VmMapping);
    fn cpp_vm_mapping_destroy(mapping: *mut VmMapping) -> i32;
    fn cpp_vm_mapping_base(mapping: *mut VmMapping) -> usize;
    fn cpp_vm_mapping_size(mapping: *mut VmMapping) -> usize;
    fn cpp_vm_mapping_flags(mapping: *mut VmMapping) -> u32;
    fn cpp_vm_mapping_object_offset(mapping: *mut VmMapping) -> u64;
    fn cpp_vm_mapping_decommit_range(mapping: *mut VmMapping, offset: usize, len: usize) -> i32;
    fn cpp_vm_mapping_map_range(
        mapping: *mut VmMapping,
        offset: usize,
        len: usize,
        commit: bool,
        ignore_existing: bool,
    ) -> i32;
    fn cpp_vm_mapping_debug_unmap(mapping: *mut VmMapping, base: usize, size: usize) -> i32;
    fn cpp_vm_mapping_debug_protect(
        mapping: *mut VmMapping,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> i32;
    fn cpp_vm_mapping_vmo(mapping: *mut VmMapping) -> *const VmObject;
}

/// A leaf mapping that maps a VMO into the address space.
///
/// This is an opaque FFI wrapper around Zircon's C++ `VmMapping` class.
#[repr(C)]
pub struct VmMapping {
    _data: [u8; 0],
    _marker: PhantomData<PhantomPinned>,
}

impl VmMapping {
    /// Destroys this mapping, unmapping all pages and removing dependencies on the underlying VMO.
    pub fn destroy(&self) -> Result<(), Status> {
        Status::ok(unsafe { cpp_vm_mapping_destroy(self.to_mut_ptr()) })
    }

    /// Returns the base virtual address of this mapping.
    pub fn base(&self) -> usize {
        unsafe { cpp_vm_mapping_base(self.to_mut_ptr()) }
    }

    /// Returns the size in bytes of this mapping.
    pub fn size(&self) -> usize {
        unsafe { cpp_vm_mapping_size(self.to_mut_ptr()) }
    }

    /// Returns the creation flags of this mapping.
    pub fn flags(&self) -> u32 {
        unsafe { cpp_vm_mapping_flags(self.to_mut_ptr()) }
    }

    /// Returns the offset into the underlying VMO for this mapping.
    pub fn object_offset(&self) -> u64 {
        unsafe { cpp_vm_mapping_object_offset(self.to_mut_ptr()) }
    }

    /// Convenience wrapper for vmo()->DecommitRange() with the necessary
    /// offset modification and locking.
    pub fn decommit_range(&self, offset: usize, len: usize) -> Result<(), Status> {
        Status::ok(unsafe { cpp_vm_mapping_decommit_range(self.to_mut_ptr(), offset, len) })
    }

    /// Map in pages from the underlying vm object, optionally committing pages as it goes.
    /// |ignore_existing| controls whether existing hardware mappings in the specified range should
    /// be ignored or treated as an error. |ignore_existing| should only be set to true for user
    /// mappings where populating mappings may already be racy with multiple threads, and where we
    /// are already tolerant of mappings being arbitrarily created and destroyed.
    pub fn map_range(
        &self,
        offset: usize,
        len: usize,
        commit: bool,
        ignore_existing: bool,
    ) -> Result<(), Status> {
        Status::ok(unsafe {
            cpp_vm_mapping_map_range(self.to_mut_ptr(), offset, len, commit, ignore_existing)
        })
    }

    /// Unlocked convenience wrapper around unmap for testing.
    pub fn debug_unmap(&self, base: usize, size: usize) -> Result<(), Status> {
        Status::ok(unsafe { cpp_vm_mapping_debug_unmap(self.to_mut_ptr(), base, size) })
    }

    /// Unlocked convenience wrapper around protect for testing.
    pub fn debug_protect(
        &self,
        base: usize,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        Status::ok(unsafe {
            cpp_vm_mapping_debug_protect(self.to_mut_ptr(), base, size, new_arch_mmu_flags)
        })
    }

    /// Returns the underlying VMO backing this mapping.
    pub fn vmo(&self) -> Option<RefPtr<VmObject>> {
        unsafe { RefPtr::try_from_raw(cpp_vm_mapping_vmo(self.to_mut_ptr())) }
    }
}

impl fbl::HasRefCount for VmMapping {
    fn ref_count(&self) -> &fbl::RefCounted {
        unsafe { &*cpp_vm_mapping_get_ref_counted(self.to_mut_ptr()) }
    }
}

unsafe impl fbl::Recyclable for VmMapping {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_vm_mapping_free(ptr.as_ptr());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}
