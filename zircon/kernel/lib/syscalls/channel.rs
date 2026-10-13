// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::kernel::deadline::{Deadline, InstantUnknown};
use crate::object::{
    ChannelDispatcher, HandleValue, MessagePacket, MessagePacketPtr, ProcessDispatcher,
    ReadHandles, ThreadDispatcher, WriteHandles,
};
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
use core::mem::{align_of, size_of};
use core::ptr;
use debug::ltracef;
use syscalls_macro::syscall;
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zr::static_assert_size_and_align;
use zx_status::Status;
use zx_types::{
    ZX_CHANNEL_READ_MAY_DISCARD, ZX_CHANNEL_WRITE_USE_IOVEC, ZX_POL_NEW_CHANNEL,
    zx_channel_call_args_t, zx_channel_call_etc_args_t, zx_channel_iovec_t,
    zx_handle_disposition_t, zx_handle_info_t, zx_handle_t, zx_instant_mono_t, zx_txid_t,
};

const LOCAL_TRACE: u32 = 0;

define_kcounter!(CHANNEL_MSG_0_BYTES, "channel.bytes.0", Sum);
define_kcounter!(CHANNEL_MSG_64_BYTES, "channel.bytes.64", Sum);
define_kcounter!(CHANNEL_MSG_256_BYTES, "channel.bytes.256", Sum);
define_kcounter!(CHANNEL_MSG_1K_BYTES, "channel.bytes.1k", Sum);
define_kcounter!(CHANNEL_MSG_4K_BYTES, "channel.bytes.4k", Sum);
define_kcounter!(CHANNEL_MSG_16K_BYTES, "channel.bytes.16k", Sum);
define_kcounter!(CHANNEL_MSG_64K_BYTES, "channel.bytes.64k", Sum);
define_kcounter!(CHANNEL_MSG_0_HANDLES, "channel.handles.00", Sum);
define_kcounter!(CHANNEL_MSG_1_HANDLES, "channel.handles.01", Sum);
define_kcounter!(CHANNEL_MSG_2_HANDLES, "channel.handles.02", Sum);
define_kcounter!(CHANNEL_MSG_4_HANDLES, "channel.handles.04", Sum);
define_kcounter!(CHANNEL_MSG_8_HANDLES, "channel.handles.08", Sum);
define_kcounter!(CHANNEL_MSG_16_HANDLES, "channel.handles.16", Sum);
define_kcounter!(CHANNEL_MSG_32_HANDLES, "channel.handles.32", Sum);
define_kcounter!(CHANNEL_MSG_64_HANDLES, "channel.handles.64", Sum);
define_kcounter!(CHANNEL_MSG_RECEIVED, "channel.messages", Sum);

#[inline]
fn record_recv_msg_sz(num_bytes: u32, num_handles: u32) {
    CHANNEL_MSG_RECEIVED.add(1);

    match num_bytes {
        0 => CHANNEL_MSG_0_BYTES.add(1),
        1..=64 => CHANNEL_MSG_64_BYTES.add(1),
        65..=256 => CHANNEL_MSG_256_BYTES.add(1),
        257..=1024 => CHANNEL_MSG_1K_BYTES.add(1),
        1025..=4096 => CHANNEL_MSG_4K_BYTES.add(1),
        4097..=16384 => CHANNEL_MSG_16K_BYTES.add(1),
        16385..=65536 => CHANNEL_MSG_64K_BYTES.add(1),
        _ => {}
    }

    match num_handles {
        0 => CHANNEL_MSG_0_HANDLES.add(1),
        1 => CHANNEL_MSG_1_HANDLES.add(1),
        2 => CHANNEL_MSG_2_HANDLES.add(1),
        3..=4 => CHANNEL_MSG_4_HANDLES.add(1),
        5..=8 => CHANNEL_MSG_8_HANDLES.add(1),
        9..=16 => CHANNEL_MSG_16_HANDLES.add(1),
        17..=32 => CHANNEL_MSG_32_HANDLES.add(1),
        33..=64 => CHANNEL_MSG_64_HANDLES.add(1),
        _ => {}
    }
}

#[repr(C)]
#[derive(Copy, Clone, FromBytes, IntoBytes, Immutable)]
struct RawChannelCallArgs {
    wr_bytes: usize,
    wr_handles: usize,
    rd_bytes: usize,
    rd_handles: usize,
    wr_num_bytes: u32,
    wr_num_handles: u32,
    rd_num_bytes: u32,
    rd_num_handles: u32,
}

