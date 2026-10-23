// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use smallvec::SmallVec;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, Ordering};

/// A blocking object that waits for signals on Zircon objects, and that can be interrupted.
///
/// `InterruptibleEvent` and `PortEvent` wake up a thread when another thread notifies or
/// interrupts it. A thread that must also wake up when a Zircon object is signaled, such as a
/// channel it reads requests from, would have to observe the object through a port, or an async
/// executor. Instead, `ObjectWaiter` lets the thread wait for the objects directly with
/// `zx_object_wait_many`: the waiter adds an event of its own to the objects the thread waits for,
/// and `interrupt` signals that event.
///
/// To block using an `ObjectWaiter`, first call `begin_wait`. At this point, the waiter is in the
/// "waiting" state, and future calls to `interrupt` will terminate the wait.
///
/// After `begin_wait` returns, call `wait_many` on the returned guard to block the current thread
/// until one of the following conditions occur:
///
///  1. One of the given objects asserts one of the signals that it is waited for.
///  2. The given deadline expires.
///  3. `interrupt` was called after `begin_wait`.
///
/// It's safe to call `interrupt` at any time. However, only one thread can wait on an
/// `ObjectWaiter` at a time: `begin_wait` cannot be called again until the guard it returns is
/// consumed by `wait_many`, or dropped.
///
/// `ObjectWaiter` uses two-phase waiting so that clients can register the waiter with the code
/// that interrupts them, for example in the `RunState` of a task, and then start blocking. This
/// approach ensures that clients do not miss interruptions that arrive after they register the
/// waiter but before they actually start blocking, while the interruptions of previous waits,
/// which arrive before `begin_wait`, are ignored.
///
/// The event of the waiter is only signaled when a wait is actually interrupted, and only cleared
/// after such a wait, so that a wait that is not interrupted costs no more syscall than
/// `zx_object_wait_many` itself.
///
/// As with `InterruptibleEvent` and `PortEvent`, a wait that remains blocked for
/// `STACK_CLEAN_DELAY` wakes up once to decommit the unused pages of the stack of the thread with
/// `starnix_stack::clean_stack`, and then blocks again until the deadline.
#[derive(Debug)]
pub struct ObjectWaiter {
    /// The event that `interrupt` signals with `INTERRUPTED_SIGNAL`, and that `wait_many` waits for
    /// along with the objects of the caller.
    ///
    /// The signal is only asserted while `state` is `INTERRUPTING` or `INTERRUPTED`, so that an
    /// interruption that arrives before `wait_many` blocks still ends the wait. The end of an
    /// interrupted wait deasserts it.
    event: zx::Event,

    /// The state of the waiter: `IDLE`, `WAITING`, `INTERRUPTING` or `INTERRUPTED`.
    state: AtomicU8,
}

/// No wait is in progress, and the event is not signaled.
///
///  * Transitions to `WAITING` after `begin_wait`.
const IDLE: u8 = 0;

/// A wait is in progress, and was not interrupted.
///
///  * Transitions to `INTERRUPTING` after `interrupt`.
///  * Transitions to `IDLE` when the wait ends.
const WAITING: u8 = 1;

/// `interrupt` is signaling the event.
///
///  * Transitions to `INTERRUPTED` once `interrupt` has signaled the event.
const INTERRUPTING: u8 = 2;

/// The wait was interrupted, and the event is signaled.
///
///  * Transitions to `IDLE` when the wait ends, after the event is cleared.
const INTERRUPTED: u8 = 3;

/// The signal that `interrupt` asserts on the event of the waiter.
const INTERRUPTED_SIGNAL: zx::Signals = zx::Signals::EVENT_SIGNALED;

/// The number of wait items, including the event of the waiter, that `wait_many` keeps on the
/// stack. Waits for more objects allocate.
const WAIT_ITEMS_COMMON_SIZE: usize = 4;

/// A guard object to enforce that clients call `begin_wait` before `wait_many`.
///
/// The wait ends when the guard is consumed by `wait_many`, or dropped.
#[must_use = "call wait_many to block until the objects are signaled or the wait is interrupted"]
#[derive(Debug)]
pub struct ObjectWaitGuard<'a> {
    waiter: &'a Arc<ObjectWaiter>,
}

