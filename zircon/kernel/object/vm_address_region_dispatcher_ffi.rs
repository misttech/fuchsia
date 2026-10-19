// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::vm_address_region_dispatcher::{
    VmAddressRegionDispatcher, VmAddressRegionDispatcherState,
};
use crate::vm::arch_vm_aspace::ArchMmuFlags;
use crate::vm::vm_address_region::VmAddressRegion;
use core::mem::MaybeUninit;
use fbl::RefPtr;
use zx_types::{ZX_OK, zx_rights_t, zx_status_t};

unsafe extern "C" {
    /// Calls into C++ to allocate and initialize a `VmAddressRegionDispatcher`.
    ///
    /// # Safety
    ///
    /// `vmar` must be a valid owning raw pointer from `RefPtr::into_raw`, and `handle_out` must
    /// point to valid writable stack memory.
    pub(crate) fn cpp_vmar_dispatcher_create(
        vmar: *mut VmAddressRegion,
        base_arch_mmu_flags: ArchMmuFlags,
        handle_out: *mut MaybeUninit<KernelHandle<VmAddressRegionDispatcher>>,
    ) -> zx_status_t;
}

crate::object::dispatcher::impl_dispatcher_state_init!(
    VmAddressRegionDispatcher,
    VmAddressRegionDispatcherState,
    vmar: RefPtr<VmAddressRegion>,
    base_arch_mmu_flags: ArchMmuFlags,
);

/// Creates a `VmAddressRegionDispatcher` from C++.
///
/// # Safety
///
/// `vmar_raw` must be a valid raw pointer exported from `fbl::RefPtr<VmAddressRegion>`.
/// `handle_out` and `rights_out` must point to valid uninitialized memory.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_vmar_dispatcher_create(
    vmar_raw: *mut VmAddressRegion,
    base_arch_mmu_flags: ArchMmuFlags,
    handle_out: *mut MaybeUninit<KernelHandle<VmAddressRegionDispatcher>>,
    rights_out: *mut MaybeUninit<zx_rights_t>,
) -> zx_status_t {
    // SAFETY: `vmar_raw` was exported from `fbl::RefPtr<VmAddressRegion>`.
    let vmar = unsafe { RefPtr::from_raw(vmar_raw) };
    match VmAddressRegionDispatcher::create(vmar, base_arch_mmu_flags) {
        Ok((handle, rights)) => {
            // SAFETY: Caller guarantees `handle_out` and `rights_out` point to valid
            // uninitialized memory.
            unsafe {
                (*handle_out).write(handle);
                (*rights_out).write(rights);
            }
            ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}
