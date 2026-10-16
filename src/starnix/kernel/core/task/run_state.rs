// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{TaskStateCode, Waiter, WaiterRef};
use starnix_sync::InterruptibleEvent;
use std::ptr::NonNull;

/// A non-owning reference to an [`InterruptibleEvent`] registered in [`RunState::Event`].
///
/// # Safety Invariant
/// An `EventRef` is only stored in `TaskMutableState::run_state` by
/// `CurrentTask::run_in_state_internal` for the lexical duration of the blocking callback (while
/// the `InterruptibleEvent` is borrowed), and is cleared back to `RunState::Running` under
/// `Task::write()` before `run_in_state_internal` returns. `RunState::wake` is only called under
/// `Task::read()`, so the pointee is guaranteed to remain valid for every `wake` call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EventRef(NonNull<InterruptibleEvent>);

// SAFETY: `EventRef` only provides shared (`&InterruptibleEvent`) access to the pointee, and
// `InterruptibleEvent` is `Sync`.
unsafe impl Send for EventRef {}
// SAFETY: `EventRef` only provides shared (`&InterruptibleEvent`) access to the pointee, and
// `InterruptibleEvent` is `Sync`.
unsafe impl Sync for EventRef {}

impl EventRef {
    /// Creates an [`EventRef`] pointing to `event`.
    ///
    /// # Safety
    /// The caller must ensure that `event` remains valid for as long as the resulting
    /// `RunState::Event` is installed in `TaskMutableState::run_state` and across all concurrent
    /// `RunState::wake` calls (by clearing `run_state` back to `RunState::Running` under
    /// `Task::write()` before `event`'s borrow ends).
    pub(super) unsafe fn new(event: &InterruptibleEvent) -> Self {
        Self(NonNull::from(event))
    }

    unsafe fn interrupt(&self) {
        // SAFETY: By the `EventRef` invariant and `RunState::wake` safety requirement, `self.0`
        // points to a live `InterruptibleEvent`.
        unsafe { self.0.as_ref().interrupt() };
    }
}

/// Whether, and how, this task is blocked. This enum can be extended with new
/// variants to optimize different kinds of waiting.
#[derive(Debug, Clone, PartialEq)]
pub enum RunState {
    /// This task is not blocked.
    ///
    /// The task might be running in userspace or kernel.
    Running,

    /// This thread is blocked in a `Waiter`.
    Waiter(WaiterRef),

    /// This thread is blocked in an `InterruptibleEvent`.
    Event(EventRef),

    /// This thread is frozen by a `Waiter`.
    ///
    /// When waiting on the `Waiter`, it should have a loop to prevent any signals except
    /// notification.
    Frozen(Waiter),

    /// This thread is stopped by a tracer (or stopped by job control).
    ///
    /// It is not interruptible except by the tracer or SIGCONT, and can be aborted by SIGKILL.
    TracingStopped,
}

impl Default for RunState {
    fn default() -> Self {
        RunState::Running
    }
}

impl RunState {
    /// Whether this task is blocked.
    ///
    /// If the task is blocked, you can break the task out of the wait using the `wake` function.
    pub fn is_blocked(&self) -> bool {
        match self {
            RunState::Running => false,
            RunState::Waiter(waiter) => waiter.is_valid(),
            RunState::Event(_) | RunState::Frozen(_) | RunState::TracingStopped => true,
        }
    }

    /// Whether this task ignores signals other than SIGKILL while blocked in this state.
    pub fn ignores_signals(&self) -> bool {
        match self {
            RunState::Frozen(_) | RunState::TracingStopped => true,
            RunState::Running | RunState::Waiter(_) | RunState::Event(_) => false,
        }
    }

    /// Unblock the task by interrupting whatever wait the task is blocked upon.
    ///
    /// # Safety
    /// If `self` is `RunState::Event`, the caller must guarantee that the target task has not yet
    /// cleared its `run_state` in `CurrentTask::run_in_state` (for example, by holding the task's
    /// state lock while reading `run_state` and calling `wake`).
    pub(super) unsafe fn wake(&self) {
        match self {
            RunState::Running => (),
            RunState::Waiter(waiter) => waiter.interrupt(),
            // SAFETY: Guaranteed by the caller.
            RunState::Event(event) => unsafe { event.interrupt() },
            // When frozen or stopped, the task is immune to any interrupts.
            RunState::Frozen(_) | RunState::TracingStopped => (),
        }
    }

    /// The TaskStateCode corresponding to the RunState.
    pub fn state_code(&self) -> TaskStateCode {
        match self {
            RunState::Running => TaskStateCode::Running,
            RunState::Waiter(waiter) => {
                if waiter.is_valid() {
                    TaskStateCode::Sleeping
                } else {
                    TaskStateCode::Running
                }
            }
            RunState::Event(_) | RunState::Frozen(_) => TaskStateCode::Sleeping,
            // TODO(https://fxbug.dev/566829060): Job-control stops currently report
            // `TaskStateCode::TracingStop` ('t'). Distinguish them to report
            // `TaskStateCode::Stopped` ('T').
            RunState::TracingStopped => TaskStateCode::TracingStop,
        }
    }
}
