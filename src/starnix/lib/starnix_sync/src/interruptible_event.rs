// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{InterruptibleEventRequeueOwnerLock, LockDepMutex};
use std::sync::Arc;
use std::sync::atomic::Ordering;

/// A blocking object that can either be notified normally or interrupted.
///
/// To block using an `InterruptibleEvent`, first call [`Self::begin_wait`]. At this point, the
/// event is in the "waiting" state, and future calls to [`Self::notify`] or [`Self::interrupt`]
/// will terminate the wait.
///
/// After `begin_wait` returns, call [`EventWaitGuard::block_until`] on the returned guard to block
/// the current thread until one of the following conditions occurs:
///
///  1. The given deadline expires.
///  2. At least one of the `notify` or `interrupt` functions was called after `begin_wait`.
///
/// Alternatively, dropping the [`EventWaitGuard`] without calling `block_until` cancels the wait
/// and resets the event back to the ready state.
///
/// It's safe to call `notify` or `interrupt` at any time. However, only one wait cycle can be
/// active at a time: `begin_wait` cannot be called again until the [`EventWaitGuard`] it returns
/// is consumed by `block_until` or dropped. When an `InterruptibleEvent` is reused across multiple
/// wait cycles, `notify`, `interrupt`, and [`Self::assign_new_owner`] are not bound to a specific
/// cycle; see [`Self::begin_wait`] and [`Self::assign_new_owner`] for cross-cycle concurrency
/// considerations.
///
/// `InterruptibleEvent` uses two-phase waiting so that clients can register for notification,
/// perform some related work, and then start blocking. This approach ensures that clients do not
/// miss notifications that arrive after they perform the related work but before they actually
/// start blocking.
///
/// # Priority Inheritance and Dynamic Futex Owner Assignment
///
/// `InterruptibleEvent` supports Zircon Priority Inheritance (PI). If the target owner thread is
/// known at wait time, it can be passed directly to `block_until` / `zx_futex_wait`.
///
/// When the owner thread is not known when waiting begins (for example, when a transaction is
/// queued to a process-wide worker pool), `assign_new_owner` can be called by the worker thread
/// that later dequeues the work item. `assign_new_owner` uses `zx_futex_requeue` to atomically
/// transfer waiting threads to a secondary `requeue_target` futex while designating the worker
/// thread as the futex PI owner.
///
/// TODO(https://fxbug.dev/542307988): The long-term solution is a dedicated Zircon syscall (such as
/// `zx_futex_assign_owner`) to assign or update the PI owner of an existing futex without
/// requiring a secondary requeue target futex.
#[derive(Debug)]
pub struct InterruptibleEvent {
    futex: zx::Futex,
    requeue_target: zx::Futex,
    /// Retains the PI owner assigned via `assign_new_owner` for the duration of the wait cycle.
    ///
    /// Keeping an `Arc<zx::Thread>` reference here (rather than only passing the handle once to
    /// `zx_futex_requeue`) is required for three reasons:
    ///  1. When `block_until` wakes at the intermediate `clean_deadline` to call `clean_stack()`,
    ///     Zircon's `FutexWait` (`ResetOwnerIfNoWaiters`) clears `requeue_target`'s kernel owner
    ///     before returning `ZX_ERR_TIMED_OUT`; re-waiting on `requeue_target` must pass the owner
    ///     handle again to preserve PI for the rest of the wait.
    ///  2. If `assign_new_owner` is called after `begin_wait` but before the waiting thread enters
    ///     `zx_futex_wait` in the kernel (or if a spurious wakeup occurs), `zx_futex_requeue` moves
    ///     0 kernel waiters, and a subsequent `requeue_target.wait(WAITING, None, ...)` would
    ///     overwrite `requeue_target`'s kernel owner with `None`.
    ///  3. Holding an `Arc<zx::Thread>` guarantees the underlying `zx_handle_t` stays open until
    ///     `reset()`, avoiding `zx_handle_duplicate` syscalls and `ZX_POL_BAD_HANDLE` policy
    ///     exceptions if the worker thread exits during the wait.
    requeue_owner: LockDepMutex<Option<Arc<zx::Thread>>, InterruptibleEventRequeueOwnerLock>,
}

/// The initial state when no wait cycle is active.
///
///  * Transitions to `WAITING` after `begin_wait`.
const READY: i32 = 0;

/// The event is waiting for a notification or an interruption.
///
///  * Transitions to `NOTIFIED` after `notify`.
///  * Transitions to `INTERRUPTED` after `interrupt`.
///  * Transitions to `REQUEUED` after `assign_new_owner` (on the primary `futex`).
///  * Transitions to `READY` when `EventWaitGuard` is dropped (either at the end of `block_until`
///    or if dropped early).
const WAITING: i32 = 1;

/// The event has been notified and will wake up.
///
///  * Transitions to `READY` when `EventWaitGuard` is dropped.
const NOTIFIED: i32 = 2;