/// A description of why `wait_many` returned without any of the given objects being signaled.
#[derive(Debug, PartialEq, Eq)]
pub enum ObjectWaitError {
    /// `wait_many` returned because another thread interrupted the wait using `interrupt`.
    Interrupted,

    /// `wait_many` returned because the given deadline expired.
    DeadlineExpired,

    /// `wait_many` returned because `zx_object_wait_many` failed with the given status, for
    /// example because the handle of an object does not have the `ZX_RIGHT_WAIT` right.
    Failed(zx::Status),
}

impl ObjectWaiter {
    pub fn new() -> Arc<Self> {
        Arc::new(Self { event: zx::Event::create(), state: AtomicU8::new(IDLE) })
    }

    /// Called to initiate a wait.
    ///
    /// Calls to `interrupt` after this function returns, and before the wait ends, will cause
    /// `wait_many` to return `ObjectWaitError::Interrupted`. Calls to `interrupt` prior to calling
    /// `begin_wait`, or after the wait ended, are ignored.
    ///
    /// Once called, this function cannot be called again until the returned guard is consumed by
    /// `wait_many`, or dropped. Otherwise, this function will panic.
    pub fn begin_wait<'a>(self: &'a Arc<Self>) -> ObjectWaitGuard<'a> {
        // The event is not signaled in the `IDLE` state, so this needs no syscall. The `Release`
        // pairs with the `Acquire` of `interrupt`, so that an `interrupt` that observes `WAITING`
        // signals the event after the end of the previous wait cleared it.
        self.state
            .compare_exchange(IDLE, WAITING, Ordering::AcqRel, Ordering::Relaxed)
            .expect("Tried to begin waiting on an ObjectWaiter that is already waiting.");
        ObjectWaitGuard { waiter: self }
    }

    /// Interrupts the wait in progress.
    ///
    /// If no wait is in progress, that is, before `begin_wait` or after the wait ended, the
    /// interruption is ignored. Otherwise, `wait_many` returns `ObjectWaitError::Interrupted`,
    /// whether it is already blocked or not. Calling this function repeatedly has no effect.
    pub fn interrupt(&self) {
        // Only signal the event of a wait in progress. An `interrupt` that observes the state of a
        // previous wait, or of no wait, does nothing, so it cannot signal the event of a later
        // wait.
        if self
            .state
            .compare_exchange(WAITING, INTERRUPTING, Ordering::Acquire, Ordering::Relaxed)
            .is_ok()
        {
            // The event is owned by this object, so signaling it cannot fail.
            let _ = self.event.signal(zx::Signals::NONE, INTERRUPTED_SIGNAL);
            // The `Release` pairs with the `Acquire` of `end_wait`, so that the end of the wait
            // clears the event after this signal.
            self.state.store(INTERRUPTED, Ordering::Release);
        }
    }

    /// Ends the current wait, and returns to the `IDLE` state.
    ///
    /// Returns whether the wait was interrupted, that is, whether `interrupt` moved the state out
    /// of `WAITING` before the wait ended.
    fn end_wait(&self) -> bool {
        loop {
            match self.state.compare_exchange(WAITING, IDLE, Ordering::Release, Ordering::Acquire) {
                // The wait was not interrupted, so the event is not signaled.
                Ok(_) => return false,
                // `interrupt` is signaling the event. Wait for it to finish, so that the event can
                // be cleared before the next wait. This only lasts until `interrupt` returns from
                // `zx_object_signal`.
                Err(INTERRUPTING) => std::thread::yield_now(),
                Err(INTERRUPTED) => {
                    // The event is owned by this object, so signaling it cannot fail.
                    let _ = self.event.signal(INTERRUPTED_SIGNAL, zx::Signals::NONE);
                    self.state.store(IDLE, Ordering::Release);
                    return true;
                }
                Err(state) => unreachable!("Unexpected ObjectWaiter state: {state}"),
            }
        }
    }
}

