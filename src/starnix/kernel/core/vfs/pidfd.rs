// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{
    CurrentTask, EventHandler, ThreadGroup, ThreadGroupKey, ThreadGroupLifecycleWaitValue,
    WaitCanceler, Waiter,
};
use crate::vfs::{
    Anon, FileHandle, FileObject, FileOps, fileops_impl_dataless, fileops_impl_nonseekable,
    fileops_impl_noop_sync,
};
use starnix_sync::{FileOpsCore, LockEqualOrBefore, Locked};
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::FdEvents;

pub struct PidFdFileObject {
    /// The key of the task represented by this file.
    tg: ThreadGroupKey,
}

pub fn new_pidfd<L>(
    locked: &mut Locked<L>,
    current_task: &CurrentTask,
    proc: &ThreadGroup,
    flags: OpenFlags,
) -> FileHandle
where
    L: LockEqualOrBefore<FileOpsCore>,
{
    Anon::new_private_file(
        locked,
        current_task,
        Box::new(PidFdFileObject { tg: proc.into() }),
        flags,
        "[pidfd]",
    )
}

impl FileOps for PidFdFileObject {
    fileops_impl_nonseekable!();
    fileops_impl_dataless!();
    fileops_impl_noop_sync!();

    fn as_thread_group_key(&self, _file: &FileObject) -> Result<ThreadGroupKey, Errno> {
        Ok(self.tg.clone())
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        _events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        let Some(tg) = self.tg.upgrade() else {
            waiter.wake_immediately(FdEvents::POLLIN, handler);
            return None;
        };
        let state = tg.read();
        if state.is_exited() {
            waiter.wake_immediately(FdEvents::POLLIN, handler);
            None
        } else {
            Some(state.lifecycle_waiters.wait_async_value_with_handler(
                waiter,
                ThreadGroupLifecycleWaitValue::Exited,
                handler,
            ))
        }
    }

    fn query_events(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(match self.tg.upgrade() {
            Some(tg) => {
                if tg.read().is_exited() {
                    FdEvents::POLLIN
                } else {
                    FdEvents::empty()
                }
            }
            None => FdEvents::POLLIN,
        })
    }
}

pub fn new_zombie_pidfd<L>(
    locked: &mut Locked<L>,
    current_task: &CurrentTask,
    flags: OpenFlags,
) -> FileHandle
where
    L: LockEqualOrBefore<FileOpsCore>,
{
    Anon::new_private_file(
        locked,
        current_task,
        Box::new(ZombiePidFdFileObject {}),
        flags,
        "[pidfd]",
    )
}

struct ZombiePidFdFileObject {}

impl FileOps for ZombiePidFdFileObject {
    fileops_impl_nonseekable!();
    fileops_impl_dataless!();
    fileops_impl_noop_sync!();

    fn as_thread_group_key(&self, _file: &FileObject) -> Result<ThreadGroupKey, Errno> {
        // There's nothing really reasonable to return here?
        error!(EINVAL)
    }

    fn wait_async(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _waiter: &Waiter,
        _events: FdEvents,
        _handler: EventHandler,
    ) -> Option<WaitCanceler> {
        // There's nothing to wait on; is denying blocking sufficient?
        None
    }

    fn query_events(
        &self,
        _locked: &mut Locked<FileOpsCore>,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(FdEvents::POLLIN)
    }
}
