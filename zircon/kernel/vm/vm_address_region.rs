// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::arch_vm_aspace::ArchMmuFlags;
use crate::vm_mapping::VmMapping;
use crate::vm_object::VmObject;
use core::ffi::{CStr, c_char};
use core::marker::{PhantomData, PhantomPinned};
use core::ptr::NonNull;
use fbl::RefPtr;
use kalloc::AllocError;
use kernel::types::VAddr;
use zx_status::Status;

/// When randomly allocating subregions, reduce sprawl by placing allocations near each other.
pub const VMAR_FLAG_COMPACT: u32 = 1 << 0;
/// Request that the new region be at the specified offset in its parent region.
pub const VMAR_FLAG_SPECIFIC: u32 = 1 << 1;
/// Like `VMAR_FLAG_SPECIFIC`, but permits overwriting existing mappings.
pub const VMAR_FLAG_SPECIFIC_OVERWRITE: u32 = 1 << 2;
/// Allow `VmMappings` to be created inside the new region with the `SPECIFIC` or `OFFSET_IS_UPPER_LIMIT` flag.
pub const VMAR_FLAG_CAN_MAP_SPECIFIC: u32 = 1 << 3;
/// Allow `VmMappings` to be created inside the region with read permissions.
pub const VMAR_FLAG_CAN_MAP_READ: u32 = 1 << 4;
/// Allow `VmMappings` to be created inside the region with write permissions.
pub const VMAR_FLAG_CAN_MAP_WRITE: u32 = 1 << 5;
/// Allow `VmMappings` to be created inside the region with execute permissions.
pub const VMAR_FLAG_CAN_MAP_EXECUTE: u32 = 1 << 6;
/// Require that VMO backing the mapping is non-resizable.
pub const VMAR_FLAG_REQUIRE_NON_RESIZABLE: u32 = 1 << 7;
/// Allow VMO backings that could result in faults.
pub const VMAR_FLAG_ALLOW_FAULTS: u32 = 1 << 8;
/// Treat the offset as an upper limit when allocating a VMO or child VMAR.
pub const VMAR_FLAG_OFFSET_IS_UPPER_LIMIT: u32 = 1 << 9;
/// Opt this VMAR out of certain debugging checks.
pub const VMAR_FLAG_DEBUG_DYNAMIC_KERNEL_MAPPING: u32 = 1 << 10;
/// Memory accesses past the stream size rounded up to the page boundary will fault.
pub const VMAR_FLAG_FAULT_BEYOND_STREAM_SIZE: u32 = 1 << 11;

/// Mask of read, write, and execute permission flags.
pub const VMAR_CAN_RWX_FLAGS: u32 =
    VMAR_FLAG_CAN_MAP_READ | VMAR_FLAG_CAN_MAP_WRITE | VMAR_FLAG_CAN_MAP_EXECUTE;

/// Memory priorities that can be applied to VMARs and mappings.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MemoryPriority {
    /// Default overcommit priority where reclamation is allowed.
    Default = 0,
    /// High priority prevents all reclamation.
    High = 1,
}

/// Whether to operate on children when unmapping or protecting.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum VmAddressRegionOpChildren {
    Yes = 0,
    No = 1,
}

// Verify sizes match C++ 1-byte bool enum representations.
const _: () = assert!(core::mem::size_of::<MemoryPriority>() == 1);
const _: () = assert!(core::mem::size_of::<VmAddressRegionOpChildren>() == 1);

/// Result of calling [`VmAddressRegion::create_vm_mapping`].
pub struct MapResult {
    /// The newly created mapping.
    pub mapping: RefPtr<VmMapping>,
    /// The virtual address of the mapping at creation time.
    pub base: usize,
}

unsafe extern "C" {
    fn cpp_vm_address_region_get_ref_counted(vmar: *mut VmAddressRegion) -> *mut fbl::RefCounted;
    fn cpp_vm_address_region_free(vmar: *mut VmAddressRegion);
    fn cpp_vm_address_region_destroy(vmar: *mut VmAddressRegion) -> i32;
    fn cpp_vm_address_region_base(vmar: *mut VmAddressRegion) -> VAddr;
    fn cpp_vm_address_region_size(vmar: *mut VmAddressRegion) -> usize;
    fn cpp_vm_address_region_flags(vmar: *mut VmAddressRegion) -> u32;
    fn cpp_vm_address_region_name(vmar: *mut VmAddressRegion) -> *const c_char;
    fn cpp_vm_address_region_has_parent(vmar: *mut VmAddressRegion) -> bool;
    fn cpp_vm_address_region_set_memory_priority(
        vmar: *mut VmAddressRegion,
        priority: MemoryPriority,
    ) -> i32;
    fn cpp_vm_address_region_unmap(
        vmar: *mut VmAddressRegion,
        base: VAddr,
        size: usize,
        op_children: VmAddressRegionOpChildren,
    ) -> i32;
    fn cpp_vm_address_region_protect(
        vmar: *mut VmAddressRegion,
        base: VAddr,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
        op_children: VmAddressRegionOpChildren,
    ) -> i32;
    fn cpp_vm_address_region_reserve_space(
        vmar: *mut VmAddressRegion,
        name: *const c_char,
        base: usize,
        size: usize,
        arch_mmu_flags: ArchMmuFlags,
    ) -> i32;
    fn cpp_vm_address_region_create_sub_vmar(
        vmar: *mut VmAddressRegion,
        offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        name: *const c_char,
        out_status: *mut i32,
    ) -> *mut VmAddressRegion;
    fn cpp_vm_address_region_create_vm_mapping(
        vmar: *mut VmAddressRegion,
        mapping_offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        vmo: *const VmObject,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        name: *const c_char,
        out_base: *mut usize,
        out_status: *mut i32,
    ) -> *mut VmMapping;
}

