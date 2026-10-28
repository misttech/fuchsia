// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::types::PAddr;
#[cfg(target_arch = "x86_64")]
use crate::object::validate_resource_ioport;
use crate::object::{
    BusTransactionInitiatorDispatcher, HandleValue, InitialMutability, ProcessDispatcher,
    VmObjectDispatcher, validate_resource_mmio,
};
use crate::vm::pmm::ALLOC_FLAG_ANY;
use crate::vm::vm_object_paged::VmObjectPaged;
use crate::vm::vm_object_physical::VmObjectPhysical;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{ZX_POL_NEW_VMO, ZX_RIGHT_MAP, zx_paddr_t};

const LOCAL_TRACE: u32 = 0;

// zx_status_t zx_vmo_create_contiguous
#[syscall]
pub fn sys_vmo_create_contiguous(
    bti: HandleValue,
    size: usize,
    mut alignment_log2: u32,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("size 0x{:x}\n", size);

    if size == 0 {
        return Err(Status::INVALID_ARGS);
    }

    if !page::is_page_rounded(size) {
        return Err(Status::INVALID_ARGS);
    }

    if alignment_log2 == 0 {
        alignment_log2 = page::SHIFT as u32;
    }
    // catch obviously wrong values
    if alignment_log2 < (page::SHIFT as u32) || alignment_log2 >= u64::BITS {
        return Err(Status::INVALID_ARGS);
    }

    let up = ProcessDispatcher::get_current();
    up.enforce_basic_policy(ZX_POL_NEW_VMO)?;

    let _bti_dispatcher = up
        .handle_table()
        .get_dispatcher_with_rights::<BusTransactionInitiatorDispatcher>(&up, bti, ZX_RIGHT_MAP)?;

    let align_log2_arg = alignment_log2 as u8;

    // create a vm object
    let vmo = VmObjectPaged::create_contiguous(ALLOC_FLAG_ANY, size as u64, align_log2_arg)?;

    // create a Vm Object dispatcher
    let (kernel_handle, rights) =
        VmObjectDispatcher::create(&vmo, size as u64, InitialMutability::Mutable)?;

    // create a handle and attach the dispatcher to it
    *out = up.make_and_add_handle(kernel_handle, rights)?;
    Ok(())
}

// zx_status_t zx_vmo_create_physical
#[syscall]
pub fn sys_vmo_create_physical(
    hrsrc: HandleValue,
    paddr: zx_paddr_t,
    size: usize,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("size 0x{:x}\n", size);

    if size == 0 {
        return Err(Status::INVALID_ARGS);
    }

    if !page::is_page_rounded(size) {
        return Err(Status::INVALID_ARGS);
    }

    let up = ProcessDispatcher::get_current();
    up.enforce_basic_policy(ZX_POL_NEW_VMO)?;

    // Memory should be subtracted from the PhysicalAspace allocators, so it's
    // safe to assume that if the caller has access to a resource for this specified
    // region of MMIO space then it is safe to allow the vmo to be created.
    validate_resource_mmio(hrsrc, paddr as u64, size)?;

    // create a vm object
    let vmo = VmObjectPhysical::create(PAddr(paddr), size)?;

    // create a Vm Object dispatcher
    let (kernel_handle, rights) = VmObjectDispatcher::create(
        &VmObjectPhysical::into_vm_object(vmo),
        size as u64,
        InitialMutability::Mutable,
    )?;

    // create a handle and attach the dispatcher to it
    *out = up.make_and_add_handle(kernel_handle, rights)?;
    Ok(())
}

// zx_status_t zx_ioports_request
#[cfg(target_arch = "x86_64")]
#[syscall]
pub fn sys_ioports_request(hrsrc: HandleValue, io_addr: u16, len: u32) -> Result<(), Status> {
    validate_resource_ioport(hrsrc, u64::from(io_addr), len as usize)?;

    ltracef!("addr {:#x} len {:#x}\n", io_addr, len);

    crate::arch_rs::IoBitmap::with_current(|io_bitmap| {
        io_bitmap.set_io_bitmap(u32::from(io_addr), len, true)
    })
    .unwrap()
}

// zx_status_t zx_ioports_release
#[cfg(target_arch = "x86_64")]
#[syscall]
pub fn sys_ioports_release(hrsrc: HandleValue, io_addr: u16, len: u32) -> Result<(), Status> {
    validate_resource_ioport(hrsrc, u64::from(io_addr), len as usize)?;

    ltracef!("addr {:#x} len {:#x}\n", io_addr, len);

    crate::arch_rs::IoBitmap::with_current(|io_bitmap| {
        io_bitmap.set_io_bitmap(u32::from(io_addr), len, false)
    })
    .unwrap()
}

// zx_status_t zx_ioports_request
#[cfg(not(target_arch = "x86_64"))]
#[syscall]
pub fn sys_ioports_request(_hrsrc: HandleValue, _io_addr: u16, _len: u32) -> Result<(), Status> {
    // doesn't make sense on non-x86
    Err(Status::NOT_SUPPORTED)
}

// zx_status_t zx_ioports_release
#[cfg(not(target_arch = "x86_64"))]
#[syscall]
pub fn sys_ioports_release(_hrsrc: HandleValue, _io_addr: u16, _len: u32) -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}