/// The event has been interrupted and will wake up.
///
///  * Transitions to `READY` when `EventWaitGuard` is dropped.
const INTERRUPTED: i32 = 3;

/// The event has been requeued to a secondary futex target with a new PI owner.
///
///  * Transitions to `READY` when `EventWaitGuard` is dropped.
const REQUEUED: i32 = 4;

/// A guard object to enforce that clients call `begin_wait` before `block_until`.
///
/// Dropping the guard (whether at the end of `block_until` or on an early abort before blocking)
/// resets the underlying [`InterruptibleEvent`] back to the `READY` state so it can be reused.
#[must_use = "call block_until to advance the event state machine"]
#[derive(Debug)]
pub struct EventWaitGuard<'a> {
    event: &'a InterruptibleEvent,
}

impl Drop for EventWaitGuard<'_> {
    fn drop(&mut self) {
        self.event.reset();
    }
}

impl<'a> EventWaitGuard<'a> {
    /// The underlying event associated with this guard.
    pub fn event(&self) -> &'a InterruptibleEvent {
        self.event
    }

    /// Returns the owner of the underlying futex, if any.
    pub fn get_owner(&self) -> Option<zx::Koid> {
        self.event.get_owner()
    }

    /// Block the thread until either `deadline` expires, the event is notified, or the event is
    /// interrupted.
    pub fn block_until(
        self,
        new_owner: Option<&zx::Thread>,
        deadline: zx::MonotonicInstant,
    ) -> Result<(), WakeReason> {
        self.event.block_until_with_clean_delay(new_owner, deadline, crate::STACK_CLEAN_DELAY)
    }

    #[cfg(test)]
    fn block_until_with_clean_delay(
        self,
        new_owner: Option<&zx::Thread>,
        deadline: zx::MonotonicInstant,
        clean_delay: zx::MonotonicDuration,
    ) -> Result<(), WakeReason> {
        self.event.block_until_with_clean_delay(new_owner, deadline, clean_delay)
    }
}

/// A description of why a `block_until` returned without the event being notified.
#[derive(Debug, PartialEq, Eq)]
pub enum WakeReason {
    /// `block_until` returned because another thread interrupted the wait using `interrupt`.
    Interrupted,

    /// `block_until` returned because the given deadline expired.
    DeadlineExpired,
}

impl Default for InterruptibleEvent {
    fn default() -> Self {
        InterruptibleEvent {
            futex: zx::Futex::new(READY),
            requeue_target: zx::Futex::new(READY),
            requeue_owner: LockDepMutex::new(None),
        }
    }
}

struct LoadedState<'a> {
    futex: &'a zx::Futex,
    state: i32,
    is_requeued: bool,
}

impl InterruptibleEvent {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Loads the active futex (`self.futex`, or `self.requeue_target` once `assign_new_owner` has
    /// transitioned `self.futex` to `REQUEUED`) together with its current state.
    fn load_state(&self) -> LoadedState<'_> {
        let state = self.futex.load(Ordering::Acquire);
        if state == REQUEUED {
            LoadedState {
                futex: &self.requeue_target,
                state: self.requeue_target.load(Ordering::Acquire),
                is_requeued: true,
            }
        } else {
            LoadedState { futex: &self.futex, state, is_requeued: false }
        }
    }

    /// Returns the owner of the active underlying futex, if any.
    pub fn get_owner(&self) -> Option<zx::Koid> {
        self.load_state().futex.get_owner()
    }

