// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{CStr, c_char, c_void};
use core::ptr;
use fbl::RefPtr;
use zx_status::Status;
use zx_types::{zx_gpaddr_t, zx_paddr_t};

use super::aspace::{GuestPhysicalAspace, GuestPtr};
use crate::kernel::types::PAddr;

/// FFI entry point for `hypervisor::GuestPtr::reset`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_ptr_reset(guest_ptr: &mut GuestPtr) {
    guest_ptr.reset();
}

/// FFI entry point for `hypervisor::GuestPtr::as`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_ptr_as(guest_ptr: &GuestPtr, type_size: usize) -> *mut c_void {
    guest_ptr.as_raw_ptr(type_size)
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::Create`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_create(
    out: &mut GuestPhysicalAspace,
) -> Result<(), Status> {
    *out = GuestPhysicalAspace::create()?;
    Ok(())
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::~GuestPhysicalAspace`.
///
/// # Safety
///
/// `gpa` must point to a valid, initialized `GuestPhysicalAspace` that is being destroyed.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_guest_physical_aspace_destroy(gpa: *mut GuestPhysicalAspace) {
    // SAFETY: Caller guarantees `gpa` is valid for destruction.
    unsafe { ptr::drop_in_place(gpa) };
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::size`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_size(gpa: &GuestPhysicalAspace) -> usize {
    gpa.size()
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::arch_aspace`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_arch_aspace(gpa: &GuestPhysicalAspace) -> *mut c_void {
    ptr::from_ref(gpa.arch_aspace()).cast_mut().cast()
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::RootVmar`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_root_vmar(gpa: &GuestPhysicalAspace) -> *mut c_void {
    RefPtr::into_raw(gpa.root_vmar()).cast_mut().cast()
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::IsMapped`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_is_mapped(
    gpa: &GuestPhysicalAspace,
    guest_paddr: zx_gpaddr_t,
) -> bool {
    gpa.is_mapped(guest_paddr)
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::MapInterruptController`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_map_interrupt_controller(
    gpa: &mut GuestPhysicalAspace,
    guest_paddr: zx_gpaddr_t,
    host_paddr: zx_paddr_t,
    len: usize,
) -> Result<(), Status> {
    gpa.map_interrupt_controller(guest_paddr, PAddr(host_paddr), len)
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::UnmapRange`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_unmap_range(
    gpa: &mut GuestPhysicalAspace,
    guest_paddr: zx_gpaddr_t,
    len: usize,
) -> Result<(), Status> {
    gpa.unmap_range(guest_paddr, len)
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::PageFault`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_guest_physical_aspace_page_fault(
    gpa: &mut GuestPhysicalAspace,
    guest_paddr: zx_gpaddr_t,
) -> Result<(), Status> {
    gpa.page_fault(guest_paddr)
}

/// FFI entry point for `hypervisor::GuestPhysicalAspace::CreateGuestPtr`.
///
/// # Safety
///
/// `name` must be a valid NUL-terminated C string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_guest_physical_aspace_create_guest_ptr(
    gpa: &mut GuestPhysicalAspace,
    guest_paddr: zx_gpaddr_t,
    len: usize,
    name: *const c_char,
    out: &mut GuestPtr,
) -> Result<(), Status> {
    // SAFETY: Caller guarantees `name` is a valid NUL-terminated C string.
    let name_bytes = unsafe { CStr::from_ptr(name) }.to_bytes();
    *out = gpa.create_guest_ptr(guest_paddr, len, name_bytes)?;
    Ok(())
}
