// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{
    CurrentTask, EventHandler, Pid, ProcessEntryRef, SignalHandler, SignalHandlerInner,
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
    /// The process represented by this file.
    pid: Pid,

    /// Receives a notification when the tracked process terminates.
    /// `None` if the process was already terminated when the pidfd was created.
    terminated_event: Option<zx::EventPair>,
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
    pid: Pid,
    flags: OpenFlags,
) -> Result<FileHandle, Errno> {
    let terminated_event = match pid.get_process() {
        Some(ProcessEntryRef::Process(proc)) => {
            // Ideally monitor the ThreadGroup's drop_notifier instead, but the pidfd must not be
            // signalled until after all memory resources associated with the process are
            // released. In the current Starnix codebase, there is a 1:1 correspondence between
            // ThreadGroups (i.e. processes) and MemoryManagers, and the MemoryManager of a process
            // may outlive the ThreadGroup in some circumstances. Therefore, as a temporary
            // workaround, monitor the MemoryManager's drop_notifier, which is guaranteed to only
            // fire when all the memory mappings associated with the process have been released.
            // To be revisited once Starnix implements explicit cleanup of resources on process exit.
            let task = pid.get_task().or_else(|_| proc.read().get_running_task());
            let mm = task.and_then(|task| task.mm());
            mm.ok().map(|mm| mm.drop_notifier.event())
        }
        Some(ProcessEntryRef::Zombie) => None,
        None => {
            if pid.get_task().is_ok() {
                return error!(EINVAL);
            }
            return error!(ESRCH);
        }
    };

    Ok(Anon::new_private_file(
        current_task,
        Box::new(PidFdFileObject { pid, terminated_event }),
        flags,
        "[pidfd]",
    ))
}

impl FileOps for PidFdFileObject {
    fileops_impl_nonseekable!();
    fileops_impl_dataless!();
    fileops_impl_noop_sync!();

    fn as_pid(&self, _file: &FileObject) -> Result<Pid, Errno> {
        Ok(self.pid.clone())
    }

    fn wait_async(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        waiter: &Waiter,
        events: FdEvents,
        handler: EventHandler,
    ) -> Option<WaitCanceler> {
        let terminated_event = self.terminated_event.as_ref()?;
        let signal_handler = SignalHandler {
            inner: SignalHandlerInner::ZxHandle(PidFdFileObject::get_events_from_signals),
            event_handler: handler,
            err_code: None,
        };
        let canceler = waiter
            .wake_on_zircon_signals(
                terminated_event,
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
        let Some(terminated_event) = &self.terminated_event else {
            return Ok(FdEvents::POLLIN);
        };
        match terminated_event
            .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::ZERO)
            .to_result()
        {
            Err(zx::Status::TIMED_OUT) => Ok(FdEvents::empty()),
            Ok(zx::Signals::EVENTPAIR_PEER_CLOSED) => Ok(FdEvents::POLLIN),
            result => unreachable!("unexpected result: {result:?}"),
        }
    }
}
