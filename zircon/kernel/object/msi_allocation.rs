// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::dev_interrupt::MsiBlock;
use fbl::RefPtr;
use ksync::{KMutex, RawSpinlock};
use zx_status::Status;
use zx_types::{zx_info_msi_t, zx_status_t};

/// Lock class for `MsiAllocation` spinlocks.
pub struct MsiAllocationLockClass;

impl ksync::LockClass for MsiAllocationLockClass {
    const ID: *mut core::ffi::c_void = core::ptr::null_mut();
}

unsafe extern "C" {
    fn cpp_msi_allocation_create(count: u32, alloc_out: *mut *mut MsiAllocation) -> zx_status_t;
    fn cpp_msi_allocation_get_info(alloc: *const MsiAllocation) -> zx_info_msi_t;
    fn cpp_msi_allocation_recycle(msi_alloc: *mut MsiAllocation);
    fn cpp_msi_allocation_reserve_id(alloc: *mut MsiAllocation, msi_id: u32) -> zx_status_t;
    fn cpp_msi_allocation_release_id(alloc: *mut MsiAllocation, msi_id: u32) -> zx_status_t;
    fn cpp_msi_allocation_get_block(alloc: *const MsiAllocation) -> *const MsiBlock;
    fn cpp_msi_allocation_get_lock(
        alloc: *mut MsiAllocation,
    ) -> *mut KMutex<MsiAllocationLockClass, RawSpinlock>;
}

fbl::impl_opaque_ref_counted_facade!(
    /// Facade type representing the C++ `MsiAllocation` object.
    pub struct MsiAllocation,
    cpp_msi_allocation_recycle,
);

impl MsiAllocation {
    /// Creates a new C++ `MsiAllocation` object with `count` interrupts.
    pub fn create(count: u32) -> Result<RefPtr<Self>, Status> {
        let mut alloc_raw = core::ptr::null_mut();
        // SAFETY: `&mut alloc_raw` is a valid non-null pointer to receive the raw
        // `MsiAllocation` pointer.
        let status = unsafe { cpp_msi_allocation_create(count, &mut alloc_raw) };
        Status::ok(status)?;
        // SAFETY: `cpp_msi_allocation_create` succeeded and returned an exported raw pointer.
        unsafe { Ok(RefPtr::from_raw(alloc_raw)) }
    }

    /// Returns `zx_info_msi_t` for this allocation.
    pub fn get_info(&self) -> zx_info_msi_t {
        // SAFETY: `self` is a valid `MsiAllocation` reference.
        unsafe { cpp_msi_allocation_get_info(self as *const _) }
    }

    /// Reserves `msi_id` within this allocation.
    pub fn reserve_id(&self, msi_id: u32) -> Result<(), Status> {
        // SAFETY: `self` is a valid `MsiAllocation` reference.
        Status::ok(unsafe { cpp_msi_allocation_reserve_id(self as *const _ as *mut _, msi_id) })
    }

    /// Releases a previously reserved `msi_id` within this allocation.
    pub fn release_id(&self, msi_id: u32) -> Result<(), Status> {
        // SAFETY: `self` is a valid `MsiAllocation` reference.
        Status::ok(unsafe { cpp_msi_allocation_release_id(self as *const _ as *mut _, msi_id) })
    }

    /// Returns a reference to the underlying `MsiBlock`.
    pub fn block(&self) -> &MsiBlock {
        // SAFETY: `self` is a valid `MsiAllocation` reference and `block_` lives as long as `self`.
        unsafe { &*cpp_msi_allocation_get_block(self as *const _) }
    }

    /// Returns a reference to the allocation's spinlock.
    pub fn lock(&self) -> &KMutex<MsiAllocationLockClass, RawSpinlock> {
        // SAFETY: `self` is a valid `MsiAllocation` reference and `lock_` is layout-compatible with
        // `KMutex<MsiAllocationLockClass, RawSpinlock>`.
        unsafe { &*cpp_msi_allocation_get_lock(self as *const _ as *mut _) }
    }
}
