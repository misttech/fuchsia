// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::thread::AutoExpiringPreemptDisabler;
use crate::object::{HandleValue, ProcessDispatcher, remove_user_handles};
use crate::user_copy::UserInPtr;
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_HANDLE_INVALID, ZX_RIGHT_DUPLICATE, ZX_RIGHT_SAME_RIGHTS, zx_handle_t, zx_rights_t,
};

const LOCAL_TRACE: u32 = 0;

// zx_status_t zx_handle_close
#[syscall]
pub fn sys_handle_close(handle_value: HandleValue) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle_value.raw_value());

    // Closing the "never a handle" invalid handle is not an error
    // It's like free(NULL).
    if handle_value.raw_value() == ZX_HANDLE_INVALID {
        return Ok(());
    }
    let up = ProcessDispatcher::get_current();
    let _handle = up.handle_table().remove_handle(&up, handle_value).ok_or(Status::BAD_HANDLE)?;
    Ok(())
}

// zx_status_t zx_handle_close_many
#[syscall]
pub fn sys_handle_close_many(
    handles: UserInPtr<zx_handle_t>,
    num_handles: usize,
) -> Result<(), Status> {
    ltracef!("handles {:p}, num_handles {}\n", handles.as_ptr(), num_handles);

    let up = ProcessDispatcher::get_current();
    remove_user_handles(handles, num_handles, &up)
}

fn handle_dup_replace(
    is_replace: bool,
    handle_value: HandleValue,
    mut rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle_value.raw_value());

    let up = ProcessDispatcher::get_current();

    let _preempt_disable = AutoExpiringPreemptDisabler::with_default_timeslice_extension();
    ksync::lock!(let mut guard = up.handle_table().write_lock());
    let source = up
        .handle_table()
        .get_handle_entry_locked(guard.as_mut().token_mut(), &up, handle_value)
        .ok_or(Status::BAD_HANDLE)?;

    if !is_replace && !source.as_ref().has_rights(ZX_RIGHT_DUPLICATE) {
        return Err(Status::ACCESS_DENIED);
    }

    if rights == ZX_RIGHT_SAME_RIGHTS {
        rights = source.as_ref().rights();
    } else if (source.as_ref().rights() & rights) != rights {
        if is_replace {
            let _ = source.remove();
        }
        return Err(Status::INVALID_ARGS);
    }

    let handle = source.as_ref().dup(rights)?;

    if is_replace {
        let _ = source.remove();
    }

    *out = up.handle_table().map_handle_to_value(handle.as_ref());
    up.handle_table().add_handle_locked(guard.as_mut().token_mut(), handle);
    Ok(())
}

// zx_status_t zx_handle_duplicate
#[syscall]
pub fn sys_handle_duplicate(
    handle_value: HandleValue,
    rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    handle_dup_replace(false, handle_value, rights, out)
}

// zx_status_t zx_handle_replace
#[syscall]
pub fn sys_handle_replace(
    handle_value: HandleValue,
    rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    handle_dup_replace(true, handle_value, rights, out)
}
