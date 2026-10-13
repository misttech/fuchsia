// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{
    HandleValue, InterruptDispatcher, InterruptEventDispatcher, PortDispatcher, ProcessDispatcher,
    VirtualInterruptDispatcher, validate_resource_irq,
};
use crate::user_copy::UserOutPtr;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_INTERRUPT_BIND, ZX_INTERRUPT_UNBIND, ZX_INTERRUPT_VIRTUAL, ZX_INTERRUPT_WAKE_VECTOR,
    ZX_RIGHT_READ, ZX_RIGHT_SIGNAL, ZX_RIGHT_WAIT, ZX_RIGHT_WRITE, zx_time_t,
};

const LOCAL_TRACE: u32 = 0;

// zx_status_t zx_interrupt_create
#[syscall]
pub fn sys_interrupt_create(
    src_obj: HandleValue,
    src_num: u32,
    options: u32,
    out_handle: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("options {:#x}\n", options);

    // The IRQ resource is required for all non-virtual interrupts, and virtual
    // interrupts which are configured as wake vectors.
    if (options & ZX_INTERRUPT_VIRTUAL) == 0 || (options & ZX_INTERRUPT_WAKE_VECTOR) != 0 {
        validate_resource_irq(src_obj, src_num)?;
    }

    let (handle, rights) = if (options & ZX_INTERRUPT_VIRTUAL) != 0 {
        VirtualInterruptDispatcher::create(options)?
    } else {
        InterruptEventDispatcher::create(src_num, options, false)?
    };

    *out_handle = handle.make_and_add_handle(rights)?;
    Ok(())
}

// zx_status_t zx_interrupt_bind
#[syscall]
pub fn sys_interrupt_bind(
    handle: HandleValue,
    port_handle: HandleValue,
    key: u64,
    options: u32,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle.raw_value());
    if options != ZX_INTERRUPT_BIND && options != ZX_INTERRUPT_UNBIND {
        return Err(Status::INVALID_ARGS);
    }

    let up = ProcessDispatcher::get_current();
    let interrupt = up.handle_table().get_dispatcher_with_rights::<InterruptDispatcher>(
        &up,
        handle,
        ZX_RIGHT_READ,
    )?;

    let port = up.handle_table().get_dispatcher_with_rights::<PortDispatcher>(
        &up,
        port_handle,
        ZX_RIGHT_WRITE,
    )?;
    if !port.can_bind_to_interrupt() {
        return Err(Status::WRONG_TYPE);
    }

    if options == ZX_INTERRUPT_BIND { interrupt.bind(port, key) } else { interrupt.unbind(&port) }
}

// zx_status_t zx_interrupt_ack
#[syscall]
pub fn sys_interrupt_ack(inth: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:x}\n", inth.raw_value());

    let up = ProcessDispatcher::get_current();
    let interrupt = up.handle_table().get_dispatcher_with_rights::<InterruptDispatcher>(
        &up,
        inth,
        ZX_RIGHT_WRITE,
    )?;
    interrupt.ack()
}

// zx_status_t zx_interrupt_wait
#[syscall]
pub fn sys_interrupt_wait(
    handle: HandleValue,
    out_timestamp: UserOutPtr<zx_time_t>,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle.raw_value());

    let up = ProcessDispatcher::get_current();
    let interrupt = up.handle_table().get_dispatcher_with_rights::<InterruptDispatcher>(
        &up,
        handle,
        ZX_RIGHT_WAIT,
    )?;

    let timestamp = interrupt.wait_for_interrupt()?;
    if !out_timestamp.is_null() {
        out_timestamp.write(timestamp)?;
    }

    Ok(())
}

// zx_status_t zx_interrupt_destroy
#[syscall]
pub fn sys_interrupt_destroy(handle: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle.raw_value());

    let up = ProcessDispatcher::get_current();
    let interrupt = up.handle_table().get_dispatcher::<InterruptDispatcher>(&up, handle)?;

    interrupt.destroy()
}

// zx_status_t zx_interrupt_trigger
#[syscall]
pub fn sys_interrupt_trigger(
    handle: HandleValue,
    options: u32,
    timestamp: zx_time_t,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle.raw_value());

    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let up = ProcessDispatcher::get_current();
    let interrupt = up.handle_table().get_dispatcher_with_rights::<InterruptDispatcher>(
        &up,
        handle,
        ZX_RIGHT_SIGNAL,
    )?;

    interrupt.trigger(timestamp)
}
