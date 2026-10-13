// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    HandleValue, MsiAllocation, MsiDispatcher, MsiInterruptDispatcher, ProcessDispatcher,
    VmObjectDispatcher, validate_resource_kind_base,
};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{ZX_RIGHT_MAP, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_MSI_BASE};

const LOCAL_TRACE: u32 = 0;

#[syscall]
pub fn sys_msi_allocate(msi: HandleValue, count: u32, out: &mut HandleValue) -> Result<(), Status> {
    ltracef!("msi handle {:#x}, count {}\n", msi.raw_value(), count);

    validate_resource_kind_base(msi, ZX_RSRC_KIND_SYSTEM, ZX_RSRC_SYSTEM_MSI_BASE)?;

    let alloc = MsiAllocation::create(count)?;
    let (kernel_handle, rights) = MsiDispatcher::create(alloc)?;
    *out = kernel_handle.make_and_add_handle(rights)?;
    Ok(())
}

#[syscall]
pub fn sys_msi_create(
    msi_alloc: HandleValue,
    options: u32,
    msi_id: u32,
    vmo: HandleValue,
    vmo_offset: usize,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!(
        "msi_alloc handle {:#x}, options {:#x}, msi_id {}, vmo handle {:#x}, vmo_offset {:#x}\n",
        msi_alloc.raw_value(),
        options,
        msi_id,
        vmo.raw_value(),
        vmo_offset
    );

    let up = ProcessDispatcher::get_current();
    let msi_alloc_disp = up.handle_table().get_dispatcher::<MsiDispatcher>(&up, msi_alloc)?;
    let vmo_disp = up.handle_table().get_dispatcher_with_rights::<VmObjectDispatcher>(
        &up,
        vmo,
        ZX_RIGHT_MAP,
    )?;

    let (kernel_handle, rights) = MsiInterruptDispatcher::create(
        msi_alloc_disp.msi_allocation().clone(),
        msi_id,
        vmo_disp.vmo(),
        vmo_offset,
        options,
        None,
    )?;

    *out = kernel_handle.make_and_add_handle(rights)?;
    Ok(())
}