impl RawChannelCallArgs {
    #[inline]
    fn wr_bytes(&self) -> UserInPtr<u8> {
        UserInPtr::new(ptr::with_exposed_provenance(self.wr_bytes))
    }

    #[inline]
    fn rd_bytes(&self) -> UserOutPtr<u8> {
        UserOutPtr::new(ptr::with_exposed_provenance_mut(self.rd_bytes))
    }

    #[inline]
    fn wr_handles(&self) -> UserInPtr<zx_handle_t> {
        UserInPtr::new(ptr::with_exposed_provenance(self.wr_handles))
    }

    #[inline]
    fn rd_handles(&self) -> UserOutPtr<zx_handle_t> {
        UserOutPtr::new(ptr::with_exposed_provenance_mut(self.rd_handles))
    }

    #[inline]
    fn wr_handle_dispositions(&self) -> UserInOutPtr<zx_handle_disposition_t> {
        UserInOutPtr::new(ptr::with_exposed_provenance_mut(self.wr_handles))
    }

    #[inline]
    fn rd_handle_infos(&self) -> UserOutPtr<zx_handle_info_t> {
        UserOutPtr::new(ptr::with_exposed_provenance_mut(self.rd_handles))
    }
}

static_assert_size_and_align!(
    RawChannelCallArgs,
    size_of::<zx_channel_call_args_t>(),
    align_of::<zx_channel_call_args_t>()
);

static_assert_size_and_align!(
    RawChannelCallArgs,
    size_of::<zx_channel_call_etc_args_t>(),
    align_of::<zx_channel_call_etc_args_t>()
);

#[syscall]
pub fn sys_channel_create(
    options: u32,
    out0: &mut HandleValue,
    out1: &mut HandleValue,
) -> Result<(), Status> {
    if options != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let proc = ProcessDispatcher::get_current();
    proc.enforce_basic_policy(ZX_POL_NEW_CHANNEL)?;

    let (handle0, handle1, rights) = ChannelDispatcher::create()?;

    let user_handle0 = proc.make_and_add_handle(handle0, rights)?;
    let user_handle1 = proc.make_and_add_handle(handle1, rights)?;

    *out0 = user_handle0;
    *out1 = user_handle1;
    Ok(())
}

