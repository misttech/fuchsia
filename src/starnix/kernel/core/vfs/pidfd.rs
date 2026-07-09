// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::mm::MemoryManager;
use crate::task::{
    CurrentTask, EventHandler, SignalHandler, SignalHandlerInner, ThreadGroup, ThreadGroupKey,
    WaitCanceler, Waiter,
};
use crate::vfs::{
    Anon, FileHandle, FileObject, FileOps, fileops_impl_dataless, fileops_impl_nonseekable,
    fileops_impl_noop_sync,
};
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::FdEvents;

pub struct PidFdFileObject {
    /// The key of the task represented by this file.
    tg: ThreadGroupKey,

    // Receives a notification when the tracked process terminates.
    terminated_event: zx::EventPair,
}

impl PidFdFileObject {
    fn get_signals_from_events(events: FdEvents) -> zx::Signals {
        if events.contains(FdEvents::POLLIN) {
            zx::Signals::EVENTPAIR_PEER_CLOSED
        } else {
            zx::Signals::NONE
        }
    }

    fn get_events_from_signals(signals: zx::Signals) -> FdEvents {
        let mut events = FdEvents::empty();

        if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED) {
            events |= FdEvents::POLLIN;
        }

        events
    }
}

pub fn new_pidfd(
    current_task: &CurrentTask,
    proc: &ThreadGroup,
    mm: &MemoryManager,
    flags: OpenFlags,
) -> FileHandle {
    // We should really be monitoring the ThreadGroup's drop_notifier instead, but we also need to
    // ensure that we're not signalling the pidfd until after all memory resources associated with
    // the process are released. In the current Starnix codebase, there is a 1:1 correspondence
    // between ThreadGroups (i.e. processes) and MemoryManagers, and the MemoryManager of a process
    // may outlive the ThreadGroup in some circumstances. Therefore, as a temporary workaround, here
    // we monitor the MemoryManager's drop_notifier, which is guaranteed to only fire when all the
    // memory mappings associated with the process have been released. To be revisited once Starnix
    // implements explicit cleanup of resources on process exit.
    let terminated_event = mm.drop_notifier.event();

    Anon::new_private_file(
        current_task,
        Box::new(PidFdFileObject { tg: proc.into(), terminated_event }),
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
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        let signal_handler = SignalHandler {
            inner: SignalHandlerInner::ZxHandle(PidFdFileObject::get_events_from_signals),
            event_handler: handler,
            err_code: None,
        };
        let canceler = waiter
            .wake_on_zircon_signals(
                &self.terminated_event,
                PidFdFileObject::get_signals_from_events(events),
                signal_handler,
            )
            .unwrap(); // errors cannot happen unless the kernel is out of memory
        Some(WaitCanceler::new_port(canceler))
    }

    fn query_events(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        match self
            .terminated_event
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
            .to_result()
        {
            Err(zx::Status::TIMED_OUT) => Ok(FdEvents::empty()),
            Ok(zx::Signals::EVENTPAIR_PEER_CLOSED) => Ok(FdEvents::POLLIN),
            result => unreachable!("unexpected result: {result:?}"),
        }
    }
}

pub fn new_zombie_pidfd(current_task: &CurrentTask, flags: OpenFlags) -> FileHandle {
    Anon::new_private_file(current_task, Box::new(ZombiePidFdFileObject {}), flags, "[pidfd]")
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
        _file: &FileObject,
        _current_task: &CurrentTask,
    ) -> Result<FdEvents, Errno> {
        Ok(FdEvents::POLLIN)
    }
}
