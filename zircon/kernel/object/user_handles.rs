// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::ChannelDispatcher;
use super::handle::{HandleOwner, HandleRef, HandleValue};
use super::handle_table::HandleTableLockClass;
use super::message_packet::MessagePacket;
use super::process_dispatcher::ProcessDispatcher;
use crate::kernel::thread::AutoExpiringPreemptDisabler;
use crate::user_copy::{UserInOutPtr, UserInPtr, UserOutPtr};
use core::mem::{MaybeUninit, align_of, size_of};
use core::ops::Deref;
use core::ptr::{self, NonNull};
use core::{cmp, slice};
use ksync::LockToken;
use zerocopy::{FromBytes, Immutable, IntoBytes};
use zx_status::Status;
use zx_types::{
    ZX_CHANNEL_MAX_MSG_HANDLES, ZX_HANDLE_INVALID, ZX_HANDLE_OP_DUPLICATE, ZX_HANDLE_OP_MOVE,
    ZX_OBJ_TYPE_NONE, ZX_RIGHT_DUPLICATE, ZX_RIGHT_SAME_RIGHTS, ZX_RIGHT_TRANSFER,
    zx_handle_disposition_t, zx_handle_info_t, zx_handle_op_t, zx_handle_t, zx_obj_type_t,
    zx_rights_t, zx_status_t,
};

#[repr(C)]
#[derive(Copy, Clone, FromBytes, IntoBytes, Immutable)]
struct RawHandleInfo {
    handle: zx_handle_t,
    type_: zx_obj_type_t,
    rights: zx_rights_t,
    unused: u32,
}

zr::static_assert_size_and_align!(
    RawHandleInfo,
    size_of::<zx_handle_info_t>(),
    align_of::<zx_handle_info_t>(),
);

#[repr(C)]
#[derive(Copy, Clone, FromBytes, IntoBytes, Immutable)]
struct RawHandleDisposition {
    operation: zx_handle_op_t,
    handle: zx_handle_t,
    type_: zx_obj_type_t,
    rights: zx_rights_t,
    result: zx_status_t,
}

zr::static_assert_size_and_align!(
    RawHandleDisposition,
    size_of::<zx_handle_disposition_t>(),
    align_of::<zx_handle_disposition_t>(),
);

/// Returns an iterator yielding a [`HandleRef`] for each handle currently stored in `msg`.
#[inline]
pub(super) fn msg_handle_refs(msg: &MessagePacket) -> impl Iterator<Item = HandleRef<'_>> {
    let handles_ptr = msg.handles();
    (0..msg.num_handles()).map(move |i| {
        // SAFETY: `i < msg.num_handles()`, and a `MessagePacket` queued in or read from a channel
        // holds `num_handles` valid, non-null `Handle*` pointers whose lifetime is tied to `msg`.
        unsafe { HandleRef::from_raw(NonNull::new_unchecked(*handles_ptr.add(i))) }
    })
}

/// Completes transferring the handles from `msg` into `proc`'s handle table after their values
/// have been copied to userspace.
fn finish_get_handles(proc: &ProcessDispatcher, msg: &mut MessagePacket) {
    // The MessagePacket currently owns the handle. Only after transferring the handles into this
    // process's handle table can we relieve MessagePacket of its handle ownership responsibility.
    for handle_ref in msg_handle_refs(msg) {
        let disp = handle_ref.dispatcher_ref();
        if disp.is_waitable() {
            // Cancel any waiters on this handle prior to adding it to the process's handle table.
            disp.cancel(handle_ref);
            // If this handle refers to a channel, cancel any channel_call waits.
            if let Some(channel) = disp.downcast::<ChannelDispatcher>() {
                channel.cancel_message_waiters();
            }
        }
    }

    let num_handles = msg.num_handles();
    let handles_ptr = msg.handles();
    {
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        let handle_table = proc.handle_table();
        ksync::lock!(let mut guard = handle_table.write_lock());
        for i in 0..num_handles {
            // SAFETY: `i < num_handles`, and ownership of each `Handle*` is transferred from `msg`
            // to `proc`'s handle table (`msg.set_owns_handles(false)` is called below).
            let handle_owner = unsafe { HandleOwner::from_raw(*handles_ptr.add(i)).unwrap() };
            handle_table.add_handle_locked(guard.as_mut().token_mut(), handle_owner);
        }
    }

    msg.set_owns_handles(false);
}

