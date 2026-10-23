// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::kernel::deadline::Deadline;
use crate::object::{HandleValue, PortDispatcher, ProcessDispatcher, RawPortPacket};
use crate::platform_rs::timer::InstantUnknown;
use crate::user_copy::{UserInPtr, UserOutPtr};
use core::mem::MaybeUninit;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_POL_NEW_PORT, ZX_RIGHT_READ, ZX_RIGHT_WAIT, ZX_RIGHT_WRITE, zx_instant_mono_t,
    zx_port_packet_t,
};

const LOCAL_TRACE: u32 = 0;

// Track the number of calls to the two zx_port_cancel.. APIs. We expect the
// ecosystem to migrate to calling zx_port_cancel_key() over time.
// TODO(https://fxbug.dev/434997822): Remove this tracking when the migration is complete.
define_kcounter!(PORT_CANCEL_CALLS, "port.cancel", Sum);
define_kcounter!(PORT_CANCEL_KEY_CALLS, "port.cancel_key", Sum);

// zx_status_t zx_port_create
#[syscall]
pub fn sys_port_create(options: u32, out: &mut HandleValue) -> Result<(), Status> {
    ltracef!("options {}\n", options);
    let up = ProcessDispatcher::get_current();
    up.enforce_basic_policy(ZX_POL_NEW_PORT)?;

    let (handle, rights) = PortDispatcher::create(options)?;

    *out = up.make_and_add_handle(handle, rights)?;
    Ok(())
}

// zx_status_t zx_port_queue
#[syscall]
pub fn sys_port_queue(
    handle: HandleValue,
    packet_in: UserInPtr<zx_port_packet_t>,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle.raw_value());

    let up = ProcessDispatcher::get_current();

    let port = up.handle_table().get_dispatcher_with_rights::<PortDispatcher>(
        &up,
        handle,
        ZX_RIGHT_WRITE,
    )?;

    let mut packet = MaybeUninit::uninit();
    let packet = packet_in.reinterpret::<RawPortPacket>().copy_from_user(&mut packet)?;

    port.queue_user(packet)
}

// zx_status_t zx_port_wait
#[syscall]
pub fn sys_port_wait(
    handle: HandleValue,
    deadline: zx_instant_mono_t,
    packet_out: UserOutPtr<zx_port_packet_t>,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle.raw_value());

    let up = ProcessDispatcher::get_current();

    let port = up.handle_table().get_dispatcher_with_rights::<PortDispatcher>(
        &up,
        handle,
        ZX_RIGHT_READ,
    )?;

    let slack_deadline = Deadline::new(InstantUnknown(deadline), up.get_timer_slack_policy());

    let mut pp = MaybeUninit::uninit();
    port.dequeue(&slack_deadline, &mut pp)?;
    // SAFETY: `dequeue` returned `Ok`, which guarantees it initialized `pp`.
    let pp = unsafe { pp.assume_init_ref() };

    packet_out.reinterpret::<RawPortPacket>().copy_to_user(pp)?;

    Ok(())
}

// zx_status_t zx_port_cancel
#[syscall]
pub fn sys_port_cancel(handle: HandleValue, source: HandleValue, key: u64) -> Result<(), Status> {
    PORT_CANCEL_CALLS.add(1);

    let up = ProcessDispatcher::get_current();

    let port = up.handle_table().get_dispatcher_with_rights::<PortDispatcher>(
        &up,
        handle,
        ZX_RIGHT_WRITE,
    )?;

    {
        ksync::lock!(let guard = up.handle_table().read_lock());
        let watched = up
            .handle_table()
            .get_handle_locked(guard.token(), &up, source)
            .ok_or(Status::BAD_HANDLE)?;
        if !watched.has_rights(ZX_RIGHT_WAIT) {
            return Err(Status::ACCESS_DENIED);
        }

        let dispatcher = watched.dispatcher();
        if !dispatcher.is_waitable() {
            return Err(Status::NOT_SUPPORTED);
        }

        let had_observer = dispatcher.cancel_by_key(watched, &port, key);
        let packet_removed = port.cancel_queued(watched, key);
        if had_observer || packet_removed { Ok(()) } else { Err(Status::NOT_FOUND) }
    }
}

// zx_status_t zx_port_cancel_key
#[syscall]
pub fn sys_port_cancel_key(handle: HandleValue, options: u32, key: u64) -> Result<(), Status> {
    PORT_CANCEL_KEY_CALLS.add(1);
    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }
    let up = ProcessDispatcher::get_current();

    let port = up.handle_table().get_dispatcher_with_rights::<PortDispatcher>(
        &up,
        handle,
        ZX_RIGHT_WRITE,
    )?;

    port.cancel_key(key)
}
