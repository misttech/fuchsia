// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;
use std::sync::atomic::Ordering;

/// A blocking object that can either be notified normally or interrupted
///
/// To block using an `InterruptibleEvent`, first call `begin_wait`. At this point, the event is
/// in the "waiting" state, and future calls to `notify` or `interrupt` will terminate the wait.
///
/// After `begin_wait` returns, call `block_until` to block the current thread until one of the
/// following conditions occur:
///
///  1. The given deadline expires.
///  2. At least one of the `notify` or `interrupt` functions were called after `begin_wait`.
///
/// It's safe to call `notify` or `interrupt` at any time. However, calls to `begin_wait` and
/// `block_until` must alternate, starting with `begin_wait`.
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
    requeue_owner: fuchsia_sync::Mutex<Option<Arc<zx::Thread>>>,
}

/// The initial state.
///
///  * Transitions to `WAITING` after `begin_wait`.
const READY: i32 = 0;

/// The event is waiting for a notification or an interruption.
///
///  * Transitions to `NOTIFIED` after `notify`.
///  * Transitions to `INTERRUPTED` after `interrupt`.
///  * Transitions to `REQUEUED` after `assign_new_owner`.
///  * Transitions to `READY` if the deadline for `block_until` expires.
const WAITING: i32 = 1;

/// The event has been notified and will wake up.
///
///  * Transitions to `READY` after `block_until` processes the notification.
const NOTIFIED: i32 = 2;

/// The event has been interrupted and will wake up.
///
///  * Transitions to `READY` after `block_until` processes the interruption.
const INTERRUPTED: i32 = 3;

/// The event has been requeued to a secondary futex target with a new PI owner.
const REQUEUED: i32 = 4;

/// A guard object to enforce that clients call `begin_wait` before `block_until`.
#[must_use = "call block_until to advance the event state machine"]
pub struct EventWaitGuard<'a> {
    event: &'a Arc<InterruptibleEvent>,
}

impl<'a> EventWaitGuard<'a> {
    /// The underlying event associated with this guard.
    pub fn event(&self) -> &'a Arc<InterruptibleEvent> {
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
            requeue_owner: fuchsia_sync::Mutex::new(None),
        }
    }
}

impl InterruptibleEvent {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    /// Returns the owner of the underlying futex or requeue target futex, if any.
    pub fn get_owner(&self) -> Option<zx::Koid> {
        self.futex.get_owner().or_else(|| self.requeue_target.get_owner())
    }

