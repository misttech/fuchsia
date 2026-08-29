// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{BusTransactionInitiatorDispatcher, Dispatcher, HandleValue, IommuDispatcher};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{ZX_RIGHT_NONE, ZX_RIGHT_WRITE};

const LOCAL_TRACE: u32 = 0;

#[syscall]
pub fn sys_bti_create(
    iommu: HandleValue,
    options: u32,
    bti_id: u64,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("options {options:#x}, bti_id {bti_id}\n");

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    // TODO(teisenbe): This should probably have a right on it.
    let iommu_dispatcher = Dispatcher::get_with_rights::<IommuDispatcher>(iommu, ZX_RIGHT_NONE)?;

    let (handle, rights) =
        BusTransactionInitiatorDispatcher::create(iommu_dispatcher.iommu(), bti_id)?;
    *out = handle.make_and_add_handle(rights)?;
    Ok(())
}

#[syscall]
pub fn sys_bti_release_quarantine(handle: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:#x}\n", handle.raw_value());

    let bti_dispatcher =
        Dispatcher::get_with_rights::<BusTransactionInitiatorDispatcher>(handle, ZX_RIGHT_WRITE)?;

    bti_dispatcher.release_quarantine();
    Ok(())
}