impl<'a> ObjectWaitGuard<'a> {
    /// The waiter associated with this guard.
    pub fn waiter(&self) -> &'a Arc<ObjectWaiter> {
        self.waiter
    }

    /// Blocks the thread until one of `items` is signaled, `deadline` expires, or the wait is
    /// interrupted.
    ///
    /// Each of `items` names an object and the signals to wait for on it, as returned by the
    /// `wait_item` method of the handle of the object. Once at least one of the objects asserts
    /// one of the signals it is waited for, returns the signals pending on each object, in the
    /// order of `items`.
    ///
    /// Returns `ObjectWaitError::Interrupted` if `interrupt` was called after `begin_wait` and
    /// before the wait ended, even if some of the objects are also signaled, so that
    /// interruptions are never missed.
    ///
    /// `items` must contain fewer than `ZX_WAIT_MANY_MAX_ITEMS` items, because the waiter adds its
    /// own event to them.
    pub fn wait_many<const N: usize>(
        self,
        items: [zx::WaitItem<'_>; N],
        deadline: zx::MonotonicInstant,
    ) -> Result<[zx::Signals; N], ObjectWaitError> {
        self.wait_many_with_clean_delay(items, deadline, crate::STACK_CLEAN_DELAY)
    }

    fn wait_many_with_clean_delay<const N: usize>(
        self,
        items: [zx::WaitItem<'_>; N],
        deadline: zx::MonotonicInstant,
        clean_delay: zx::MonotonicDuration,
    ) -> Result<[zx::Signals; N], ObjectWaitError> {
        const { assert!(N < zx::sys::ZX_WAIT_MANY_MAX_ITEMS, "Too many items to wait for") };

        // `zx_object_wait_many` takes a single array, so append the event of the waiter to
        // `items`. Reserve room for all of them first, so that the items are not moved to the heap
        // after being written inline when there are `WAIT_ITEMS_COMMON_SIZE` of them.
        let waiter = self.waiter;
        let mut all_items: SmallVec<[zx::WaitItem<'_>; WAIT_ITEMS_COMMON_SIZE]> =
            SmallVec::with_capacity(N + 1);
        all_items.extend(items);
        all_items.push(waiter.event.wait_item(INTERRUPTED_SIGNAL));

        // Clean the stack once if the wait remains blocked for `clean_delay`.
        let mut clean_deadline = crate::initial_clean_deadline(deadline, clean_delay);
        let result = loop {
            match zx::object_wait_many(&mut all_items, clean_deadline) {
                Err(zx::Status::TIMED_OUT) if clean_deadline < deadline => {
                    starnix_stack::clean_stack();
                    clean_deadline = deadline;
                }
                result => break result,
            }
        };

        // The state of the waiter, rather than the signals that `zx_object_wait_many` returned,
        // tells whether the wait was interrupted: `interrupt` may have moved the state out of
        // `WAITING` before signaling the event, or after the syscall returned.
        if self.end() {
            return Err(ObjectWaitError::Interrupted);
        }
        match result {
            // `Ok(true)` means that a handle was closed during the wait, which cannot happen
            // because `items` borrow their handles.
            Ok(_) => Ok(std::array::from_fn(|i| all_items[i].pending())),
            Err(zx::Status::TIMED_OUT) => Err(ObjectWaitError::DeadlineExpired),
            Err(status) => Err(ObjectWaitError::Failed(status)),
        }
    }

    /// Ends the wait instead of `drop`, and returns whether the wait was interrupted.
    fn end(self) -> bool {
        let waiter = self.waiter;
        std::mem::forget(self);
        waiter.end_wait()
    }
}

impl Drop for ObjectWaitGuard<'_> {
    fn drop(&mut self) {
        self.waiter.end_wait();
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::AtomicBool;

    fn signal(event: &zx::Event) {
        event.signal(zx::Signals::NONE, zx::Signals::USER_0).expect("failed to signal event");
    }

    /// Waits with `guard` until `event` asserts `USER_0`, `deadline` expires, or the wait is
    /// interrupted.
    fn wait_for(
        guard: ObjectWaitGuard<'_>,
        event: &zx::Event,
        deadline: zx::MonotonicInstant,
    ) -> Result<[zx::Signals; 1], ObjectWaitError> {
        guard.wait_many([event.wait_item(zx::Signals::USER_0)], deadline)
    }

    /// Runs `f` on a new thread once the calling thread is blocked in `zx_object_wait_many`.
    fn spawn_when_blocked(f: impl FnOnce() + Send + 'static) -> std::thread::JoinHandle<()> {
        let waiting_thread = fuchsia_runtime::with_thread_self(|thread| {
            thread.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("duplicate_handle")
        });
        std::thread::spawn(move || {
            while waiting_thread.info().expect("info").state
                != zx::ThreadState::Blocked(zx::ThreadBlockType::WaitMany)
            {
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
            f();
        })
    }

    /// Returns whether the event of `waiter` is signaled.
    fn is_event_signaled(waiter: &ObjectWaiter) -> bool {
        waiter.event.wait_one(INTERRUPTED_SIGNAL, zx::MonotonicInstant::INFINITE_PAST).is_ok()
    }

    #[test]
    fn test_wait_block_and_signal() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        let other_event = event.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let thread = spawn_when_blocked(move || signal(&other_event));

        let result = wait_for(waiter.begin_wait(), &event, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Ok([zx::Signals::USER_0]));
        thread.join().expect("failed to join thread");
    }

    #[test]
    fn test_wait_block_and_interrupt() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        let other_waiter = Arc::clone(&waiter);
        let thread = spawn_when_blocked(move || other_waiter.interrupt());

        let result = wait_for(waiter.begin_wait(), &event, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(ObjectWaitError::Interrupted));
        thread.join().expect("failed to join thread");
        // The end of the interrupted wait cleared the event.
        assert!(!is_event_signaled(&waiter));
    }

    #[test]
    fn test_wait_block_and_timeout() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        let deadline = zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(20));

        let result = wait_for(waiter.begin_wait(), &event, deadline);
        assert_eq!(result, Err(ObjectWaitError::DeadlineExpired));
        assert!(zx::MonotonicInstant::get() >= deadline);
    }

    #[test]
    fn test_poll_with_past_deadline() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        let deadline = zx::MonotonicInstant::INFINITE_PAST;

        let result = wait_for(waiter.begin_wait(), &event, deadline);
        assert_eq!(result, Err(ObjectWaitError::DeadlineExpired));
        signal(&event);
        assert_eq!(wait_for(waiter.begin_wait(), &event, deadline), Ok([zx::Signals::USER_0]));
    }

    #[test]
    fn test_wait_returns_pending_signals_in_order() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        let (channel, peer) = zx::Channel::create();
        std::mem::drop(peer);

        let result = waiter.begin_wait().wait_many(
            [
                event.wait_item(zx::Signals::USER_0),
                channel.wait_item(zx::Signals::CHANNEL_PEER_CLOSED),
            ],
            zx::MonotonicInstant::INFINITE,
        );
        let [event_signals, channel_signals] = result.expect("failed to wait");
        assert_eq!(event_signals, zx::Signals::NONE);
        assert!(channel_signals.contains(zx::Signals::CHANNEL_PEER_CLOSED));
    }

    #[test]
    fn test_wait_without_items() {
        let waiter = ObjectWaiter::new();

        let result = waiter.begin_wait().wait_many([], zx::MonotonicInstant::INFINITE_PAST);
        assert_eq!(result, Err(ObjectWaitError::DeadlineExpired));
        let guard = waiter.begin_wait();
        waiter.interrupt();
        let result = guard.wait_many([], zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(ObjectWaitError::Interrupted));
    }

    #[test]
    fn test_wait_fails_without_wait_right() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create().duplicate_handle(zx::Rights::TRANSFER).unwrap();

        let result = wait_for(waiter.begin_wait(), &event, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(ObjectWaitError::Failed(zx::Status::ACCESS_DENIED)));
    }

    #[test]
    fn test_interrupt_outside_wait_is_ignored() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();

        // Neither an interruption before the first wait, nor one after a wait ended, signals the
        // event or interrupts the next wait.
        for _ in 0..2 {
            waiter.interrupt();
            assert!(!is_event_signaled(&waiter));
            let result = wait_for(waiter.begin_wait(), &event, zx::MonotonicInstant::INFINITE_PAST);
            assert_eq!(result, Err(ObjectWaitError::DeadlineExpired));
        }
    }

    #[test]
    fn test_interrupt_before_block_ends_wait() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();

        // The interruption arrives after `begin_wait`, but before the thread blocks.
        let guard = waiter.begin_wait();
        waiter.interrupt();
        let result = wait_for(guard, &event, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(ObjectWaitError::Interrupted));

        // It takes precedence over the signals of the objects.
        signal(&event);
        let guard = waiter.begin_wait();
        waiter.interrupt();
        let result = wait_for(guard, &event, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(ObjectWaitError::Interrupted));

        // The next wait ignores the previous interruptions.
        let result = wait_for(waiter.begin_wait(), &event, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Ok([zx::Signals::USER_0]));
    }

    #[test]
    fn test_interrupt_signals_event_once() {
        let waiter = ObjectWaiter::new();

        let guard = waiter.begin_wait();
        assert!(!is_event_signaled(&waiter));
        waiter.interrupt();
        assert!(is_event_signaled(&waiter));
        // Repeated interruptions keep the wait interrupted.
        waiter.interrupt();
        assert!(is_event_signaled(&waiter));

        // Dropping the guard of the interrupted wait ends the wait, and clears the event.
        std::mem::drop(guard);
        assert!(!is_event_signaled(&waiter));
        std::mem::drop(waiter.begin_wait());
    }

    #[test]
    fn test_interrupt_in_progress_ends_wait() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        signal(&event);

        // Start an interruption that has not signaled the event yet, as if `interrupt` were
        // preempted right after leaving `WAITING`, so that `zx_object_wait_many` returns because
        // `event` is signaled, without observing the interruption.
        let guard = waiter.begin_wait();
        waiter.state.store(INTERRUPTING, Ordering::Relaxed);
        let other_waiter = Arc::clone(&waiter);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(10));
            // Finish the interruption, as `interrupt` does.
            other_waiter.event.signal(zx::Signals::NONE, INTERRUPTED_SIGNAL).expect("signal");
            other_waiter.state.store(INTERRUPTED, Ordering::Release);
        });

        // The wait reports the interruption anyway, and its end clears the event.
        let result = wait_for(guard, &event, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(ObjectWaitError::Interrupted));
        thread.join().expect("failed to join thread");
        assert!(!is_event_signaled(&waiter));
    }

    #[test]
    fn test_interrupt_races_with_end_of_wait() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        signal(&event);
        let done = Arc::new(AtomicBool::new(false));
        let interrupter = std::thread::spawn({
            let (waiter, done) = (Arc::clone(&waiter), Arc::clone(&done));
            move || {
                while !done.load(Ordering::Relaxed) {
                    waiter.interrupt();
                }
            }
        });

        // Each wait either returns because `event` is signaled, or is interrupted, and no
        // interruption outlives the wait that it interrupted.
        for _ in 0..1000 {
            let result = wait_for(waiter.begin_wait(), &event, zx::MonotonicInstant::INFINITE);
            assert!(
                result == Ok([zx::Signals::USER_0]) || result == Err(ObjectWaitError::Interrupted),
                "Unexpected result: {result:?}"
            );
            assert!(!is_event_signaled(&waiter));
        }
        done.store(true, Ordering::Relaxed);
        interrupter.join().expect("failed to join thread");
    }

    #[test]
    #[should_panic(expected = "Tried to begin waiting on an ObjectWaiter that is already waiting.")]
    fn test_concurrent_begin_wait_panics() {
        let waiter = ObjectWaiter::new();
        let _guard = waiter.begin_wait();
        let _ = waiter.begin_wait();
    }

    #[test]
    fn test_wait_clean_stack() {
        let waiter = ObjectWaiter::new();
        let event = zx::Event::create();
        let wait = |guard: ObjectWaitGuard<'_>, deadline| {
            guard.wait_many_with_clean_delay(
                [event.wait_item(zx::Signals::USER_0)],
                deadline,
                zx::MonotonicDuration::from_millis(10),
            )
        };

        // After cleaning the stack, the wait blocks again until the deadline expires...
        let deadline = zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(30));
        assert_eq!(wait(waiter.begin_wait(), deadline), Err(ObjectWaitError::DeadlineExpired));
        assert!(zx::MonotonicInstant::get() >= deadline);

        // ...the wait is interrupted...
        let guard = waiter.begin_wait();
        let other_waiter = Arc::clone(&waiter);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            other_waiter.interrupt();
        });
        let result = wait(guard, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(ObjectWaitError::Interrupted));
        thread.join().expect("failed to join thread");

        // ...or an object is signaled.
        let other_event = event.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            signal(&other_event);
        });
        let result = wait(waiter.begin_wait(), zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Ok([zx::Signals::USER_0]));
        thread.join().expect("failed to join thread");
    }
}