#[inline]
fn copy_out_message<R: ReadHandles>(
    proc: &ProcessDispatcher,
    mut msg: MessagePacketPtr,
    bytes: UserOutPtr<u8>,
    handles: R,
) -> Result<(), Status> {
    let num_bytes = msg.data_size() as u32;
    let num_handles = msg.num_handles() as u32;

    if num_bytes > 0 {
        msg.copy_data_to(bytes)?;
    }

    // The documented public API states that writing to the handles buffer must happen after writing
    // to the data buffer.
    if num_handles > 0 {
        handles.get_handles(proc, &mut msg)?;
    }

    record_recv_msg_sz(num_bytes, num_handles);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn channel_read<R: ReadHandles>(
    handle_value: HandleValue,
    options: u32,
    bytes: UserOutPtr<u8>,
    handles: R,
    mut num_bytes: u32,
    mut num_handles: u32,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    ltracef!(
        "handle {:#x} bytes {:?} num_bytes {} num_handles {} actual_bytes {:?} actual_handles {:?}\n",
        handle_value.raw_value(),
        bytes.as_ptr(),
        num_bytes,
        num_handles,
        actual_bytes.as_ptr(),
        actual_handles.as_ptr()
    );

    let proc = ProcessDispatcher::get_current();
    let handle_table = proc.handle_table();
    let channel = handle_table.get_dispatcher_with_rights::<ChannelDispatcher>(
        &proc,
        handle_value,
        zx_types::ZX_RIGHT_READ,
    )?;

    // Currently MAY_DISCARD is the only allowable option.
    if (options & !ZX_CHANNEL_READ_MAY_DISCARD) != 0 {
        return Err(Status::NOT_SUPPORTED);
    }

    let read_result = channel.read(
        handle_table.get_koid(),
        &mut num_bytes,
        &mut num_handles,
        (options & ZX_CHANNEL_READ_MAY_DISCARD) != 0,
    );

    if let Err(err) = read_result
        && err != Status::BUFFER_TOO_SMALL
    {
        return Err(err);
    }

    // On ZX_ERR_BUFFER_TOO_SMALL, read() gives us the size of the next message (which remains
    // unconsumed, unless `options` has ZX_CHANNEL_READ_MAY_DISCARD set).
    if !actual_bytes.is_null() {
        actual_bytes.write(num_bytes)?;
    }
    if !actual_handles.is_null() {
        actual_handles.write(num_handles)?;
    }

    copy_out_message(&proc, read_result?, bytes, handles)
}

#[syscall]
pub fn sys_channel_read(
    handle: HandleValue,
    options: u32,
    bytes: UserOutPtr<u8>,
    handles: UserOutPtr<zx_handle_t>,
    num_bytes: u32,
    num_handles: u32,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    channel_read(
        handle,
        options,
        bytes,
        handles,
        num_bytes,
        num_handles,
        actual_bytes,
        actual_handles,
    )
}

#[syscall]
pub fn sys_channel_read_etc(
    handle: HandleValue,
    options: u32,
    bytes: UserOutPtr<u8>,
    handles: UserOutPtr<zx_handle_info_t>,
    num_bytes: u32,
    num_handles: u32,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    channel_read(
        handle,
        options,
        bytes,
        handles,
        num_bytes,
        num_handles,
        actual_bytes,
        actual_handles,
    )
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn prepare_write<W: WriteHandles>(
    proc: &ProcessDispatcher,
    handle: HandleValue,
    rights: zx_types::zx_rights_t,
    options: u32,
    bytes: UserInPtr<u8>,
    num_bytes: u32,
    handles: W,
    num_handles: u32,
    min_data_size: usize,
) -> Result<(fbl::RefPtr<ChannelDispatcher>, zx_types::zx_koid_t, MessagePacketPtr), Status> {
    let mut cleanup = zr::defer(|| {
        if num_handles > 0 {
            handles.remove_user_handles(proc, num_handles);
        }
    });

    if (options & !ZX_CHANNEL_WRITE_USE_IOVEC) != 0 {
        return Err(Status::INVALID_ARGS);
    }

    let handle_table = proc.handle_table();
    let channel =
        handle_table.get_dispatcher_with_rights::<ChannelDispatcher>(proc, handle, rights)?;
    // Prepare a MessagePacket for writing.
    let mut msg = if (options & ZX_CHANNEL_WRITE_USE_IOVEC) != 0 {
        MessagePacket::create_from_iovecs(
            bytes.reinterpret::<zx_channel_iovec_t>(),
            num_bytes as usize,
            num_handles as usize,
        )?
    } else {
        MessagePacket::create_from_user(bytes, num_bytes as usize, num_handles as usize)?
    };

    if msg.data_size() < min_data_size {
        return Err(Status::INVALID_ARGS);
    }

    // put_handles() always consumes all handles that should be consumed (or there are zero handles,
    // and so there's nothing to be done).
    cleanup.cancel();

    if num_handles > 0 {
        handles.put_handles(proc, &channel, &mut msg)?;
    }

    Ok((channel, handle_table.get_koid(), msg))
}

#[inline]
fn channel_write<W: WriteHandles>(
    handle: HandleValue,
    options: u32,
    bytes: UserInPtr<u8>,
    num_bytes: u32,
    handles: W,
    num_handles: u32,
) -> Result<(), Status> {
    ltracef!(
        "handle {:#x} bytes {:?} num_bytes {} num_handles {} options {:#x}\n",
        handle.raw_value(),
        bytes.as_ptr(),
        num_bytes,
        num_handles,
        options
    );

    let proc = ProcessDispatcher::get_current();
    let (channel, owner, msg) = prepare_write(
        &proc,
        handle,
        zx_types::ZX_RIGHT_WRITE,
        options,
        bytes,
        num_bytes,
        handles,
        num_handles,
        0,
    )?;
    channel.write(owner, msg)
}

#[syscall]
pub fn sys_channel_write(
    handle: HandleValue,
    options: u32,
    bytes: UserInPtr<u8>,
    num_bytes: u32,
    handles: UserInPtr<zx_handle_t>,
    num_handles: u32,
) -> Result<(), Status> {
    channel_write(handle, options, bytes, num_bytes, handles, num_handles)
}

#[syscall]
pub fn sys_channel_write_etc(
    handle: HandleValue,
    options: u32,
    bytes: UserInPtr<u8>,
    num_bytes: u32,
    handles: UserInOutPtr<zx_handle_disposition_t>,
    num_handles: u32,
) -> Result<(), Status> {
    channel_write(handle, options, bytes, num_bytes, handles, num_handles)
}

#[inline]
fn channel_call_epilogue<R: ReadHandles>(
    proc: &ProcessDispatcher,
    reply: MessagePacketPtr,
    args: &RawChannelCallArgs,
    rd_handles: R,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    let num_bytes = reply.data_size() as u32;
    let num_handles = reply.num_handles() as u32;

    if args.rd_num_bytes < num_bytes || args.rd_num_handles < num_handles {
        return Err(Status::BUFFER_TOO_SMALL);
    }

    actual_bytes.write(num_bytes)?;
    actual_handles.write(num_handles)?;
    copy_out_message(proc, reply, args.rd_bytes(), rd_handles)
}

#[allow(clippy::too_many_arguments)]
#[inline]
fn channel_call_noretry<W: WriteHandles, R: ReadHandles>(
    handle: HandleValue,
    options: u32,
    deadline: zx_instant_mono_t,
    args: RawChannelCallArgs,
    wr_handles: W,
    rd_handles: R,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    let proc = ProcessDispatcher::get_current();
    let (channel, owner, msg) = prepare_write(
        &proc,
        handle,
        zx_types::ZX_RIGHT_WRITE | zx_types::ZX_RIGHT_READ,
        options,
        args.wr_bytes(),
        args.wr_num_bytes,
        wr_handles,
        args.wr_num_handles,
        size_of::<zx_txid_t>(),
    )?;

    let deadline = Deadline::new(InstantUnknown(deadline), proc.get_timer_slack_policy());

    // Write message and wait for reply, deadline, or cancellation.
    let reply = ChannelDispatcher::call(channel, owner, msg, deadline)?;
    channel_call_epilogue(&proc, reply, &args, rd_handles, actual_bytes, actual_handles)
}

#[inline]
fn channel_call_finish<R: ReadHandles>(
    deadline: zx_instant_mono_t,
    args: RawChannelCallArgs,
    rd_handles: R,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    let waiter = ThreadDispatcher::get_current_message_waiter();
    let proc = ProcessDispatcher::get_current();
    let deadline = Deadline::new(InstantUnknown(deadline), proc.get_timer_slack_policy());
    let reply = ChannelDispatcher::resume_interrupted_call(&waiter, deadline)?;
    channel_call_epilogue(&proc, reply, &args, rd_handles, actual_bytes, actual_handles)
}

#[syscall]
pub fn sys_channel_call_noretry(
    handle: HandleValue,
    options: u32,
    deadline: zx_instant_mono_t,
    args: UserInPtr<zx_channel_call_args_t>,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    let call_args = args.reinterpret::<RawChannelCallArgs>().read()?;

    channel_call_noretry(
        handle,
        options,
        deadline,
        call_args,
        call_args.wr_handles(),
        call_args.rd_handles(),
        actual_bytes,
        actual_handles,
    )
}

#[syscall]
pub fn sys_channel_call_finish(
    deadline: zx_instant_mono_t,
    args: UserInPtr<zx_channel_call_args_t>,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    let call_args = args.reinterpret::<RawChannelCallArgs>().read()?;

    channel_call_finish(deadline, call_args, call_args.rd_handles(), actual_bytes, actual_handles)
}

#[syscall]
pub fn sys_channel_call_etc_noretry(
    handle: HandleValue,
    options: u32,
    deadline: zx_instant_mono_t,
    args: UserInOutPtr<zx_channel_call_etc_args_t>,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    let call_args = args.reinterpret::<RawChannelCallArgs>().read()?;

    channel_call_noretry(
        handle,
        options,
        deadline,
        call_args,
        call_args.wr_handle_dispositions(),
        call_args.rd_handle_infos(),
        actual_bytes,
        actual_handles,
    )
}

#[syscall]
pub fn sys_channel_call_etc_finish(
    deadline: zx_instant_mono_t,
    args: UserInOutPtr<zx_channel_call_etc_args_t>,
    actual_bytes: UserOutPtr<u32>,
    actual_handles: UserOutPtr<u32>,
) -> Result<(), Status> {
    let call_args = args.reinterpret::<RawChannelCallArgs>().read()?;

    channel_call_finish(
        deadline,
        call_args,
        call_args.rd_handle_infos(),
        actual_bytes,
        actual_handles,
    )
}