fn get_handles_with<T: IntoBytes + Immutable>(
    user_out: UserOutPtr<T>,
    proc: &ProcessDispatcher,
    msg: &mut MessagePacket,
    mut map_handle: impl FnMut(HandleRef<'_>) -> T,
) -> Result<(), Status> {
    let num_handles = msg.num_handles();
    let mut hvs = [const { MaybeUninit::<T>::uninit() }; ZX_CHANNEL_MAX_MSG_HANDLES as usize];
    for (dst, handle_ref) in hvs.iter_mut().zip(msg_handle_refs(msg)) {
        dst.write(map_handle(handle_ref));
    }

    // SAFETY: The first `num_handles` elements of `hvs` were initialized above.
    let hvs = unsafe { slice::from_raw_parts(hvs.as_ptr().cast(), num_handles) };
    user_out.copy_slice_to_user(hvs)?;
    finish_get_handles(proc, msg);
    Ok(())
}

/// Userspace output handle buffer (`zx_handle_t` or `zx_handle_info_t`) for channel reads.
pub trait ReadHandles: Copy {
    /// Removes the handles from `msg`, installs them in `proc`'s handle table, and copies them out
    /// to the user array `self`.
    ///
    /// Upon completion, the `Handle` object will either be owned by the process (success) or closed
    /// (error).
    fn get_handles(self, proc: &ProcessDispatcher, msg: &mut MessagePacket) -> Result<(), Status>;
}

impl ReadHandles for UserOutPtr<zx_handle_t> {
    fn get_handles(self, proc: &ProcessDispatcher, msg: &mut MessagePacket) -> Result<(), Status> {
        let handle_table = proc.handle_table();
        get_handles_with(self, proc, msg, |handle_ref| {
            handle_table.map_handle_to_value(handle_ref).raw_value()
        })
    }
}

impl ReadHandles for UserOutPtr<zx_handle_info_t> {
    fn get_handles(self, proc: &ProcessDispatcher, msg: &mut MessagePacket) -> Result<(), Status> {
        let handle_table = proc.handle_table();
        get_handles_with(self.reinterpret::<RawHandleInfo>(), proc, msg, |handle_ref| {
            let disp = handle_ref.dispatcher_ref();
            RawHandleInfo {
                handle: handle_table.map_handle_to_value(handle_ref).raw_value(),
                type_: disp.get_type(),
                rights: handle_ref.rights(),
                unused: 0,
            }
        })
    }
}

/// Basic checks for `handle` to be able to be sent via `channel`.
#[inline]
fn common_handle_checks_locked(
    handle: HandleRef<'_>,
    channel: &ChannelDispatcher,
    desired_rights: zx_rights_t,
    type_: zx_obj_type_t,
) -> Result<(), Status> {
    if !handle.has_rights(ZX_RIGHT_TRANSFER) {
        return Err(Status::ACCESS_DENIED);
    }
    let disp = handle.dispatcher_ref();
    if ptr::eq(disp, channel.deref()) {
        return Err(Status::NOT_SUPPORTED);
    }
    if type_ != ZX_OBJ_TYPE_NONE && disp.get_type() != type_ {
        return Err(Status::WRONG_TYPE);
    }
    if desired_rights != ZX_RIGHT_SAME_RIGHTS
        && (handle.rights() & desired_rights) != desired_rights
    {
        return Err(Status::INVALID_ARGS);
    }
    Ok(())
}

#[inline]
fn move_handle_for_transfer(
    mut handle: HandleOwner,
    channel: &ChannelDispatcher,
    type_: zx_obj_type_t,
    desired_rights: zx_rights_t,
) -> Result<HandleOwner, Status> {
    common_handle_checks_locked(handle.as_ref(), channel, desired_rights, type_)?;

    // If the caller has requested a different set of rights, we have to mint a new handle for them.
    if desired_rights != ZX_RIGHT_SAME_RIGHTS && desired_rights != handle.rights() {
        // common_handle_checks_locked verifies that the desired rights are a subset of the handle's
        // current rights.
        handle = handle.dup(desired_rights)?;
    }

    Ok(handle)
}

#[inline]
fn duplicate_handle_for_transfer(
    source: HandleRef<'_>,
    channel: &ChannelDispatcher,
    type_: zx_obj_type_t,
    desired_rights: zx_rights_t,
) -> Result<HandleOwner, Status> {
    common_handle_checks_locked(source, channel, desired_rights, type_)?;

    if !source.has_rights(ZX_RIGHT_DUPLICATE) {
        return Err(Status::ACCESS_DENIED);
    }

    // common_handle_checks_locked verifies that the desired rights are a subset of the handle's
    // current rights.
    let dest_rights =
        if desired_rights == ZX_RIGHT_SAME_RIGHTS { source.rights() } else { desired_rights };

    source.dup(dest_rights)
}

/// Returns a handle that should be sent over `channel`. In case of error, the return value should
/// be reflected back to the user.
// This helper is used by zx_channel_write.
#[inline]
fn get_handle_for_message_locked(
    token: &mut LockToken<'_, HandleTableLockClass>,
    proc: &ProcessDispatcher,
    channel: &ChannelDispatcher,
    handle_val: zx_handle_t,
) -> Result<HandleOwner, Status> {
    let source = proc
        .handle_table()
        .remove_handle_locked(token, proc, HandleValue::new(handle_val))
        .ok_or(Status::BAD_HANDLE)?;
    move_handle_for_transfer(source, channel, ZX_OBJ_TYPE_NONE, ZX_RIGHT_SAME_RIGHTS)
}

/// Returns a handle that should be sent over `channel`. In case of error, the return value should
/// be reflected back to the user.
// This helper is used by zx_channel_write_etc.
#[inline]
fn get_handle_disposition_for_message_locked(
    token: &mut LockToken<'_, HandleTableLockClass>,
    proc: &ProcessDispatcher,
    channel: &ChannelDispatcher,
    handle_disposition: &mut RawHandleDisposition,
) -> Result<HandleOwner, Status> {
    // The documentation for zx_channel_write_etc says this about the operation performed on
    // handles:
    //
    // * `ZX_HANDLE_OP_MOVE` This is equivalent to first issuing `zx_handle_replace()` then
    //   `zx_channel_write()`. The source handle is always closed.
    //
    // * `ZX_HANDLE_OP_DUPLICATE` This is equivalent to first issuing `zx_handle_duplicate()`
    //   then `zx_channel_write()`. The source handle always remains open and accessible to
    //   the caller.
    //
    // So when duplicating a handle, we leave the source handle in the handle table. For all
    // other operations (including invalid operations) we immediately remove the source handle
    // from the handle table and then attempt to move it.
    let handle_table = proc.handle_table();
    let handle_val = HandleValue::new(handle_disposition.handle);
    let res = (|| match handle_disposition.operation {
        ZX_HANDLE_OP_DUPLICATE => {
            let source = handle_table
                .get_handle_locked(token, proc, handle_val)
                .ok_or(Status::BAD_HANDLE)?;
            duplicate_handle_for_transfer(
                source,
                channel,
                handle_disposition.type_,
                handle_disposition.rights,
            )
        }
        ZX_HANDLE_OP_MOVE => {
            let source = handle_table
                .remove_handle_locked(token, proc, handle_val)
                .ok_or(Status::BAD_HANDLE)?;
            move_handle_for_transfer(
                source,
                channel,
                handle_disposition.type_,
                handle_disposition.rights,
            )
        }
        _ => {
            let _ = handle_table
                .remove_handle_locked(token, proc, handle_val)
                .ok_or(Status::BAD_HANDLE)?;
            Err(Status::INVALID_ARGS)
        }
    })();
    res.inspect_err(|err| {
        handle_disposition.result = err.into_raw();
    })
}

fn put_handles_from_user<'a, T: FromBytes>(
    user_in: UserInPtr<T>,
    buf: &'a mut [MaybeUninit<T>; ZX_CHANNEL_MAX_MSG_HANDLES as usize],
    proc: &ProcessDispatcher,
    msg: &mut MessagePacket,
    mut get_handle: impl FnMut(
        &mut LockToken<'_, HandleTableLockClass>,
        &mut T,
    ) -> Result<HandleOwner, Status>,
) -> Result<(&'a mut [T], Result<(), Status>), Status> {
    let num_handles = msg.num_handles();
    debug_assert!(num_handles <= ZX_CHANNEL_MAX_MSG_HANDLES as usize); // This must be checked before calling.
    // `msg` is created with `owns_handles = false`, so if copying from userspace fails before any
    // handles are written to `handles_ptr`, `MessagePacket::drop` will not inspect `handles_ptr`.
    let items = user_in.copy_slice_from_user(&mut buf[..num_handles])?;

    let handles_ptr = msg.handles_mut();

    let mut result = Ok(());
    {
        let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
        ksync::lock!(let mut guard = proc.handle_table().write_lock());
        for (ix, item) in items.iter_mut().enumerate() {
            let handle = get_handle(guard.as_mut().token_mut(), item);
            if let Err(err) = handle
                && result.is_ok()
            {
                // Latch the first error encountered. It will be what the function returns.
                result = Err(err);
            }
            let raw_ptr = handle.map_or(ptr::null_mut(), HandleOwner::release);
            // SAFETY: `ix < num_handles`, and `handles_ptr` points to `num_handles` writable
            // entries in `msg`.
            unsafe {
                *handles_ptr.add(ix) = raw_ptr;
            }
        }
    }

    msg.set_owns_handles(true);
    Ok((items, result))
}

