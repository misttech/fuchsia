// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The state of a thread has two axes. One is its arc from birth to death, expressed via its
//! `Lifecycle` value. The other is its current exception handler state, expressed via its
//! `Exception` value.
//!
//! 1. Lifecycle: Initial -> Initialized -> Running <-> Suspended -> Dying -> Dead.
//! 2. Exception: Idle -> Unprocessed -> TryNext / Resume -> Idle.
//!
//! This module exists to encapsulate all legal state transitions, so generally other assertions
//! about the state of a thread prior to transitioning are not necessary.
//!
//! Only Running, Suspended, or Dying threads may participate in exception handling.

/// The lifecycle state of a thread.
///
/// The only legal transition that isn't from top-to-bottom occurs when a thread is resumed
/// after being suspended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Lifecycle {
    /// The ThreadDispatcher has been allocated, but not yet associated to a Thread or an aspace.
    Initial = 0,

    /// The ThreadDispatcher is now associated to its underlying Thread and the containing
    /// process's address space, and is waiting to be run.
    Initialized = 1,

    /// The thread is running.
    Running = 2,

    /// The thread is currently suspended.
    ///
    /// Note that suspension is orthogonal to being "in an exception". A thread may be both
    /// suspended and in an exception, and the thread does not "resume" execution until it is
    /// resumed from both the suspension and the exception.
    Suspended = 3,

    /// The thread is going to die. It may still be interacting with exception handling state.
    Dying = 4,

    /// The thread is being dissociated from all of its state, and no more interaction with
    /// userspace (including exception handlers) is possible.
    Dead = 5,
}

/// The exception handler state of a thread.
///
/// Idle threads become Unprocessed. Unprocessed threads are told to either Resume or TryNext in
/// a loop until they are killed (no more exception handlers) or resumed, in which case they
/// become Idle again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Exception {
    /// There's no pending exception.
    Idle = 0,

    /// The thread is waiting for the pending exception to be processed.
    Unprocessed = 1,

    /// The exception has been processed, and the next exception handler should be queried.
    TryNext = 2,

    /// The exception has been processed, and the thread should resume.
    Resume = 3,
}

/// Internal representation of the combined lifecycle and exception state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum Value {
    InitialIdle,

    InitializedIdle,

    RunningIdle,
    RunningUnprocessed,
    RunningTryNext,
    RunningResume,

    SuspendedIdle,
    SuspendedUnprocessed,
    SuspendedTryNext,
    SuspendedResume,

    DyingIdle,
    DyingUnprocessed,
    DyingTryNext,
    DyingResume,

    DeadIdle,
}

/// Encapsulates legal thread state transitions along the Lifecycle and Exception axes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ThreadState {
    value: Value,
}

impl Default for ThreadState {
    fn default() -> Self {
        Self::new()
    }
}

impl ThreadState {
    /// Creates a new `ThreadState` in the `Initial` lifecycle and `Idle` exception state.
    pub const fn new() -> Self {
        Self { value: Value::InitialIdle }
    }

    /// Returns the current lifecycle state.
    pub fn lifecycle(&self) -> Lifecycle {
        match self.value {
            Value::InitialIdle => Lifecycle::Initial,
            Value::InitializedIdle => Lifecycle::Initialized,
            Value::RunningIdle
            | Value::RunningUnprocessed
            | Value::RunningTryNext
            | Value::RunningResume => Lifecycle::Running,
            Value::SuspendedIdle
            | Value::SuspendedUnprocessed
            | Value::SuspendedTryNext
            | Value::SuspendedResume => Lifecycle::Suspended,
            Value::DyingIdle
            | Value::DyingUnprocessed
            | Value::DyingTryNext
            | Value::DyingResume => Lifecycle::Dying,
            Value::DeadIdle => Lifecycle::Dead,
        }
    }

    /// Returns the current exception state.
    ///
    /// Only Running, Suspended, and Dying threads have meaningful exception state.
    pub fn exception(&self) -> Exception {
        match self.value {
            Value::RunningIdle | Value::SuspendedIdle | Value::DyingIdle => Exception::Idle,
            Value::RunningUnprocessed | Value::SuspendedUnprocessed | Value::DyingUnprocessed => {
                Exception::Unprocessed
            }
            Value::RunningTryNext | Value::SuspendedTryNext | Value::DyingTryNext => {
                Exception::TryNext
            }
            Value::RunningResume | Value::SuspendedResume | Value::DyingResume => Exception::Resume,
            Value::InitializedIdle | Value::DeadIdle => {
                // Someone could have, for example, requested zx_info_thread_t.
                Exception::Idle
            }
            Value::InitialIdle => {
                panic!("invalid state: a thread in Initial has no meaningful exception state")
            }
        }
    }

