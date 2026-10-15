// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::object::{Dispatcher, HandleValue, JobDispatcher, ProcessDispatcher};
use debug::ltracef;
use syscalls_macro::syscall;
use zx_status::Status;
use zx_types::{
    ZX_RIGHT_ENUMERATE, ZX_RIGHT_SAME_RIGHTS, ZX_RIGHT_SIGNAL, ZX_RIGHT_SIGNAL_PEER, zx_rights_t,
};

const LOCAL_TRACE: u32 = 0;

// zx_status_t zx_object_signal
#[syscall]
pub fn sys_object_signal(
    handle_value: HandleValue,
    clear_mask: u32,
    set_mask: u32,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle_value.raw_value());

    let up = ProcessDispatcher::get_current();

    let dispatcher = up.handle_table().get_dispatcher_with_rights::<Dispatcher>(
        &up,
        handle_value,
        ZX_RIGHT_SIGNAL,
    )?;

    dispatcher.user_signal_self(clear_mask, set_mask)
}

// zx_status_t zx_object_signal_peer
#[syscall]
pub fn sys_object_signal_peer(
    handle_value: HandleValue,
    clear_mask: u32,
    set_mask: u32,
) -> Result<(), Status> {
    ltracef!("handle {:x}\n", handle_value.raw_value());

    let up = ProcessDispatcher::get_current();

    let dispatcher = up.handle_table().get_dispatcher_with_rights::<Dispatcher>(
        &up,
        handle_value,
        ZX_RIGHT_SIGNAL_PEER,
    )?;

    dispatcher.user_signal_peer(clear_mask, set_mask)
}

// Given a kernel object with children objects, obtain a handle to the
// child specified by the provided kernel object id.
// zx_status_t zx_object_get_child
#[syscall]
pub fn sys_object_get_child(
    handle: HandleValue,
    koid: u64,
    mut rights: zx_rights_t,
    out: &mut HandleValue,
) -> Result<(), Status> {
    let up = ProcessDispatcher::get_current();

    let (dispatcher, parent_rights) =
        up.handle_table().get_dispatcher_and_rights::<Dispatcher>(&up, handle)?;

    if (parent_rights & ZX_RIGHT_ENUMERATE) == 0 {
        return Err(Status::ACCESS_DENIED);
    }

    if rights == ZX_RIGHT_SAME_RIGHTS {
        rights = parent_rights;
    } else if (parent_rights & rights) != rights {
        return Err(Status::ACCESS_DENIED);
    }

    // TODO(https://fxbug.dev/42175105): Constructing the handles below may cause the handle count
    // to go from 0->1, resulting in multiple on_zero_handles invocations. Presently this is benign,
    // except for one scenario with processes in the initial state. Such processes are filtered out
    // by the SimpleJobEnumerator and should not be able to be learned about. Further protection
    // against guessing is not performed here since the worst case scenario is a misbehaving
    // privileged process guessing a koid and destroying a process that was in construction.
    let process = dispatcher.downcast::<ProcessDispatcher>();
    if let Some(process) = process {
        let thread = process.lookup_thread_by_id(koid).ok_or(Status::NOT_FOUND)?;
        return up.make_and_add_handle_from_ref(thread, rights).map(|h| *out = h);
    }

    let job = dispatcher.downcast::<JobDispatcher>();
    if let Some(job) = job {
        let child = job.lookup_job_by_id(koid);
        if let Some(child) = child {
            return up.make_and_add_handle_from_ref(child, rights).map(|h| *out = h);
        }
        let proc = job.lookup_process_by_id(koid);
        if let Some(proc) = proc {
            return up.make_and_add_handle_from_ref(proc, rights).map(|h| *out = h);
        }
        return Err(Status::NOT_FOUND);
    }

    Err(Status::WRONG_TYPE)
}