/// Userspace input handle buffer (`zx_handle_t` or `zx_handle_disposition_t`) for channel writes.
pub trait WriteHandles: Copy {
    /// Removes the handles pointed by `self` from `proc`. It only stops early if copying from user
    /// memory fails.
    fn remove_user_handles(self, proc: &ProcessDispatcher, num_handles: u32);

    /// Transfers handles or handle dispositions from `self` in `proc`'s handle table into `msg`.
    ///
    /// For `zx_channel_write` or `zx_channel_write_etc` with the `ZX_HANDLE_OP_MOVE` flag, handles
    /// are closed whether success or failure. For `zx_channel_write_etc` with the
    /// `ZX_HANDLE_OP_DUPLICATE` flag, handles always remain open.
    fn put_handles(
        self,
        proc: &ProcessDispatcher,
        channel: &ChannelDispatcher,
        msg: &mut MessagePacket,
    ) -> Result<(), Status>;
}

impl WriteHandles for UserInPtr<zx_handle_t> {
    #[cold]
    fn remove_user_handles(self, proc: &ProcessDispatcher, num_handles: u32) {
        let num_handles = num_handles as usize;
        let mut handles =
            [MaybeUninit::<zx_handle_t>::uninit(); ZX_CHANNEL_MAX_MSG_HANDLES as usize];
        let mut offset = 0;
        // We process `num_handles` in chunks of `ZX_CHANNEL_MAX_MSG_HANDLES` because we don't have
        // a limit on how large `num_handles` can be.
        while offset < num_handles {
            let chunk_size = cmp::min(num_handles - offset, ZX_CHANNEL_MAX_MSG_HANDLES as usize);
            let Ok(chunk) =
                self.element_offset(offset).copy_slice_from_user(&mut handles[..chunk_size])
            else {
                break;
            };
            let _ = proc.handle_table().remove_handles(proc, chunk);
            offset += chunk_size;
        }
    }