    /// Transitions the lifecycle axis.
    ///
    /// Illegal transitions `debug_assert!` and leave the state unchanged.
    pub fn set_lifecycle(&mut self, lifecycle: Lifecycle) {
        match lifecycle {
            Lifecycle::Initial => {
                debug_assert!(false);
            }
            Lifecycle::Initialized => match self.value {
                Value::InitialIdle => self.value = Value::InitializedIdle,
                _ => debug_assert!(false),
            },
            Lifecycle::Running => match self.value {
                Value::InitializedIdle | Value::SuspendedIdle => self.value = Value::RunningIdle,
                Value::SuspendedUnprocessed => self.value = Value::RunningUnprocessed,
                Value::SuspendedTryNext => self.value = Value::RunningTryNext,
                Value::SuspendedResume => self.value = Value::RunningResume,
                _ => debug_assert!(false),
            },
            Lifecycle::Suspended => match self.value {
                Value::RunningIdle => self.value = Value::SuspendedIdle,
                Value::RunningUnprocessed => self.value = Value::SuspendedUnprocessed,
                Value::RunningTryNext => self.value = Value::SuspendedTryNext,
                Value::RunningResume => self.value = Value::SuspendedResume,
                _ => debug_assert!(false),
            },
            Lifecycle::Dying => match self.value {
                Value::RunningIdle | Value::SuspendedIdle | Value::DyingIdle => {
                    self.value = Value::DyingIdle
                }
                Value::RunningUnprocessed
                | Value::SuspendedUnprocessed
                | Value::DyingUnprocessed => self.value = Value::DyingUnprocessed,
                Value::RunningTryNext | Value::SuspendedTryNext | Value::DyingTryNext => {
                    self.value = Value::DyingTryNext
                }
                Value::RunningResume | Value::SuspendedResume | Value::DyingResume => {
                    self.value = Value::DyingResume
                }
                _ => debug_assert!(false),
            },
            Lifecycle::Dead => match self.value {
                Value::DyingIdle
                | Value::DyingUnprocessed
                | Value::DyingTryNext
                | Value::DyingResume => self.value = Value::DeadIdle,
                _ => debug_assert!(false),
            },
        }
    }

    /// Transitions the exception axis.
    ///
    /// Illegal transitions `debug_assert!` and leave the state unchanged.
    pub fn set_exception(&mut self, exception: Exception) {
        match exception {
            Exception::Idle => match self.value {
                Value::RunningUnprocessed | Value::RunningTryNext | Value::RunningResume => {
                    self.value = Value::RunningIdle
                }
                Value::SuspendedUnprocessed | Value::SuspendedTryNext | Value::SuspendedResume => {
                    self.value = Value::SuspendedIdle
                }
                Value::DyingUnprocessed | Value::DyingTryNext | Value::DyingResume => {
                    self.value = Value::DyingIdle
                }
                _ => debug_assert!(false),
            },
            Exception::Unprocessed => match self.value {
                Value::RunningIdle => self.value = Value::RunningUnprocessed,
                Value::SuspendedIdle => self.value = Value::SuspendedUnprocessed,
                Value::DyingIdle => self.value = Value::DyingUnprocessed,
                _ => debug_assert!(false),
            },
            Exception::TryNext => match self.value {
                Value::RunningUnprocessed => self.value = Value::RunningTryNext,
                Value::SuspendedUnprocessed => self.value = Value::SuspendedTryNext,
                Value::DyingUnprocessed => self.value = Value::DyingTryNext,
                _ => debug_assert!(false),
            },
            Exception::Resume => match self.value {
                Value::RunningUnprocessed => self.value = Value::RunningResume,
                Value::SuspendedUnprocessed => self.value = Value::SuspendedResume,
                Value::DyingUnprocessed => self.value = Value::DyingResume,
                _ => debug_assert!(false),
            },
        }
    }
}

/// Converts a lifecycle enum value to its human-readable string representation.
pub fn lifecycle_to_string(lifecycle: Lifecycle) -> &'static str {
    match lifecycle {
        Lifecycle::Initial => "initial",
        Lifecycle::Initialized => "initialized",
        Lifecycle::Running => "running",
        Lifecycle::Suspended => "suspended",
        Lifecycle::Dying => "dying",
        Lifecycle::Dead => "dead",
    }
}

#[cfg(ktest)]
#[unittest::suite(name = "thread_state_rust")]
/// Unit tests for ThreadState.
mod tests {
    use super::{Exception, Lifecycle, ThreadState, lifecycle_to_string};
    use unittest::{expect_eq, expect_true};