    /// Called to initiate a wait cycle.
    ///
    /// Calls to [`Self::notify`] or [`Self::interrupt`] after this function returns, and before the
    /// returned [`EventWaitGuard`] is dropped, will cause the event to wake up. Calls that complete
    /// while the event is idle (before `begin_wait` or after the previous `EventWaitGuard` has been
    /// dropped) are ignored.
    ///
    /// When reusing an `InterruptibleEvent` across multiple wait cycles, `notify`, `interrupt`, and
    /// [`Self::assign_new_owner`] are not tied to a specific wait cycle: an in-flight call intended
    /// for cycle `N` that races with cycle `N`'s guard drop and cycle `N + 1`'s `begin_wait` can
    /// apply to cycle `N + 1`. Callers reusing an event must tolerate spurious wakeups from a late
    /// `notify` (or late `interrupt`), and must serialize [`Self::assign_new_owner`] against
    /// wait-cycle turnover.
    ///
    /// Once called, this function cannot be called again until the returned [`EventWaitGuard`] is
    /// consumed by [`EventWaitGuard::block_until`] or dropped.
    ///
    /// # Panics
    ///
    /// Panics if a previous [`EventWaitGuard`] for this event has not yet been dropped.
    pub fn begin_wait(&self) -> EventWaitGuard<'_> {
        self.requeue_target.store(READY, Ordering::Relaxed);
        self.futex
            .compare_exchange(READY, WAITING, Ordering::AcqRel, Ordering::Relaxed)
            .expect("Tried to begin waiting on an event when not ready.");
        EventWaitGuard { event: self }
    }

    /// Assigns `new_owner` as the Priority Inheritance (PI) owner for waiting thread(s).
    ///
    /// If threads are currently waiting on `futex`, this dynamically requeues them to
    /// `requeue_target`, designating `new_owner` as the futex PI owner.
    ///
    /// This establishes Zircon Priority Inheritance (PI) from the waiting thread(s) to
    /// `new_owner` when the owner was not known at `begin_wait` time.
    ///
    /// Note: This can only be called once per `begin_wait` cycle (while the event is in the
    /// `WAITING` state). If the event is not waiting or has already been requeued/notified,
    /// this returns `Err(zx::Status::BAD_STATE)`.
    ///
    /// # Event Reuse and Cycle Turnover
    ///
    /// `assign_new_owner` is not bound to a specific wait-cycle token. If an `InterruptibleEvent`
    /// is reused across multiple wait cycles, an in-flight `assign_new_owner` intended for cycle
    /// `N` that races with cycle `N`'s [`EventWaitGuard`] drop and cycle `N + 1`'s `begin_wait`
    /// can succeed against cycle `N + 1`, causing cycle `N + 1`'s own `assign_new_owner` to fail
    /// with `BAD_STATE` and overriding any `new_owner` passed directly to
    /// [`EventWaitGuard::block_until`]. Callers that reuse an `InterruptibleEvent` with
    /// `assign_new_owner` must serialize `assign_new_owner` against wait-cycle turnover so a call
    /// from an earlier cycle cannot straddle into the next `begin_wait`.
    ///
    /// TODO(https://fxbug.dev/542307988): Replace this requeue pattern with a dedicated Zircon
    /// syscall to assign a new owner to a futex with waiting threads once available.
    pub fn assign_new_owner(&self, new_owner: &Arc<zx::Thread>) -> Result<(), zx::Status> {
        if self.futex.load(Ordering::Relaxed) != WAITING {
            return Err(zx::Status::BAD_STATE);
        }
        // Hold `requeue_owner` across the `WAITING -> REQUEUED` transition, `zx_futex_requeue`,
        // and `requeue_target` initialization so that any thread in `block_until` or `reset` that
        // observes `futex == REQUEUED` and locks `requeue_owner` is serialized after the requeue
        // completes (with kernel Priority Inheritance from `fuchsia_sync::Mutex` if
        // `assign_new_owner` was preempted).
        let mut requeue_owner = self.requeue_owner.lock();
        if self
            .futex
            .compare_exchange(WAITING, REQUEUED, Ordering::AcqRel, Ordering::Relaxed)
            .is_ok()
        {
            *requeue_owner = Some(Arc::clone(new_owner));
            let res =
                self.futex.requeue(0, REQUEUED, &self.requeue_target, u32::MAX, Some(new_owner));
            // Setting `requeue_target` to `WAITING` after `zx_futex_requeue` ensures that any
            // concurrent `wake()` loop does not issue `requeue_target.wake_all()` until after the
            // kernel wait queue transfer has completed.
            self.requeue_target.store(WAITING, Ordering::Release);
            res
        } else {
            Err(zx::Status::BAD_STATE)
        }
    }

    /// Returns whether `begin_wait` has been called and the event has not yet been notified,
    /// interrupted, or reset.
    pub fn is_waiting(&self) -> bool {
        let LoadedState { state, is_requeued, .. } = self.load_state();
        // Treat `READY && is_requeued` as waiting: `assign_new_owner` has already claimed `futex`
        // (`REQUEUED`) and is in the middle of transferring waiters before storing `WAITING` into
        // `requeue_target`.
        state == WAITING || (state == READY && is_requeued)
    }

    fn reset(&self) {
        // `reset()` is only called when dropping `EventWaitGuard` (either at the end of
        // `block_until` or if the guard is dropped before blocking), which is the unique guard
        // created by `begin_wait` (when `futex == READY`). Because `futex` is not set back to
        // `READY` until the end of this function, at most one thread can execute `reset()` at a
        // time.
        //
        // On normal non-requeued wakeups where `futex` is already `NOTIFIED` or `INTERRUPTED`,
        // `assign_new_owner` cannot transition `futex` to `REQUEUED` and `requeue_target` is
        // already `READY`, so we only need to serialize against `assign_new_owner` when `futex` is
        // `WAITING` or `REQUEUED`.
        let observed = self.futex.load(Ordering::Relaxed);
        if observed == WAITING || observed == REQUEUED {
            if self.futex.swap(NOTIFIED, Ordering::AcqRel) == REQUEUED {
                *self.requeue_owner.lock() = None;
            }
            self.requeue_target.store(READY, Ordering::Relaxed);
        }
        // Use `Ordering::Release` so clearing `requeue_owner` and resetting `requeue_target`
        // happen-before a subsequent `begin_wait()`'s `AcqRel` CAS from `READY` to `WAITING`.
        self.futex.store(READY, Ordering::Release);
    }

    fn block_until_with_clean_delay(
        &self,
        new_owner: Option<&zx::Thread>,
        deadline: zx::MonotonicInstant,
        clean_delay: zx::MonotonicDuration,
    ) -> Result<(), WakeReason> {
        // We need to loop around the call to zx_futex_wait because we can receive spurious
        // wakeups. Clean the stack once if the wait remains blocked for `clean_delay`.
        let mut clean_deadline = crate::initial_clean_deadline(deadline, clean_delay);
        loop {
            let LoadedState { futex: target_futex, state, is_requeued } = self.load_state();
            match state {
                // If `assign_new_owner()` set `futex = REQUEUED` before `requeue_target` was
                // initialized to `WAITING`, `state` briefly reads `READY`; locking `requeue_owner`
                // below serializes behind `assign_new_owner()` completing the transition.
                WAITING => {}
                READY if is_requeued => {}
                NOTIFIED => return Ok(()),
                INTERRUPTED => return Err(WakeReason::Interrupted),
                other => panic!("Unexpected event state: {other}"),
            }

            let requeued_owner = if is_requeued { self.requeue_owner.lock().clone() } else { None };
            let effective_owner = if is_requeued { requeued_owner.as_deref() } else { new_owner };

            match target_futex.wait(WAITING, effective_owner, clean_deadline) {
                // The deadline expired while we were sleeping.
                Err(zx::Status::TIMED_OUT) => {
                    if clean_deadline < deadline {
                        starnix_stack::clean_stack();
                        clean_deadline = deadline;
                    } else {
                        return Err(WakeReason::DeadlineExpired);
                    }
                }
                // The value changed before we went to sleep, or we woke up; the next iteration
                // re-loads the active futex and state via `load_state()`.
                Err(zx::Status::BAD_STATE) | Ok(()) => {}
                Err(e) => panic!("Unexpected error from zx_futex_wait: {e}"),
            }
        }
    }

    /// Wake up the event normally.
    ///
    /// If this function completes while no wait cycle is active (before `begin_wait` or after the
    /// [`EventWaitGuard`] is dropped), this notification is ignored; when an event is reused, an
    /// in-flight call racing with cycle turnover may wake the next wait cycle. Calling this
    /// function repeatedly within a cycle has no additional effect. If both `notify` and
    /// `interrupt` are called, the state observed by `block_until` is a race.
    pub fn notify(&self) {
        self.wake(NOTIFIED);
    }

    /// Wake up the event because of an interruption.
    ///
    /// If this function completes while no wait cycle is active (before `begin_wait` or after the
    /// [`EventWaitGuard`] is dropped), this interruption is ignored; when an event is reused, an
    /// in-flight call racing with cycle turnover may interrupt the next wait cycle. Calling this
    /// function repeatedly within a cycle has no additional effect. If both `notify` and
    /// `interrupt` are called, the state observed by `block_until` is a race.
    pub fn interrupt(&self) {
        self.wake(INTERRUPTED);
    }

    fn wake(&self, state: i32) {
        // Check `self.futex` in Shared cache state before attempting `compare_exchange` to avoid
        // unnecessary exclusive cache-line invalidations when the event is already READY,
        // NOTIFIED, or INTERRUPTED (see <https://marabos.nl/atomics/hardware.html#failing-compare-exchange>).
        //
        // When `assign_new_owner()` transitions `futex` to `REQUEUED`, there is a brief window
        // before `requeue_target` transitions to `WAITING` (or while `reset()` transitions back to
        // `READY`), so we yield and re-check `load_state()` until the transition settles.
        loop {
            let LoadedState { futex, state: current_state, is_requeued } = self.load_state();
            match current_state {
                NOTIFIED | INTERRUPTED => return,
                READY if !is_requeued => return,
                READY => std::thread::yield_now(),
                WAITING => {
                    if futex
                        .compare_exchange(WAITING, state, Ordering::Release, Ordering::Acquire)
                        .is_ok()
                    {
                        futex.wake_all();
                        return;
                    }
                }
                other => panic!("Unexpected event state: {other}"),
            }
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn test_wait_block_and_notify() {
        let event = InterruptibleEvent::new();

        let guard = event.begin_wait();

        let other_event = Arc::clone(&event);
        let thread = std::thread::spawn(move || {
            other_event.notify();
        });

        guard.block_until(None, zx::MonotonicInstant::INFINITE).expect("failed to be notified");
        thread.join().expect("failed to join thread");
    }

    #[test]
    fn test_wait_block_and_interrupt() {
        let event = InterruptibleEvent::new();

        let guard = event.begin_wait();

        let other_event = Arc::clone(&event);
        let thread = std::thread::spawn(move || {
            other_event.interrupt();
        });

        let result = guard.block_until(None, zx::MonotonicInstant::INFINITE);
        assert_eq!(result, Err(WakeReason::Interrupted));
        thread.join().expect("failed to join thread");
    }

    #[test]
    fn test_wait_block_and_timeout() {
        let event = InterruptibleEvent::new();

        let guard = event.begin_wait();
        let result = guard
            .block_until(None, zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(20)));
        assert_eq!(result, Err(WakeReason::DeadlineExpired));
    }

    #[test]
    fn futex_ownership_is_transferred() {
        let event = Arc::new(InterruptibleEvent::new());

        let (root_thread_handle, root_thread_koid) = fuchsia_runtime::with_thread_self(|thread| {
            (thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(), thread.koid().unwrap())
        });

        let event_for_blocked_thread = event.clone();

        let blocked_thread = std::thread::spawn(move || {
            let event = event_for_blocked_thread;
            let guard = event.begin_wait();
            guard.block_until(Some(&root_thread_handle), zx::MonotonicInstant::INFINITE).unwrap();
        });

        // Wait for the correct owner to appear.
        // TODO(b/502692311): Replace this polling loop if it starts timing out.
        while event.get_owner() != Some(root_thread_koid) {
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        event.notify();
        blocked_thread.join().unwrap();
    }

    #[test]
    fn stale_pi_owner_is_noop() {
        let mut new_owner = None;
        std::thread::scope(|s| {
            s.spawn(|| {
                new_owner = Some(
                    fuchsia_runtime::with_thread_self(|thread| {
                        thread.duplicate_handle(zx::Rights::SAME_RIGHTS)
                    })
                    .unwrap(),
                );
            });
        });
        let new_owner = new_owner.unwrap();

        let event = InterruptibleEvent::new();
        let guard = event.begin_wait();
        let result = guard.block_until(
            Some(&new_owner),
            zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(20)),
        );
        assert_eq!(result, Err(WakeReason::DeadlineExpired));
    }

    #[test]
    fn futex_ownership_is_transferred_via_requeue() {
        let event = Arc::new(InterruptibleEvent::new());

        let (root_thread_handle, root_thread_koid) = fuchsia_runtime::with_thread_self(|thread| {
            (
                Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                thread.koid().unwrap(),
            )
        });

        let event_for_blocked_thread = event.clone();

        let (tx, rx) = std::sync::mpsc::channel();
        let blocked_thread = std::thread::spawn(move || {
            let (thread_handle, _) = fuchsia_runtime::with_thread_self(|thread| {
                (thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(), thread.koid().unwrap())
            });
            tx.send(thread_handle).unwrap();
            let event = event_for_blocked_thread;
            let guard = event.begin_wait();
            guard.block_until(None, zx::MonotonicInstant::INFINITE).unwrap();
        });

        // Wait until the thread starts sleeping on the primary futex in the kernel.
        let blocked_thread_handle = rx.recv().unwrap();
        while blocked_thread_handle.info().unwrap().state
            != zx::ThreadState::Blocked(zx::ThreadBlockType::Futex)
        {
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        // Dynamically assign PI ownership to root_thread_handle.
        event.assign_new_owner(&root_thread_handle).unwrap();

        // Wait for the correct requeue owner to appear.
        while event.get_owner() != Some(root_thread_koid) {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }

        event.notify();
        blocked_thread.join().unwrap();
    }

    #[test]
    fn concurrent_wait_wake_assign_new_owner_stress_test() {
        let root_thread_handle = fuchsia_runtime::with_thread_self(|thread| {
            Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
        });

        for _ in 0..200 {
            let event = Arc::new(InterruptibleEvent::new());
            let barrier = Arc::new(std::sync::Barrier::new(2));

            let event_waiter = event.clone();
            let event_assign = event.clone();
            let event_waker = event.clone();
            let barrier_assign = barrier.clone();
            let barrier_waker = barrier.clone();
            let thread_handle = Arc::clone(&root_thread_handle);

            let waiter = std::thread::spawn(move || {
                let guard = event_waiter.begin_wait();
                let _ = guard.block_until(None, zx::MonotonicInstant::INFINITE);
            });

            while event.futex.load(Ordering::Relaxed) != WAITING {
                std::thread::yield_now();
            }

            let assigner = std::thread::spawn(move || {
                barrier_assign.wait();
                let _ = event_assign.assign_new_owner(&thread_handle);
            });

            let waker = std::thread::spawn(move || {
                barrier_waker.wait();
                event_waker.notify();
            });

            assigner.join().unwrap();
            waker.join().unwrap();
            waiter.join().unwrap();
        }
    }

    #[test]
    fn futex_ownership_is_transferred_when_requeued_before_block() {
        let event = InterruptibleEvent::new();

        let (root_thread_handle, root_thread_koid) = fuchsia_runtime::with_thread_self(|thread| {
            (
                Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                thread.koid().unwrap(),
            )
        });

        let event_for_blocked_thread = Arc::clone(&event);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        let blocked_thread = std::thread::spawn(move || {
            let guard = event_for_blocked_thread.begin_wait();
            ready_tx.send(()).unwrap();
            go_rx.recv().unwrap();
            guard.block_until(None, zx::MonotonicInstant::INFINITE).unwrap();
        });

        // Call `assign_new_owner` after `begin_wait()`, before the thread calls `block_until()`.
        ready_rx.recv().unwrap();
        event.assign_new_owner(&root_thread_handle).unwrap();
        go_tx.send(()).unwrap();

        while event.get_owner() != Some(root_thread_koid) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        event.notify();
        blocked_thread.join().unwrap();
    }

    #[test]
    fn test_wait_clean_stack_then_notify() {
        let event = InterruptibleEvent::new();
        let guard = event.begin_wait();

        let other_event = Arc::clone(&event);
        let thread = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(30));
            other_event.notify();
        });

        guard
            .block_until_with_clean_delay(
                None,
                zx::MonotonicInstant::INFINITE,
                zx::MonotonicDuration::from_millis(10),
            )
            .expect("failed to be notified after clean_stack");
        thread.join().expect("failed to join thread");
    }

    #[test]
    fn test_wait_clean_stack_then_timeout() {
        let event = InterruptibleEvent::new();
        let guard = event.begin_wait();
        let deadline = zx::MonotonicInstant::after(zx::MonotonicDuration::from_millis(30));

        let result = guard.block_until_with_clean_delay(
            None,
            deadline,
            zx::MonotonicDuration::from_millis(10),
        );
        assert_eq!(result, Err(WakeReason::DeadlineExpired));
        assert!(zx::MonotonicInstant::get() >= deadline);
    }

    #[test]
    fn futex_requeue_owner_is_preserved_across_clean_stack() {
        let event = InterruptibleEvent::new();

        let (root_thread_handle, root_thread_koid) = fuchsia_runtime::with_thread_self(|thread| {
            (
                Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                thread.koid().unwrap(),
            )
        });

        let event_for_blocked_thread = Arc::clone(&event);
        let (ready_tx, ready_rx) = std::sync::mpsc::channel();
        let (go_tx, go_rx) = std::sync::mpsc::channel();
        let blocked_thread = std::thread::spawn(move || {
            let thread_handle = fuchsia_runtime::with_thread_self(|thread| {
                thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()
            });
            let guard = event_for_blocked_thread.begin_wait();
            ready_tx.send(thread_handle).unwrap();
            go_rx.recv().unwrap();
            guard
                .block_until_with_clean_delay(
                    None,
                    zx::MonotonicInstant::INFINITE,
                    zx::MonotonicDuration::from_millis(25),
                )
                .unwrap();
        });

        // Assign PI ownership after `begin_wait()` before `blocked_thread` enters
        // `block_until_with_clean_delay`, so the 25ms `clean_deadline` cannot expire before
        // `assign_new_owner` runs even on a loaded bot.
        let blocked_thread_handle = ready_rx.recv().unwrap();
        event.assign_new_owner(&root_thread_handle).unwrap();
        go_tx.send(()).unwrap();

        while event.get_owner() != Some(root_thread_koid) {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }

        // Sleep past the 25ms clean_deadline so the blocked thread wakes, calls `clean_stack()`,
        // and re-waits on `requeue_target`.
        std::thread::sleep(std::time::Duration::from_millis(50));
        while blocked_thread_handle.info().unwrap().state
            != zx::ThreadState::Blocked(zx::ThreadBlockType::Futex)
        {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(event.get_owner(), Some(root_thread_koid));

        event.notify();
        blocked_thread.join().unwrap();
    }

    #[test]
    fn test_stack_allocated_wait_notify_and_interrupt() {
        let event = InterruptibleEvent::default();
        let guard = event.begin_wait();
        std::thread::scope(|s| {
            s.spawn(|| {
                event.notify();
            });
            guard.block_until(None, zx::MonotonicInstant::INFINITE).expect("failed to be notified");
        });

        let guard = event.begin_wait();
        std::thread::scope(|s| {
            s.spawn(|| {
                event.interrupt();
            });
            let result = guard.block_until(None, zx::MonotonicInstant::INFINITE);
            assert_eq!(result, Err(WakeReason::Interrupted));
        });
    }

    #[test]
    fn test_is_waiting_during_requeue_transition() {
        let event = InterruptibleEvent::new();
        assert!(!event.is_waiting());

        let guard = event.begin_wait();
        assert!(event.is_waiting());

        // Simulate the mid-transition window in `assign_new_owner` where `futex` has transitioned
        // to `REQUEUED` while `requeue_target` is still `READY`.
        event.futex.store(REQUEUED, Ordering::Release);
        assert_eq!(event.requeue_target.load(Ordering::Relaxed), READY);
        assert!(event.is_waiting());

        drop(guard);
        assert!(!event.is_waiting());

        let guard = event.begin_wait();
        event.notify();
        guard.block_until(None, zx::MonotonicInstant::INFINITE).unwrap();
        assert!(!event.is_waiting());
    }

    #[test]
    fn test_dropping_wait_guard_resets_event() {
        let event = InterruptibleEvent::new();
        assert!(!event.is_waiting());
        {
            let _guard = event.begin_wait();
            assert!(event.is_waiting());
        }
        assert!(!event.is_waiting());

        let guard = event.begin_wait();
        event.notify();
        guard.block_until(None, zx::MonotonicInstant::INFINITE).unwrap();
        assert!(!event.is_waiting());
    }

    #[test]
    fn test_dropping_requeued_wait_guard_releases_owner() {
        let event = InterruptibleEvent::new();
        let owner = fuchsia_runtime::with_thread_self(|thread| {
            Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
        });
        assert_eq!(Arc::strong_count(&owner), 1);

        {
            let _guard = event.begin_wait();
            event.assign_new_owner(&owner).unwrap();
            assert!(event.is_waiting());
            assert_eq!(Arc::strong_count(&owner), 2);
        }

        assert!(!event.is_waiting());
        assert_eq!(Arc::strong_count(&owner), 1);
        assert_eq!(event.get_owner(), None);

        let guard = event.begin_wait();
        event.notify();
        guard.block_until(None, zx::MonotonicInstant::INFINITE).unwrap();
        assert!(!event.is_waiting());
    }

    #[test]
    fn test_notify_before_block_resets_event_for_reuse() {
        let event = InterruptibleEvent::new();

        let guard = event.begin_wait();
        event.notify();
        assert!(!event.is_waiting());
        guard.block_until(None, zx::MonotonicInstant::INFINITE).unwrap();
        assert!(!event.is_waiting());

        let guard = event.begin_wait();
        event.notify();
        guard.block_until(None, zx::MonotonicInstant::INFINITE).unwrap();
        assert!(!event.is_waiting());
    }

    #[test]
    fn test_reuse_after_requeued_block() {
        const POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);

        let event = InterruptibleEvent::new();
        let (owner, owner_koid) = fuchsia_runtime::with_thread_self(|thread| {
            (
                Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()),
                thread.koid().unwrap(),
            )
        });
        assert_eq!(Arc::strong_count(&owner), 1);

        // Cycle 1: Real requeued block woken by `notify()`.
        let event_for_waiter = Arc::clone(&event);
        let (thread_tx, thread_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let thread_handle = fuchsia_runtime::with_thread_self(|thread| {
                thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()
            });
            let guard = event_for_waiter.begin_wait();
            thread_tx.send(thread_handle).unwrap();
            guard.block_until(None, zx::MonotonicInstant::INFINITE)
        });

        let waiter_handle = thread_rx.recv().unwrap();
        while waiter_handle.info().unwrap().state
            != zx::ThreadState::Blocked(zx::ThreadBlockType::Futex)
        {
            std::thread::sleep(POLL_INTERVAL);
        }

        event.assign_new_owner(&owner).unwrap();
        assert_eq!(Arc::strong_count(&owner), 2);
        assert_eq!(event.get_owner(), Some(owner_koid));

        event.notify();
        assert_eq!(waiter.join().unwrap(), Ok(()));
        assert!(!event.is_waiting());
        assert_eq!(Arc::strong_count(&owner), 1);
        assert_eq!(event.get_owner(), None);

        // Cycle 2: Reuse the same event for a second real requeued block woken by `interrupt()`.
        let event_for_waiter = Arc::clone(&event);
        let (thread_tx, thread_rx) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let thread_handle = fuchsia_runtime::with_thread_self(|thread| {
                thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap()
            });
            let guard = event_for_waiter.begin_wait();
            thread_tx.send(thread_handle).unwrap();
            guard.block_until(None, zx::MonotonicInstant::INFINITE)
        });

        let waiter_handle = thread_rx.recv().unwrap();
        while waiter_handle.info().unwrap().state
            != zx::ThreadState::Blocked(zx::ThreadBlockType::Futex)
        {
            std::thread::sleep(POLL_INTERVAL);
        }

        event.assign_new_owner(&owner).unwrap();
        assert_eq!(Arc::strong_count(&owner), 2);
        assert_eq!(event.get_owner(), Some(owner_koid));

        event.interrupt();
        assert_eq!(waiter.join().unwrap(), Err(WakeReason::Interrupted));
        assert!(!event.is_waiting());
        assert_eq!(Arc::strong_count(&owner), 1);
        assert_eq!(event.get_owner(), None);
    }

    #[test]
    fn test_reuse_after_timeout_and_interrupt() {
        const SHORT_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(20);

        let event = InterruptibleEvent::new();

        // Spawn a distinct live thread to serve as a valid PI owner while the main thread blocks.
        let (owner_tx, owner_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let owner_thread = std::thread::spawn(move || {
            let handle = fuchsia_runtime::with_thread_self(|thread| {
                Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
            });
            owner_tx.send(handle).unwrap();
            let _ = stop_rx.recv();
        });
        let owner = owner_rx.recv().unwrap();
        assert_eq!(Arc::strong_count(&owner), 1);

        // Cycle 1: Timeout on standard wait.
        let guard = event.begin_wait();
        assert_eq!(
            guard.block_until(None, zx::MonotonicInstant::after(SHORT_TIMEOUT)),
            Err(WakeReason::DeadlineExpired)
        );
        assert!(!event.is_waiting());

        // Cycle 2: Timeout on requeued wait releases `requeue_owner`.
        let guard = event.begin_wait();
        event.assign_new_owner(&owner).unwrap();
        assert_eq!(Arc::strong_count(&owner), 2);
        assert_eq!(
            guard.block_until(None, zx::MonotonicInstant::after(SHORT_TIMEOUT)),
            Err(WakeReason::DeadlineExpired)
        );
        assert!(!event.is_waiting());
        assert_eq!(Arc::strong_count(&owner), 1);
        assert_eq!(event.get_owner(), None);

        // Cycle 3: Interruption.
        let guard = event.begin_wait();
        let other_event = Arc::clone(&event);
        let interrupter = std::thread::spawn(move || {
            other_event.interrupt();
        });
        assert_eq!(
            guard.block_until(None, zx::MonotonicInstant::INFINITE),
            Err(WakeReason::Interrupted)
        );
        interrupter.join().unwrap();
        assert!(!event.is_waiting());

        // Cycle 4: Normal notification after prior timeouts and interruption.
        let guard = event.begin_wait();
        let other_event = Arc::clone(&event);
        let notifier = std::thread::spawn(move || {
            other_event.notify();
        });
        assert_eq!(guard.block_until(None, zx::MonotonicInstant::INFINITE), Ok(()));
        notifier.join().unwrap();
        assert!(!event.is_waiting());

        drop(stop_tx);
        owner_thread.join().unwrap();
    }

    #[test]
    fn test_concurrent_notify_during_reuse() {
        const REUSE_ITERATIONS: usize = 500;

        let event = InterruptibleEvent::new();
        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let notifier_event = &event;
            let done_ref = &done;
            scope.spawn(move || {
                while !done_ref.load(Ordering::Relaxed) {
                    notifier_event.notify();
                }
            });
            scopeguard::defer! {
                done.store(true, Ordering::Relaxed);
            }
            for i in 0..REUSE_ITERATIONS {
                let guard = event.begin_wait();
                if i % 4 == 0 {
                    drop(guard);
                } else {
                    assert_eq!(guard.block_until(None, zx::MonotonicInstant::INFINITE), Ok(()));
                }
            }
        });
        assert!(!event.is_waiting());
    }

    #[test]
    fn test_concurrent_interrupt_and_requeue_during_reuse() {
        const REUSE_ITERATIONS: usize = 500;

        let event = InterruptibleEvent::new();
        let (owner_tx, owner_rx) = std::sync::mpsc::channel();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let owner_thread = std::thread::spawn(move || {
            let handle = fuchsia_runtime::with_thread_self(|thread| {
                Arc::new(thread.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap())
            });
            owner_tx.send(handle).unwrap();
            let _ = stop_rx.recv();
        });
        let owner = owner_rx.recv().unwrap();
        assert_eq!(Arc::strong_count(&owner), 1);

        let done = AtomicBool::new(false);
        std::thread::scope(|scope| {
            let interrupter_event = &event;
            let assigner_event = &event;
            let owner_ref = &owner;
            let done_ref = &done;
            scope.spawn(move || {
                while !done_ref.load(Ordering::Relaxed) {
                    interrupter_event.interrupt();
                }
            });
            scope.spawn(move || {
                while !done_ref.load(Ordering::Relaxed) {
                    let _ = assigner_event.assign_new_owner(owner_ref);
                }
            });
            scopeguard::defer! {
                done.store(true, Ordering::Relaxed);
            }
            for i in 0..REUSE_ITERATIONS {
                let guard = event.begin_wait();
                if i % 4 == 0 {
                    drop(guard);
                } else {
                    assert_eq!(
                        guard.block_until(None, zx::MonotonicInstant::INFINITE),
                        Err(WakeReason::Interrupted)
                    );
                }
            }
        });

        assert!(!event.is_waiting());
        assert_eq!(Arc::strong_count(&owner), 1);
        assert_eq!(event.get_owner(), None);

        drop(stop_tx);
        owner_thread.join().unwrap();
    }
}
