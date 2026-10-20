// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use starnix_logging::CATEGORY_STARNIX;
use starnix_uapi::signals::SIGTRAP;
use starnix_uapi::{CLONE_PTRACE, CLONE_UNTRACED, CLONE_VFORK, SIGCHLD, clone_args};

use crate::ptrace::{PtraceEventData, PtraceOptions, StopState, ptrace_attach_from_state};
use crate::signals::{SignalDetail, SignalInfo, send_signal_first};
use crate::task::{CurrentTask, Task};

/// Provides access to ptrace hooks for a [`CurrentTask`].
#[derive(Debug)]
pub struct CurrentTaskPtrace<'a> {
    current_task: &'a mut CurrentTask,
}

impl CurrentTask {
    /// Returns a [`CurrentTaskPtrace`] wrapper to invoke ptrace hooks.
    pub fn ptrace(&mut self) -> CurrentTaskPtrace<'_> {
        CurrentTaskPtrace { current_task: self }
    }
}

impl<'a> CurrentTaskPtrace<'a> {
    /// Invoked upon entering a syscall to handle ptrace syscall entry tracing.
    pub fn on_syscall_enter(self) {
        if self.current_task.trace_syscalls.load(Ordering::Relaxed) {
            fuchsia_trace::duration!(CATEGORY_STARNIX, "ptrace:syscall_enter");
            self.syscall_stop(StopState::SyscallEnterStopping, None);
        }
    }

    /// Invoked upon exiting a syscall to handle ptrace syscall exit tracing.
    pub fn on_syscall_exit(self, is_error: bool) {
        if self.current_task.trace_syscalls.load(Ordering::Relaxed) {
            fuchsia_trace::duration!(CATEGORY_STARNIX, "ptrace:syscall_exit");
            self.syscall_stop(StopState::SyscallExitStopping, Some(is_error));
        }
    }

    #[inline(never)]
    fn syscall_stop(self, stop_state: StopState, is_error: Option<bool>) {
        {
            let mut state = self.current_task.write();
            self.current_task.trace_syscalls.store(false, Ordering::Relaxed);
            let Some(ptrace) = &mut state.ptrace else {
                return;
            };

            let mut sig = SignalInfo::with_detail(
                SIGTRAP,
                (linux_uapi::SIGTRAP | 0x80) as i32,
                SignalDetail::None,
            );
            if ptrace.has_option(PtraceOptions::TRACESYSGOOD) {
                sig.signal.set_ptrace_syscall_bit();
            }
            if let Some(is_error) = is_error {
                ptrace.last_syscall_was_error = is_error;
            }
            state.set_stopped(stop_state, Some(sig), None, None);
        }
        self.current_task.block_if_stopped();
    }

    /// If currently being ptraced with the given option, emit the appropriate
    /// event.  PTRACE_EVENTMSG will return the given message.  Also emits the
    /// appropriate event for execve in the absence of TRACEEXEC.
    ///
    /// Note that the Linux kernel has a documented bug where, if TRACEEXIT is
    /// enabled, SIGKILL will trigger an event.  We do not exhibit this
    /// behavior.
    pub fn on_event(self, trace_kind: PtraceOptions, msg: u64) {
        if trace_kind.is_empty() {
            return;
        }
        {
            let mut state = self.current_task.write();
            let Some(ptrace) = &mut state.ptrace else {
                return;
            };

            if !ptrace.has_option(trace_kind) {
                // If this would be a TRACEEXEC, but TRACEEXEC is not
                // turned on, then send a SIGTRAP.
                if trace_kind == PtraceOptions::TRACEEXEC && !ptrace.is_seized() {
                    // Send a SIGTRAP so that the parent can gain control.
                    send_signal_first(self.current_task, state, SignalInfo::kernel(SIGTRAP));
                }

                return;
            }
            let event_data = PtraceEventData::new(trace_kind, msg);
            let siginfo = SignalInfo::with_detail(
                SIGTRAP,
                (((event_data.event as u32) << 8) | SIGTRAP.number()) as i32,
                SignalDetail::None,
            );
            state.set_stopped(
                StopState::PtraceEventStopping,
                Some(siginfo),
                None,
                Some(event_data),
            );
        }
        self.current_task.block_if_stopped();
    }

    /// For traced tasks, attaches `child_task` to the same tracer if required by the clone
    /// flags/ptrace options, and returns the `PtraceOptions` event to emit after spawning.
    pub fn on_clone(self, clone_args: &clone_args, child_task: &Arc<Task>) -> PtraceOptions {
        let (trace_type, ptrace_state) = {
            let state = self.current_task.read();
            let Some(ptrace) = &state.ptrace else {
                return PtraceOptions::empty();
            };

            // ptrace(2): If the tracee calls clone(2) with the CLONE_VFORK flag,
            // PTRACE_EVENT_VFORK will be delivered instead if PTRACE_O_TRACEVFORK
            // is set, otherwise if the tracee calls clone(2) with the exit signal
            // set to SIGCHLD, PTRACE_EVENT_FORK will be delivered if
            // PTRACE_O_TRACEFORK is set.
            let trace_type = if clone_args.flags & (CLONE_UNTRACED as u64) != 0 {
                PtraceOptions::empty()
            } else if clone_args.flags & (CLONE_VFORK as u64) != 0 {
                PtraceOptions::TRACEVFORK
            } else if clone_args.exit_signal != (SIGCHLD as u64) {
                PtraceOptions::TRACECLONE
            } else {
                PtraceOptions::TRACEFORK
            };

            let trace_type = if ptrace.has_option(trace_type) {
                trace_type
            } else if clone_args.flags & (CLONE_PTRACE as u64) != 0 {
                PtraceOptions::empty()
            } else {
                return PtraceOptions::empty();
            };

            (trace_type, ptrace.get_core_state())
        };

        if ptrace_attach_from_state(child_task, ptrace_state).is_err() {
            return PtraceOptions::empty();
        }
        trace_type
    }
}