    /// Tests initial thread state.
    #[test]
    fn test_initial_state() {
        let state = ThreadState::new();
        expect_eq!(state.lifecycle(), Lifecycle::Initial);
        // A thread in Initial has no meaningful exception state and `exception()` asserts, so it
        // is deliberately not queried here.
    }

    /// Tests thread lifecycle state transitions.
    #[test]
    fn test_lifecycle_transitions() {
        let mut state = ThreadState::new();
        state.set_lifecycle(Lifecycle::Initialized);
        expect_eq!(state.lifecycle(), Lifecycle::Initialized);

        state.set_lifecycle(Lifecycle::Running);
        expect_eq!(state.lifecycle(), Lifecycle::Running);

        state.set_lifecycle(Lifecycle::Suspended);
        expect_eq!(state.lifecycle(), Lifecycle::Suspended);

        state.set_lifecycle(Lifecycle::Running);
        expect_eq!(state.lifecycle(), Lifecycle::Running);

        state.set_lifecycle(Lifecycle::Dying);
        expect_eq!(state.lifecycle(), Lifecycle::Dying);

        state.set_lifecycle(Lifecycle::Dead);
        expect_eq!(state.lifecycle(), Lifecycle::Dead);
    }

    /// Tests that Dying accepts itself as a source, which is what makes a second kill() legal.
    #[test]
    fn test_dying_is_idempotent() {
        let mut state = ThreadState::new();
        state.set_lifecycle(Lifecycle::Initialized);
        state.set_lifecycle(Lifecycle::Running);
        state.set_lifecycle(Lifecycle::Dying);
        expect_eq!(state.lifecycle(), Lifecycle::Dying);

        state.set_lifecycle(Lifecycle::Dying);
        expect_eq!(state.lifecycle(), Lifecycle::Dying);

        // The exception axis is carried through the repeated transition unchanged.
        state.set_exception(Exception::Unprocessed);
        state.set_lifecycle(Lifecycle::Dying);
        expect_eq!(state.lifecycle(), Lifecycle::Dying);
        expect_eq!(state.exception(), Exception::Unprocessed);
    }

    /// Tests thread exception state transitions.
    #[test]
    fn test_exception_transitions() {
        let mut state = ThreadState::new();
        state.set_lifecycle(Lifecycle::Initialized);
        state.set_lifecycle(Lifecycle::Running);

        // Running: Idle -> Unprocessed -> TryNext -> Idle
        state.set_exception(Exception::Unprocessed);
        expect_eq!(state.exception(), Exception::Unprocessed);

        state.set_exception(Exception::TryNext);
        expect_eq!(state.exception(), Exception::TryNext);

        state.set_exception(Exception::Idle);
        expect_eq!(state.exception(), Exception::Idle);

        // Transition with suspension. The exception axis is preserved across it.
        state.set_exception(Exception::Unprocessed);
        state.set_lifecycle(Lifecycle::Suspended);
        expect_eq!(state.lifecycle(), Lifecycle::Suspended);
        expect_eq!(state.exception(), Exception::Unprocessed);

        state.set_exception(Exception::Resume);
        expect_eq!(state.exception(), Exception::Resume);

        state.set_exception(Exception::Idle);
        expect_eq!(state.exception(), Exception::Idle);
    }

    /// Tests the exception state of threads that do not participate in exception handling.
    #[test]
    fn test_exception_of_non_participating_states() {
        // Initialized and Dead are reachable by, for example, a request for `zx_info_thread_t`,
        // even though a thread in either state never participates in exception handling.
        let mut state = ThreadState::new();
        state.set_lifecycle(Lifecycle::Initialized);
        expect_eq!(state.exception(), Exception::Idle);

        state.set_lifecycle(Lifecycle::Running);
        state.set_lifecycle(Lifecycle::Dying);
        state.set_lifecycle(Lifecycle::Dead);
        expect_eq!(state.exception(), Exception::Idle);
    }

    /// Tests converting thread lifecycle to string representation.
    #[test]
    fn test_lifecycle_to_string() {
        expect_true!(lifecycle_to_string(Lifecycle::Initial) == "initial");
        expect_true!(lifecycle_to_string(Lifecycle::Initialized) == "initialized");
        expect_true!(lifecycle_to_string(Lifecycle::Running) == "running");
        expect_true!(lifecycle_to_string(Lifecycle::Suspended) == "suspended");
        expect_true!(lifecycle_to_string(Lifecycle::Dying) == "dying");
        expect_true!(lifecycle_to_string(Lifecycle::Dead) == "dead");
    }
}