    /// Called to initiate a wait.
    ///
    /// Calls to `notify` or `interrupt` after this function returns will cause the event to wake
    /// up. Calls to those functions prior to calling `begin_wait` will be ignored.
    ///
    /// Once called, this function cannot be called again until `block_until` returns. Otherwise,
    /// this function will panic.
    pub fn begin_wait<'a>(self: &'a Arc<Self>) -> EventWaitGuard<'a> {
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

    fn reset(&self) {
        // `reset()` is only called at the end of `block_until`, which consumes the unique
        // `EventWaitGuard` created by `begin_wait` (when `futex == READY`). Because `futex` is not
        // set back to `READY` until the end of this function, at most one thread can execute
        // `reset()` at a time.
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
            let futex_val = self.futex.load(Ordering::Acquire);
            let is_requeued = futex_val == REQUEUED;
            let requeued_owner = if is_requeued { self.requeue_owner.lock().clone() } else { None };
            let (target_futex, effective_owner) = if is_requeued {
                (&self.requeue_target, requeued_owner.as_deref())
            } else {
                (&self.futex, new_owner)
            };

            match target_futex.wait(WAITING, effective_owner, clean_deadline) {
                // The deadline expired while we were sleeping.
                Err(zx::Status::TIMED_OUT) => {
                    if clean_deadline < deadline {
                        starnix_stack::clean_stack();
                        clean_deadline = deadline;
                        // Continue immediately so we re-read `self.futex` and `requeue_owner` in
                        // case `assign_new_owner` requeued us during the initial `clean_delay`
                        // sleep.
                        continue;
                    } else {
                        self.reset();
                        return Err(WakeReason::DeadlineExpired);
                    }
                }
                // The value changed before we went to sleep. Note: If `assign_new_owner()` set
                // `futex = REQUEUED` before `requeue_target` was initialized to `WAITING`,
                // `target_futex.wait()` fails with `BAD_STATE`. In this case, `state` evaluates to
                // `WAITING` or `READY`, causing the loop to retry on `requeue_target`.
                Err(zx::Status::BAD_STATE) => (),
                Err(e) => panic!("Unexpected error from zx_futex_wait: {e}"),
                Ok(()) => (),
            }

            let mut state = target_futex.load(Ordering::Acquire);
            if state == REQUEUED {
                state = self.requeue_target.load(Ordering::Acquire);
            }

            let res = match state {
                // If we're still in the `WAITING` state (or `READY` while `assign_new_owner` is
                // initializing `requeue_target`), then the wake ended spuriously and we need to go
                // back to sleep.
                WAITING | READY => continue,
                NOTIFIED => Ok(()),
                INTERRUPTED => Err(WakeReason::Interrupted),
                _ => panic!("Unexpected event state: {state}"),
            };
            self.reset();
            return res;
        }
    }

    /// Wake up the event normally.
    ///
    /// If this function is called before `begin_wait`, this notification is ignored. Calling this
    /// function repeatedly has no effect. If both `notify` and `interrupt` are called, the state
    /// observed by `block_until` is a race.
    pub fn notify(&self) {
        self.wake(NOTIFIED);
    }

    /// Wake up the event because of an interruption.
    ///
    /// If this function is called before `begin_wait`, this notification is ignored. Calling this
    /// function repeatedly has no effect. If both `notify` and `interrupt` are called, the state
    /// observed by `block_until` is a race.
    pub fn interrupt(&self) {
        self.wake(INTERRUPTED);
    }

    fn wake(&self, state: i32) {
        // Fast path for standard (non-requeued) events:
        // See <https://marabos.nl/atomics/hardware.html#failing-compare-exchange> for why we issue
        // this relaxed load before the `compare_exchange` below. Checking `observed == WAITING`
        // in Shared cache state prevents unnecessary exclusive cache line invalidations (RFOs)
        // when the event is already NOTIFIED, INTERRUPTED, or REQUEUED.
        //
        // We specify `Ordering::Acquire` on failure so that if `compare_exchange` fails because
        // another thread concurrently assigned a new owner (`futex` transitioned to `REQUEUED`),
        // we observe the updated state with Acquire semantics before moving to the slow path.
        let observed = self.futex.load(Ordering::Relaxed);
        if observed == WAITING
            && self
                .futex
                .compare_exchange(WAITING, state, Ordering::Release, Ordering::Acquire)
                .is_ok()
        {
            self.futex.wake_all();
            return;
        }

        // Slow path: `futex` was either already NOTIFIED/INTERRUPTED/READY, or `assign_new_owner()`
        // claimed `futex` by setting it to `REQUEUED`.
        //
        // This loop only applies when `assign_new_owner()` is called (e.g. during Binder worker
        // dispatch). When `assign_new_owner()` transitions `futex` to `REQUEUED`, there is a brief
        // window before it initializes `requeue_target` to `WAITING` and executes
        // `zx_futex_requeue`.
        //
        // We spin/yield until `requeue_target` transitions to `WAITING` (or until another thread
        // notifies it), ensuring that we wake the waiter on `requeue_target` without missing
        // notifications or deadlocking.
        loop {
            let futex_val = self.futex.load(Ordering::Acquire);
            if futex_val == NOTIFIED || futex_val == INTERRUPTED || futex_val == READY {
                return;
            }

            // futex_val is REQUEUED. Attempt to wake requeue_target once it is WAITING.
            let requeue_val = self.requeue_target.load(Ordering::Acquire);
            if requeue_val == WAITING {
                if self
                    .requeue_target
                    .compare_exchange(WAITING, state, Ordering::Release, Ordering::Acquire)
                    .is_ok()
                {
                    self.requeue_target.wake_all();
                    return;
                }
            } else if requeue_val == NOTIFIED || requeue_val == INTERRUPTED {
                return;
            }

            std::thread::yield_now();
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;

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
}
