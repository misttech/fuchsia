// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::{TaskStateCode, Waiter, WaiterRef};
use starnix_sync::InterruptibleEvent;
use std::sync::Arc;

/// Whether, and how, this task is blocked. This enum can be extended with new
/// variants to optimize different kinds of waiting.
#[derive(Debug, Clone)]
pub enum RunState {
    /// This task is not blocked.
    ///
    /// The task might be running in userspace or kernel.
    Running,

    /// This thread is blocked in a `Waiter`.
    Waiter(WaiterRef),

    /// This thread is blocked in an `InterruptibleEvent`.
    Event(Arc<InterruptibleEvent>),

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
    pub fn wake(&self) {
        match self {
            RunState::Running => (),
            RunState::Waiter(waiter) => waiter.interrupt(),
            RunState::Event(event) => event.interrupt(),
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

impl PartialEq<RunState> for RunState {
    fn eq(&self, other: &RunState) -> bool {
        match (self, other) {
            (RunState::Running, RunState::Running) => true,
            (RunState::Waiter(lhs), RunState::Waiter(rhs)) => lhs == rhs,
            (RunState::Event(lhs), RunState::Event(rhs)) => Arc::ptr_eq(lhs, rhs),
            (RunState::Frozen(lhs), RunState::Frozen(rhs)) => lhs == rhs,
            (RunState::TracingStopped, RunState::TracingStopped) => true,
            _ => false,
        }
    }
}