    fn put_handles(
        self,
        proc: &ProcessDispatcher,
        channel: &ChannelDispatcher,
        msg: &mut MessagePacket,
    ) -> Result<(), Status> {
        let mut handles =
            [MaybeUninit::<zx_handle_t>::uninit(); ZX_CHANNEL_MAX_MSG_HANDLES as usize];
        let (_handles, result) =
            put_handles_from_user(self, &mut handles, proc, msg, |guard, &mut handle_val| {
                get_handle_for_message_locked(guard, proc, channel, handle_val)
            })?;
        result
    }
}

impl WriteHandles for UserInOutPtr<zx_handle_disposition_t> {
    #[cold]
    fn remove_user_handles(self, proc: &ProcessDispatcher, num_handles: u32) {
        let num_handles = num_handles as usize;
        let user_dispositions = self.reinterpret::<RawHandleDisposition>();
        let mut local_dispositions =
            [MaybeUninit::<RawHandleDisposition>::uninit(); ZX_CHANNEL_MAX_MSG_HANDLES as usize];
        let mut handles =
            [MaybeUninit::<zx_handle_t>::uninit(); ZX_CHANNEL_MAX_MSG_HANDLES as usize];
        let mut offset = 0;
        // We process `num_handles` in chunks of `ZX_CHANNEL_MAX_MSG_HANDLES` because we don't have
        // a limit on how large `num_handles` can be.
        while offset < num_handles {
            let chunk_size = cmp::min(num_handles - offset, ZX_CHANNEL_MAX_MSG_HANDLES as usize);
            // Extract the handles that would be consumed on syscalls with handle_release semantics
            // from the offset..offset + chunk_size slice of user_handles.
            let Ok(dispositions) = user_dispositions
                .element_offset(offset)
                .copy_slice_from_user(&mut local_dispositions[..chunk_size])
            else {
                break;
            };
            for (dst, disp) in handles[..chunk_size].iter_mut().zip(dispositions.iter()) {
                // !ZX_HANDLE_OP_DUPLICATE is used to capture the case where we failed due to a bad
                // operational arg.
                let hv = if disp.operation != ZX_HANDLE_OP_DUPLICATE {
                    disp.handle
                } else {
                    ZX_HANDLE_INVALID
                };
                dst.write(hv);
            }
            // SAFETY: The first `chunk_size` elements of `handles` were initialized above.
            let handles = unsafe { slice::from_raw_parts(handles.as_ptr().cast(), chunk_size) };
            let _ = proc.handle_table().remove_handles(proc, handles);
            offset += chunk_size;
        }
    }

    fn put_handles(
        self,
        proc: &ProcessDispatcher,
        channel: &ChannelDispatcher,
        msg: &mut MessagePacket,
    ) -> Result<(), Status> {
        let user_dispositions = self.reinterpret::<RawHandleDisposition>();
        let mut dispositions =
            [MaybeUninit::<RawHandleDisposition>::uninit(); ZX_CHANNEL_MAX_MSG_HANDLES as usize];
        let (dispositions, result) = put_handles_from_user(
            user_dispositions.as_in_ptr(),
            &mut dispositions,
            proc,
            msg,
            |guard, disposition| {
                get_handle_disposition_for_message_locked(guard, proc, channel, disposition)
            },
        )?;

        // For zx_channel_write_etc, copy out to convey zx_status_t result on failure. The caller is
        // expected to have initialized the result to ZX_OK (mentioned in the user docs) to save
        // cycles for the success case.
        if result.is_err() {
            user_dispositions.copy_slice_to_user(dispositions)?;
        }
        result
    }
}
