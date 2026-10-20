// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitflags::bitflags;
use starnix_uapi::{
    PTRACE_EVENT_CLONE, PTRACE_EVENT_EXEC, PTRACE_EVENT_EXIT, PTRACE_EVENT_FORK,
    PTRACE_EVENT_SECCOMP, PTRACE_EVENT_STOP, PTRACE_EVENT_VFORK, PTRACE_EVENT_VFORK_DONE,
    PTRACE_O_EXITKILL, PTRACE_O_SUSPEND_SECCOMP, PTRACE_O_TRACECLONE, PTRACE_O_TRACEEXEC,
    PTRACE_O_TRACEEXIT, PTRACE_O_TRACEFORK, PTRACE_O_TRACESECCOMP, PTRACE_O_TRACESYSGOOD,
    PTRACE_O_TRACEVFORK, PTRACE_O_TRACEVFORKDONE,
};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    #[repr(transparent)]
    pub struct PtraceOptions: u32 {
        const EXITKILL = PTRACE_O_EXITKILL;
        const TRACECLONE = PTRACE_O_TRACECLONE;
        const TRACEEXEC = PTRACE_O_TRACEEXEC;
        const TRACEEXIT = PTRACE_O_TRACEEXIT;
        const TRACEFORK = PTRACE_O_TRACEFORK;
        const TRACESYSGOOD = PTRACE_O_TRACESYSGOOD;
        const TRACEVFORK = PTRACE_O_TRACEVFORK;
        const TRACEVFORKDONE = PTRACE_O_TRACEVFORKDONE;
        const TRACESECCOMP = PTRACE_O_TRACESECCOMP;
        const SUSPEND_SECCOMP = PTRACE_O_SUSPEND_SECCOMP;
    }
}

#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub enum PtraceEvent {
    #[default]
    None = 0,
    Stop = PTRACE_EVENT_STOP,
    Clone = PTRACE_EVENT_CLONE,
    Fork = PTRACE_EVENT_FORK,
    Vfork = PTRACE_EVENT_VFORK,
    VforkDone = PTRACE_EVENT_VFORK_DONE,
    Exec = PTRACE_EVENT_EXEC,
    Exit = PTRACE_EVENT_EXIT,
    Seccomp = PTRACE_EVENT_SECCOMP,
}

impl PtraceEvent {
    pub fn from_option(option: PtraceOptions) -> Self {
        match option {
            PtraceOptions::TRACECLONE => PtraceEvent::Clone,
            PtraceOptions::TRACEFORK => PtraceEvent::Fork,
            PtraceOptions::TRACEVFORK => PtraceEvent::Vfork,
            PtraceOptions::TRACEVFORKDONE => PtraceEvent::VforkDone,
            PtraceOptions::TRACEEXEC => PtraceEvent::Exec,
            PtraceOptions::TRACEEXIT => PtraceEvent::Exit,
            PtraceOptions::TRACESECCOMP => PtraceEvent::Seccomp,
            _ => unreachable!("Bad ptrace event specified"),
        }
    }
}

/// Information about what caused a ptrace-event-stop.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PtraceEventData {
    /// The event that caused the task to stop (e.g., PTRACE_EVENT_FORK or PTRACE_EVENT_EXIT).
    pub event: PtraceEvent,

    /// The message associated with the event (e.g., tid, exit status).
    pub msg: u64,
}

impl PtraceEventData {
    pub fn new(option: PtraceOptions, msg: u64) -> Self {
        Self { event: PtraceEvent::from_option(option), msg }
    }
    pub fn new_from_event(event: PtraceEvent, msg: u64) -> Self {
        Self { event, msg }
    }
}