/// A contiguous region of the virtual address space.
#[repr(C)]
pub struct VmAddressRegion {
    _data: [u8; 0],
    _marker: PhantomData<PhantomPinned>,
}

impl VmAddressRegion {
    fn as_mut_ptr(&self) -> *mut VmAddressRegion {
        self as *const VmAddressRegion as *mut VmAddressRegion
    }

    /// Creates a subregion of this region.
    pub fn create_sub_vmar(
        &self,
        offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        name: &CStr,
    ) -> Result<RefPtr<VmAddressRegion>, Status> {
        let mut status = 0;
        let raw = unsafe {
            cpp_vm_address_region_create_sub_vmar(
                self.as_mut_ptr(),
                offset,
                size,
                align_pow2,
                vmar_flags,
                name.as_ptr(),
                &mut status,
            )
        };
        Status::ok(status)?;
        unsafe { RefPtr::try_from_raw(raw).ok_or(Status::NO_MEMORY) }
    }

    /// Creates a [`VmMapping`] within this region.
    ///
    /// To avoid leaks, this should be paired with a call to [`VmMapping::destroy`] if desired;
    /// dropping `MapResult::mapping` will not destroy the mapping.
    pub fn create_vm_mapping(
        &self,
        mapping_offset: usize,
        size: usize,
        align_pow2: u8,
        vmar_flags: u32,
        vmo: RefPtr<VmObject>,
        vmo_offset: u64,
        arch_mmu_flags: ArchMmuFlags,
        name: &CStr,
    ) -> Result<MapResult, Status> {
        let mut base = 0;
        let mut status = 0;
        let raw = unsafe {
            cpp_vm_address_region_create_vm_mapping(
                self.as_mut_ptr(),
                mapping_offset,
                size,
                align_pow2,
                vmar_flags,
                RefPtr::into_raw(vmo),
                vmo_offset,
                arch_mmu_flags,
                name.as_ptr(),
                &mut base,
                &mut status,
            )
        };
        Status::ok(status)?;
        let mapping = unsafe { RefPtr::try_from_raw(raw).ok_or(Status::NO_MEMORY)? };
        Ok(MapResult { mapping, base })
    }

    /// Destroys this region and recursively destroys child VMARs.
    pub fn destroy(&self) -> Result<(), Status> {
        Status::ok(unsafe { cpp_vm_address_region_destroy(self.as_mut_ptr()) })
    }

    /// Returns the base address of this region.
    pub fn base(&self) -> VAddr {
        unsafe { cpp_vm_address_region_base(self.as_mut_ptr()) }
    }

    /// Returns the size in bytes of this region.
    pub fn size(&self) -> usize {
        unsafe { cpp_vm_address_region_size(self.as_mut_ptr()) }
    }

    /// Returns the creation flags of this region.
    pub fn flags(&self) -> u32 {
        unsafe { cpp_vm_address_region_flags(self.as_mut_ptr()) }
    }

    /// Returns the name of this region.
    pub fn name(&self) -> &CStr {
        unsafe { CStr::from_ptr(cpp_vm_address_region_name(self.as_mut_ptr())) }
    }

    /// Returns true if this region has a parent region.
    pub fn has_parent(&self) -> bool {
        unsafe { cpp_vm_address_region_has_parent(self.as_mut_ptr()) }
    }

    /// Applies the given memory priority to this region and all subregions.
    pub fn set_memory_priority(&self, priority: MemoryPriority) -> Result<(), Status> {
        Status::ok(unsafe {
            cpp_vm_address_region_set_memory_priority(self.as_mut_ptr(), priority)
        })
    }

    /// Unmaps a subset of the region of memory in the containing address space.
    ///
    /// # Safety
    ///
    /// Caller must ensure the specified virtual address region to unmap is no longer used.
    pub unsafe fn unmap(
        &self,
        base: VAddr,
        size: usize,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        Status::ok(unsafe {
            cpp_vm_address_region_unmap(self.as_mut_ptr(), base, size, op_children)
        })
    }

    /// Changes protections on a subset of the region of memory in the containing address space.
    pub fn protect(
        &self,
        base: VAddr,
        size: usize,
        new_arch_mmu_flags: ArchMmuFlags,
        op_children: VmAddressRegionOpChildren,
    ) -> Result<(), Status> {
        Status::ok(unsafe {
            cpp_vm_address_region_protect(
                self.as_mut_ptr(),
                base,
                size,
                new_arch_mmu_flags,
                op_children,
            )
        })
    }

    /// Reserves a memory region within this VMAR without allocating physical pages.
    pub fn reserve_space(
        &self,
        name: &CStr,
        base: usize,
        size: usize,
        arch_mmu_flags: ArchMmuFlags,
    ) -> Result<(), Status> {
        Status::ok(unsafe {
            cpp_vm_address_region_reserve_space(
                self.as_mut_ptr(),
                name.as_ptr(),
                base,
                size,
                arch_mmu_flags,
            )
        })
    }
}

impl fbl::HasRefCount for VmAddressRegion {
    fn ref_count(&self) -> &fbl::RefCounted {
        unsafe { &*cpp_vm_address_region_get_ref_counted(self.as_mut_ptr()) }
    }
}

unsafe impl fbl::Recyclable for VmAddressRegion {
    unsafe fn recycle(ptr: NonNull<Self>) {
        unsafe {
            cpp_vm_address_region_free(ptr.as_ptr());
        }
    }

    fn allocate(_value: Self) -> Result<NonNull<Self>, AllocError> {
        Err(AllocError)
    }
}
