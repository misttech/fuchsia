// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! A basic counting semaphore, implementing the C++ `Semaphore` declared in
//! `<kernel/semaphore.h>`.

use core::convert::Infallible;
use core::mem::{align_of, offset_of, size_of};
use core::sync::atomic::{AtomicI64, Ordering};
use pin_init::{PinInit, pin_data};
use semaphore_bindings as bindings;
use zr::static_assert;
use zx_status::Status;
use zx_types::zx_status_t;

use crate::arch_rs::ops::num_spinlocks_held;
use crate::kconcurrent::{
    Action, AllowFinalized, ChainLockTransaction, ChainLockTransactionToken, DONE,
    EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION, IRQ_SAVE_OPTION, OwnerGuard, RecordBackoff,
    TransactionResult, clt_tag, single_chain_lock_guard_owned,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::kernel::scheduler;
use crate::kernel::thread::{PreemptDisabledToken, Thread, ThreadPtr, preempt_is_enabled};
use crate::kernel::wait_queue::{Interruptible, ResourceOwnership, WaitQueue, WaitQueueBase};

/// A basic counting semaphore used to control access to a shared resource.
///
/// Think of `Semaphore` as a gatekeeper that allows a certain number of threads to pass through
/// a gate so they can access some resource.
///
/// Threads queue up at the gate by calling [`Semaphore::wait`] and then block until the
/// gatekeeper lets them through or they give up and leave the queue because of timeout or thread
/// signal. In the case of timeout or thread signal, the waiter may not proceed to the resource.
///
/// Calling [`Semaphore::post`] tells the gatekeeper that they may immediately admit one through
/// the gate (if there's a waiter in the queue) or admit one in the future once a waiter has
/// queued up.
///
/// Layout-compatible with the C++ `Semaphore` class, whose methods forward to the
/// `rust_semaphore_*` functions below. Every byte of a `Semaphore` is interior-mutable (an
/// atomic, or the `UnsafeCell`-backed state of the `!Freeze` [`WaitQueue`]; there is no padding),
/// so `&Semaphore` references remain sound while other CPUs operate on it. Do not add a `Freeze`
/// field.
#[pin_data]
#[repr(C)]
pub struct Semaphore {
    /// This struct has two fields that must be kept in sync, a count and a `WaitQueue`. There are
    /// some rules for keeping them in sync. See the implementation comments of `post_worker` and
    /// `wait_worker` for details.
    ///
    /// When `count` is greater than zero, a call to `wait` will decrement the count and "fall
    /// through" without blocking.
    ///
    /// When `count` is zero or negative, a call to `wait` will decrement `count` and block. Note,
    /// a negative value does not necessarily imply there are currently waiters in the `waitq`.
    /// It's possible to have a negative count and no waiters because a `wait` can timeout or be
    /// interrupted (e.g. by thread kill or thread suspend). The next call to `post` after an
    /// interrupted `wait` will update the `count`.
    ///
    /// Many methods on `waitq` require the caller to be holding the `WaitQueue`'s lock. In order
    /// to reduce lock contention and improve scalability, the `post` and `wait` operations are
    /// designed to only acquire the lock when necessary. In particular:
    ///
    /// 1. `post` does not need to access `waitq` if it knows that no threads are blocked (i.e.
    ///    when `count` is non-negative).
    ///
    /// 2. `wait` does not need to access the `waitq` if the caller does not need to block (i.e.
    ///    when `count` is greater than zero).
    ///
    /// It's critical that the lock protecting `waitq` be held when transitioning `count` from
    /// negative to non-negative or from non-negative to negative.
    count: AtomicI64,
    #[pin]
    waitq: WaitQueue,
}

// Compile-time layout assertions against the C++ Semaphore type via bindgen.
static_assert!(size_of::<Semaphore>() == size_of::<bindings::Semaphore>());
static_assert!(align_of::<Semaphore>() == align_of::<bindings::Semaphore>());
static_assert!(offset_of!(Semaphore, count) == offset_of!(bindings::Semaphore, count_));
static_assert!(offset_of!(Semaphore, waitq) == offset_of!(bindings::Semaphore, waitq_));

impl Semaphore {
    /// Creates an in-place pinned initializer for a `Semaphore` with `initial_count`.
    pub fn new(initial_count: i64) -> impl PinInit<Self, Infallible> {
        pin_init::pin_init!(Self {
            count: AtomicI64::new(initial_count),
            waitq <- WaitQueue::init(),
        })
    }

    /// Unblocks a single thread if any are waiting. If none are waiting, this operation logically
    /// increments the count such that a future call to [`Semaphore::wait`] may return without
    /// blocking.
    ///
    /// `post` has release memory order semantics and synchronizes with `wait`.
    ///
    /// `queue_to_own` is the [`OwnedWaitQueue`] that the unblocked thread will take ownership of.
    /// If this is `Some`, then we will perform an assign-owner operation on the queue. If there is
    /// no thread waiting, then no assign-owner operation will occur.
    ///
    /// Every call to this method must wake one waiter unless the queue is empty.
    ///
    /// When leaving this method, we must have either
    ///   - incremented a non-negative count or
    ///   - unblocked one thread and
    ///     - left an empty queue with a count of zero or
    ///     - left a non-empty queue with negative count
    ///
    /// Must not be called while a [`ChainLockTransaction`] is active on the current CPU (asserted
    /// by [`ChainLockTransaction::until_done`] whenever `waitq`'s lock is needed). If any
    /// spinlocks are held, preemption must have been disabled outside of the outermost one.
    #[inline]
    pub fn post(&self, queue_to_own: Option<&OwnedWaitQueue>) {
        // SAFETY: `self` is a valid `Semaphore`, kept alive by the caller's borrow for the whole
        // call.
        unsafe { Self::post_worker(self, queue_to_own) }
    }

    /// Implementation of [`Semaphore::post`], taking `this: *const Self` so that no `&Semaphore`
    /// is held after `waitq`'s lock is released (allowing a woken waiter to immediately destroy
    /// the `Semaphore`, as permitted by the C++ `Semaphore::Post`).
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid `Semaphore` that remains alive at least until the count has
    ///   been updated without waking a waiter, or until a waiter has been dequeued and `waitq`'s
    ///   lock released.
    /// - The requirements of [`Semaphore::post`] regarding active transactions and held spinlocks
    ///   apply.
    #[inline]
    unsafe fn post_worker(this: *const Self, queue_to_own: Option<&OwnedWaitQueue>) {
        // When calling post, we know we cannot be holding any chain locks (since we
        // demand that there be no transaction in process). In addition, if they are
        // holding any spinlocks, they need to have disabled local preemption outside
        // of the outer-most scope of the spin-lock(s) they hold.  Otherwise, posting
        // to the semaphore might result in a local preemption while a spin-lock is
        // held, something which cannot be permitted.
        debug_assert!(
            !preempt_is_enabled() || (num_spinlocks_held() == 0),
            "Preemption {} Num Spinlocks {}",
            u8::from(preempt_is_enabled()),
            num_spinlocks_held()
        );

        // Is the count greater than or equal to zero?  If so, increment and return.
        // Take care to not increment a negative count.
        //
        // Memory ordering: a successful increment is `Release`, pairing with the `Acquire`
        // decrement in `wait_worker`. The initial load and the CAS failure ordering are
        // `Relaxed` because the value read is only a hint, which the next CAS validates.
        {
            // SAFETY: The caller guarantees `this` points to a valid `Semaphore`, and no waiter
            // can be woken (and so destroy it) while this reference is in use.
            let count: &AtomicI64 = unsafe { &(*this).count };
            let mut old_count = count.load(Ordering::Relaxed);
            while old_count >= 0 {
                match count.compare_exchange_weak(
                    old_count,
                    old_count + 1,
                    Ordering::Release,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return,
                    Err(observed) => old_count = observed,
                }
            }
        }

        // SAFETY: Forwarded from this function's `# Safety`.
        unsafe { Self::post_slow(this, queue_to_own) }
    }

    /// Lock-acquiring slow path of [`Semaphore::post_worker`], taken when the count was observed
    /// to be negative (so there may be waiters to wake).
    ///
    /// This is kept out of line (but not `#[cold]`, since it is the normal path whenever a waiter
    /// is blocked) so that the lock-free fast path in `post_worker` needs no stack frame or
    /// callee-saved registers.
    ///
    /// # Safety
    ///
    /// Those of [`Semaphore::post_worker`].
    #[inline(never)]
    unsafe fn post_slow(this: *const Self, queue_to_own: Option<&OwnedWaitQueue>) {
        // The fast-path has failed, looks like we are going to need to obtain some
        // locks in order to proceed.  Create the CLT we will need to do so now.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables eager rescheduling,
            // which counts as preemption being disabled for the duration of this transaction.
            let preempt_disabled = unsafe { PreemptDisabledToken::new() };

            let thread_guard = {
                // SAFETY: The caller guarantees `this` points to a valid `Semaphore` that stays
                // live at least until `guard.release()` at the end of this block (or until a
                // return from this block that wakes no waiter). Nothing reads it afterwards.
                let sema: &Self = unsafe { &*this };

                // We observed a negative count and have not yet incremented it.  There might
                // be waiters waiting.  We'll need to acquire the lock and check.
                let mut guard = OwnerGuard::new(&sema.waitq, token);

                // Because we hold the lock we know that no other thread can transition the
                // count from non-negative to negative or vice versa.  If the count is
                // non-negative, then there must be no waiters so just increment and return.
                //
                // Memory ordering: this load is `Relaxed` because every transition across zero
                // is made with `waitq`'s lock held, which we now hold. The updates to 1 below are
                // `Release` for the same reason as the fast path: a later `wait` may consume the
                // count without taking the lock.
                let old_count = sema.count.load(Ordering::Relaxed);
                if old_count >= 0 {
                    debug_assert!(
                        guard.proof().count() == 0,
                        "q={} old_count={}",
                        guard.proof().count(),
                        old_count
                    );
                    sema.count.fetch_add(1, Ordering::Release);
                    return DONE.into();
                }

                // Because we hold the lock we know the number of waiters cannot change out
                // from under us.  At this point we know the count is negative, and it cannot
                // become non-negative without holding our queue's lock.  Check for waiters to
                // wake.
                let num_in_queue = guard.proof().count();
                if num_in_queue == 0 {
                    // There are no waiters to wake.  They must have timed out or been
                    // interrupted.  Reset the count and perform our increment.
                    sema.count.store(1, Ordering::Release);
                    return DONE.into();
                }

                // At this point we know there's at least one waiter waiting.  We're committed
                // to waking.  However, we need to determine what the count should be when we
                // drop the lock and return.  After we wake, if there are no waiters left, the
                // count should be reset to zero, otherwise it should remain negative.
                //
                // Try to wake one of the threads in the queue, but be ready for it to fail
                // because of a backoff error.
                let thread_ptr =
                    guard.proof().peek_front().expect("non-empty wait queue has a front");
                // SAFETY: `num_in_queue != 0`, so `peek_front` returned a thread which is
                // currently blocked in `sema.waitq`, and which cannot exit while `sema.waitq`'s
                // lock (held by `guard`) or its own lock (held by `thread_guard` below, once
                // acquired, until `thread_guard` is consumed by `scheduler::unblock*`) is held.
                let thread: &Thread = unsafe { thread_ptr.as_ref() };
                let Some(mut thread_guard) = OwnerGuard::acquire_or_backoff(thread, token) else {
                    return Action::Backoff.into();
                };

                // If we have a queue_to_own, we will perform a few operations with the
                // following actors and objects:
                //
                //    P_c: Current thread
                //    P_w: Woken thread
                //    Q_o: Queue to own
                //    Q_w: Queue to wake from
                //
                //    Starting state:
                //    P_c  AND   P_w -> Q_w
                //
                //    Intermediate state:
                //    Q_w   AND   Q_o -> P_w   AND   P_c
                //
                //    Target state:
                //    Q_w  AND   P_c -> Q_o -> P_w
                //
                // We begin at the starting state, and with this function, transition to the
                // intermediate state. `assign_owner_locked` performs the "Q_o -> P_w" join, and
                // `dequeue_thread` performs the "P_w -> Q_w" split.
                //
                // To reach the target state, `OwnedWaitQueue::block_and_assign_owner_locked`
                // must perform the "P_c -> Q_o -> P_w" join. This occurs when in
                // `MessageWaiter::wait` (object/channel_dispatcher.rs).
                if let Some(queue_to_own) = queue_to_own {
                    // SAFETY: The active transaction has not been finalized, and
                    // `queue_to_own`'s lock is not held (the only locks held are `sema.waitq`'s
                    // and `thread`'s). `thread` is blocked, so it has been started, and its
                    // downstream PI chain consists solely of `sema.waitq` (a plain `WaitQueue`,
                    // which is always the target node of its PI graph), whose lock is held by
                    // this transaction. Since `sema.waitq` is a terminal node and is a different
                    // object from `queue_to_own` (an `OwnedWaitQueue`), the resulting chain
                    // `queue_to_own -> thread -> sema.waitq` cannot form a cycle.
                    if unsafe {
                        queue_to_own.assign_owner_locked(
                            &mut thread_guard.proof(),
                            token,
                            &preempt_disabled,
                        )
                    }
                    .is_err()
                    {
                        thread_guard.release();
                        return Action::Backoff.into();
                    }
                }

                // SAFETY: `thread` was obtained from `guard.proof().peek_front()`, so it is
                // currently blocked in `sema.waitq`; `sema.waitq`'s lock has been held
                // continuously since then, and `thread`'s lock is held by `thread_guard`.
                unsafe {
                    guard.proof().dequeue_thread(&mut thread_guard.proof(), Ok(()));
                }

                // If we are waking the last thread from the queue, set the negative count
                // back to zero.  We know that this is safe because we are still holding the
                // queue's lock.  Signaling threads cannot increment the count because it is
                // currently negative, and they cannot transition from negative to
                // non-negative without holding the queue's lock.  Likewise, Waiting threads
                // cannot decrement the count (again, because it is negative) without first
                // obtaining the queue's lock.  This holds true even if the thread we just
                // woke starts running on another CPU and attempts to alter the count
                // with either a post or a wait operation.
                if num_in_queue == 1 {
                    // Memory ordering: `Release`, matching the C++ implementation. The woken
                    // thread is synchronized via the queue and thread locks rather than `count`,
                    // and any later fast-path increment continues this store's release sequence.
                    sema.count.store(0, Ordering::Release);
                }

                guard.release();
                thread_guard
            };

            ChainLockTransaction::finalize(token);
            if queue_to_own.is_some() {
                // The new owner of the queue will unblock synchronously. This causes us to defer
                // rescheduling until we block in `MessageWaiter::wait`
                // (object/channel_dispatcher.rs).
                // SAFETY: `thread` was dequeued from `waitq` and the active transaction is
                // finalized.
                unsafe { scheduler::unblock_synchronous(thread_guard, token) };
            } else {
                // SAFETY: `thread` was dequeued from `waitq` and the active transaction is
                // finalized.
                unsafe { scheduler::unblock(thread_guard, token) };
            }
            DONE.into()
        };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("Semaphore::Post"),
            do_transaction,
        );
    }

    /// If the count is positive, decrement the count and return `Ok(())`. Otherwise, decrement
    /// the count and wait (uninterruptibly) until some other thread wakes us via
    /// [`Semaphore::post`].
    ///
    /// `wait_infinite` has acquire memory order semantics and synchronizes with `post`.
    ///
    /// # Caller requirements
    ///
    /// As with the other safe blocking primitives (e.g. [`Event::wait`] and [`thread::sleep`]),
    /// this must only be called from thread context (after threading has been initialized) where
    /// blocking is permitted: no spinlocks held, blocking not disallowed, and no
    /// [`ChainLockTransaction`] active on the current CPU (the last is asserted by
    /// [`ChainLockTransaction::until_done`] whenever `waitq`'s lock is needed).
    ///
    /// [`Event::wait`]: crate::kernel::event::Event::wait
    /// [`thread::sleep`]: crate::kernel::thread::sleep
    #[inline]
    pub fn wait_infinite(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Semaphore`, kept alive by the caller's borrow for the whole
        // call; the remaining requirements are this function's documented caller requirements
        // (`Deadline::infinite()` has no timeline).
        unsafe { Self::wait_worker(self, &const { Deadline::infinite() }, Interruptible::No) }
    }

    /// Just like [`Semaphore::wait_infinite`], but interruptible. In addition to `Ok(())`, the
    /// result can be `Err(Status::TIMED_OUT)` if the deadline had passed or one of the
    /// `Status::INTERNAL_INTR_*` errors if the thread had a signal delivered.
    ///
    /// `wait` has acquire memory order semantics and synchronizes with `post`.
    ///
    /// # Caller requirements
    ///
    /// Those of [`Semaphore::wait_infinite`]. In addition, a finite `deadline` must be on the
    /// monotonic timeline (a [`Deadline`] does not record its timeline).
    #[inline]
    pub fn wait(&self, deadline: &Deadline) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Semaphore`, kept alive by the caller's borrow for the whole
        // call; the remaining requirements are this function's documented caller requirements.
        unsafe { Self::wait_worker(self, deadline, Interruptible::Yes) }
    }

    /// Observe the current internal count of the semaphore.
    ///
    /// This should only be used for testing/diagnostic purposes.
    pub fn count(&self) -> i64 {
        // Memory ordering: `Relaxed`, since this is only a diagnostic snapshot.
        self.count.load(Ordering::Relaxed)
    }

    /// Observe the current internal count of waiters.
    ///
    /// This should only be used for testing/diagnostic purposes.
    pub fn num_waiters(&self) -> u64 {
        single_chain_lock_guard_owned!(
            scope,
            &self.waitq,
            IRQ_SAVE_OPTION,
            clt_tag!("Semaphore::num_waiters")
        );
        u64::from(scope.proof().count())
    }

    /// Common implementation of [`Semaphore::wait`] and [`Semaphore::wait_infinite`]: decrements
    /// the count, and blocks in `waitq` until `deadline` (with `interruptible`) if the count was
    /// not positive.
    ///
    /// Takes `this: *const Self` so that no `&Semaphore` or `&WaitQueue` is held across
    /// [`scheduler::block`], during which a posting thread may wake this thread and destroy the
    /// `Semaphore`.
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid `Semaphore` that remains alive at least until the count has
    ///   been decremented without blocking, or until this thread has been inserted into `waitq`
    ///   and `waitq`'s lock released.
    /// - The current thread must be running in thread context (after threading has been
    ///   initialized) where blocking is permitted (no spinlocks held and blocking not
    ///   disallowed).
    /// - If `deadline` is not infinite, it must use the monotonic timeline.
    /// - No [`ChainLockTransaction`] may be active on the current CPU (asserted by
    ///   [`ChainLockTransaction::until_done`] whenever `waitq`'s lock is needed).
    #[inline]
    unsafe fn wait_worker(
        this: *const Self,
        deadline: &Deadline,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        // Handling failed waits -- Waits can fail due to timeout or thread signal.
        // When a `wait` fails, the caller cannot proceed to the resource guarded by the
        // semaphore.  It's as if the waiter gave up and left.  We want to ensure failed
        // waits do not impact other waiters.  While it seems like a failed wait should
        // simply "undo" its decrement, it is not safe to do so in `wait`.  Instead, we
        // "fix" the count in subsequent call to `post`.
        //
        // To understand why we can't simply fix the count in `wait` after returning from
        // `WaitQueue::block`, let's look at an alternative (buggy) implementation of `post` and
        // `wait`.  In this hypothetical implementation, `post` increments and `wait` decrements
        // before `block`, but also increments if `block` returns an error.  With this
        // hypothetical implementation in mind, consider the following sequence of
        // operations:
        //
        //    Q  C
        //    0  0
        // 1W 1 -1  B
        // 1T 0 -1
        // 2P 0  0
        // 3W 1 -1  B
        // 1R 1  0
        //
        // The way to read the sequence above is that the wait queue (Q) starts empty
        // and the count (C) starts at zero.  Thread1 calls `wait` (1W) and blocks (B).
        // Thread1's times out (1T) and is removed from the queue, but has not yet
        // resumed execution.  Thread2 calls `post` (2P), but does not unblock any threads
        // because it finds an empty queue. Thread3 calls `wait` (3W) and blocks (B).
        // Thread1 returns from `block`, resumes execution (1R), and increments the count.
        // At this point Thread3 is blocked as it should be (two `post`s and one failed
        // `wait`), however, a subsequent call to `post` will not unblock it.  We have a
        // "lost wakeup".

        // Is the count greater than zero?  If so, decrement and return.  Take care to
        // not decrement zero or a negative value.
        //
        // Memory ordering: a successful decrement is `Acquire`, pairing with the `Release`
        // increments and stores in `post_worker`. The initial load and the CAS failure ordering
        // are `Relaxed` because the value read is only a hint, which the next CAS validates.
        {
            // SAFETY: The caller guarantees `this` points to a valid `Semaphore` that remains
            // alive at least until this thread either consumes the count or blocks.
            let count: &AtomicI64 = unsafe { &(*this).count };
            let mut old_count = count.load(Ordering::Relaxed);
            while old_count > 0 {
                match count.compare_exchange_weak(
                    old_count,
                    old_count - 1,
                    Ordering::Acquire,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return Ok(()),
                    Err(observed) => old_count = observed,
                }
            }
        }

        // SAFETY: Forwarded from this function's `# Safety`.
        unsafe { Self::wait_slow(this, deadline, interruptible) }
    }

    /// Lock-acquiring (and possibly blocking) slow path of [`Semaphore::wait_worker`], taken when
    /// the count was observed to be zero or negative.
    ///
    /// This is kept out of line (but not `#[cold]`, since it is the normal path whenever the
    /// caller needs to block) so that the lock-free fast path in `wait_worker` needs no stack
    /// frame or callee-saved registers.
    ///
    /// # Safety
    ///
    /// Those of [`Semaphore::wait_worker`].
    #[inline(never)]
    unsafe fn wait_slow(
        this: *const Self,
        deadline: &Deadline,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        // We either observed that count is zero or negative.  We have not
        // decremented yet.  We may or may not need to block, but we need to hold
        // our queue's lock before proceeding.
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("Semaphore::Wait"),
            |token: &ChainLockTransactionToken| -> TransactionResult<Result<(), Status>> {
                let (preamble_result, mut current_thread_guard) = {
                    // SAFETY: The caller guarantees `this` points to a valid `Semaphore` that
                    // stays live at least until `block_etc_preamble` below releases `waitq`'s
                    // lock (or until the count is consumed without blocking). Nothing reads it
                    // after this block.
                    let sema: &Self = unsafe { &*this };
                    let guard = OwnerGuard::new(&sema.waitq, token);

                    // Because we hold the lock we know that no other thread can transition the
                    // count from non-negative to negative or vice versa.  If we can decrement a
                    // positive count, then we're done and don't need to block.
                    //
                    // Memory ordering: `Acquire`, pairing with the `Release` updates in
                    // `post_worker`.
                    let old_count = sema.count.fetch_sub(1, Ordering::Acquire);
                    if old_count > 0 {
                        // Lost a race with a `post` between the fast path and taking the lock.
                        core::hint::cold_path();
                        guard.release();
                        return TransactionResult::Value(Ok(()));
                    }

                    // We either decremented zero or a negative value.  We must block.  Grab our
                    // thread's lock and do so.
                    //
                    // TODO(johngro): Is it safe to simply grab the thread's lock
                    // unconditionally here?  I cannot (currently) think of a way that we might
                    // need to encounter a scenario where we might need to legitimately back off
                    // (even if we encounter a Backoff error), but if one _could_ exist, we
                    // should be prepared to do so.
                    //
                    // Currently, this is the best argument I have for why this is safe.
                    // 1) Our queue is a normal WaitQueue, not an OwnedWaitQueue.  Because of
                    //    this, it must always be the target node of any PI graph it is involved
                    //    in.
                    // 2) The current thread is running (by definition), therefore it must also
                    //    be the target node of any PI graph it is involved in.
                    // 3) No one else can be involved in any operation which would add an edge
                    //    from the current thread to another wait queue (only threads can block
                    //    themselves in wait queues).
                    //
                    // We already have our queue's lock.  If there is another operation in
                    // flight which currently held the thread's lock, it would also need to
                    // involve holding our queue's lock in order to produce a scenario where the
                    // backoff protocol was needed.  By the above arguments, the only such
                    // operation would be blocking the current thread in this queue, which cannot
                    // happen because of #3 (we are the current thread).
                    //
                    // Will this reasoning always hold, however?  For example, what if (some day)
                    // there was an operation which wanted to globally hold all thread locks at
                    // the same time as all wait queue locks?  While this does not seem likely,
                    // I'm not sure how to effectively stop someone from accidentally making a
                    // mistake like this.
                    //
                    // Acquire the lock without backing off using an explicit retry loop because
                    // `ChainLock::acquire_first_in_chain` asserts that no chain locks are
                    // currently held to avoid misuse in the general case.
                    // SAFETY: `ThreadPtr::current` requires that threading has been initialized,
                    // which the caller guarantees (thread context). `as_ref` requires the thread
                    // to stay alive for the reference's lifetime, which holds since the current
                    // thread cannot exit while it is running this closure.
                    let current_thread: &Thread = unsafe { ThreadPtr::current().as_ref() };
                    let mut current_thread_guard = loop {
                        if let Some(current_thread_guard) = OwnerGuard::try_acquire_with_options(
                            current_thread,
                            token,
                            AllowFinalized::No,
                            RecordBackoff::No,
                        ) {
                            break current_thread_guard;
                        }
                        libarch::yield_processor();
                    };
                    ChainLockTransaction::finalize(token);
                    // The preamble/postamble split is used instead of `WaitQueue::block` because
                    // a posting thread may destroy this `Semaphore` before `scheduler::block`
                    // returns, so no `&Semaphore` or `&WaitQueue` may outlive the preamble.
                    //
                    // SAFETY: The preconditions of `WaitQueue::block_etc_preamble` are met:
                    // - `current_thread` is the currently running thread (so it is
                    //   `THREAD_RUNNING`), and it is not queued in any wait queue since only a
                    //   thread can block itself. Blocking is permitted per the caller's contract.
                    // - The active transaction was finalized above, with `sema.waitq`'s lock
                    //   (held by `guard`, which the preamble consumes and releases) and
                    //   `current_thread`'s lock (held by `current_thread_guard`) as the only held
                    //   chain locks.
                    // - A finite `deadline` is on the monotonic timeline per the caller's
                    //   contract.
                    // - `sema.waitq` remains valid while `current_thread` is blocked in it, since
                    //   the caller guarantees `this` stays live until the preamble releases
                    //   `waitq`'s lock, and `current_thread` cannot be dequeued (allowing the
                    //   `Semaphore` to be destroyed) until `current_thread`'s lock is released
                    //   inside `block_etc_postamble`.
                    // - The postamble is invoked below on `Ok`, before `current_thread`'s lock is
                    //   released.
                    let preamble_result = unsafe {
                        WaitQueue::block_etc_preamble(
                            guard,
                            &mut current_thread_guard.proof(),
                            deadline,
                            0,
                            ResourceOwnership::Normal,
                            interruptible,
                            token,
                        )
                    };
                    (preamble_result, current_thread_guard)
                };

                let result = preamble_result.and_then(|base_ptr| {
                    // SAFETY: `base_ptr` was returned by `block_etc_preamble` above, which
                    // released `waitq`'s lock; the finalized transaction holds only
                    // `current_thread`'s lock (proved by `current_thread_guard`); blocking is
                    // permitted and a finite `deadline` is monotonic per the caller's contract.
                    unsafe {
                        WaitQueueBase::block_etc_postamble(
                            base_ptr,
                            current_thread_guard.proof(),
                            deadline,
                            token,
                        )
                    }
                });
                current_thread_guard.release();
                TransactionResult::Value(result)
            },
        )
    }
}

/// C++ FFI entry point for `Semaphore::Post()`.
///
/// # Safety
///
/// - `semaphore` must point to a live `Semaphore` that remains allocated at least until the count
///   has been updated without waking a waiter, or until a waiter has been dequeued and `waitq`'s
///   lock released (a woken waiter may then destroy it, as with C++ `Semaphore::Post`).
/// - `queue_to_own` must be null or point to a live [`OwnedWaitQueue`] that remains allocated
///   until this call returns.
/// - If any spinlocks are held, preemption must have been disabled outside of the outermost one.
///
/// # Panics
///
/// Panics if a [`ChainLockTransaction`] is active on the current CPU and `waitq`'s lock is needed
/// (the C++ declaration is `TA_EXCL(chainlock_transaction_token)`).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/semaphore.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_semaphore_post(
    semaphore: *const Semaphore,
    queue_to_own: *mut OwnedWaitQueue,
) {
    debug_assert!(!semaphore.is_null());
    // SAFETY: Per this function's `# Safety`, `queue_to_own` is null or points to a live
    // `OwnedWaitQueue` which remains allocated until this call returns.
    let queue_to_own: Option<&OwnedWaitQueue> = unsafe { queue_to_own.as_ref() };
    // SAFETY: Forwarded from this function's `# Safety`. `semaphore` is passed as a raw pointer so
    // that no `&Semaphore` outlives `waitq`'s lock inside `post_worker`.
    unsafe { Semaphore::post_worker(semaphore, queue_to_own) };
}

/// C++ FFI entry point for `Semaphore::WaitInternal()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `semaphore` points to a live `Semaphore` which remains valid at least until the count has
///   been decremented without blocking, or until the current thread has been inserted into its
///   wait queue and the queue's lock released.
/// - The current thread is running in thread context (after threading has been initialized)
///   where blocking is permitted (no spinlocks held and blocking not disallowed).
/// - If `deadline` is not infinite, it uses the monotonic timeline.
///
/// # Panics
///
/// Panics if a [`ChainLockTransaction`] is active on the current CPU and `waitq`'s lock is needed
/// (the C++ declaration is `TA_EXCL(chainlock_transaction_token)`).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/semaphore.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_semaphore_wait_internal(
    semaphore: *const Semaphore,
    deadline: &Deadline,
    interruptible: Interruptible,
) -> zx_status_t {
    debug_assert!(!semaphore.is_null());
    // SAFETY: Forwarded from this function's `# Safety`. `semaphore` is passed as a raw pointer so
    // that no `&Semaphore` lives across `scheduler::block` inside `wait_worker`.
    Status::result_into_raw(unsafe { Semaphore::wait_worker(semaphore, deadline, interruptible) })
}

/// C++ FFI entry point for `Semaphore::count()`.
///
/// # Safety
///
/// `semaphore` must point to a live `Semaphore` which remains valid until this call returns.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/semaphore.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_semaphore_count(semaphore: *const Semaphore) -> i64 {
    debug_assert!(!semaphore.is_null());
    // SAFETY: Per this function's `# Safety`, `semaphore` points to a live `Semaphore` which
    // remains valid until this call returns.
    let semaphore: &Semaphore = unsafe { &*semaphore };
    semaphore.count()
}

/// C++ FFI entry point for `Semaphore::num_waiters()`.
///
/// # Safety
///
/// `semaphore` must point to a live `Semaphore` which remains valid until this call returns.
///
/// # Panics
///
/// Panics if a [`ChainLockTransaction`] is active on the current CPU.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/semaphore.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_semaphore_num_waiters(semaphore: *const Semaphore) -> u64 {
    debug_assert!(!semaphore.is_null());
    // SAFETY: Per this function's `# Safety`, `semaphore` points to a live `Semaphore` which
    // remains valid until this call returns.
    let semaphore: &Semaphore = unsafe { &*semaphore };
    // `num_waiters` is safe; it asserts that no transaction is active (`TA_EXCL` in C++).
    semaphore.num_waiters()
}

/// Semaphore unit tests for the Rust-native API (the C++ `semaphore` suite covers the C++ API).
#[cfg(ktest)]
#[unittest::suite(name = "semaphore_rust")]
mod tests {
    use crate::kernel::deadline::TimerSlack;
    use crate::kernel::thread::{self, DEFAULT_PRIORITY};
    use crate::platform_rs::timer::{DurationMono, InstantMono, zx_msec, zx_usec};
    use core::ffi::c_void;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_eq, assert_err, assert_ge, assert_ok, assert_true, expect_eq, expect_true, subtest,
        unwrap_ok,
    };

    /// Mirrors the C++ `semaphore::smoke_test` against Rust-initialized semaphores.
    #[test]
    fn smoke_test() {
        for initial_count in [0, 5, -2] {
            stack_pin_init!(let sema = Semaphore::new(initial_count));
            assert_eq!(initial_count, sema.count());
            assert_eq!(0u64, sema.num_waiters());
        }

        const POST_COUNT: i64 = 10;
        stack_pin_init!(let sema = Semaphore::new(0));

        for i in 0..POST_COUNT {
            assert_eq!(i, sema.count());
            assert_eq!(0u64, sema.num_waiters());

            sema.post(None);

            assert_eq!(i + 1, sema.count());
            assert_eq!(0u64, sema.num_waiters());
        }

        for i in 0..POST_COUNT {
            assert_eq!(POST_COUNT - i, sema.count());
            assert_eq!(0u64, sema.num_waiters());

            let result = sema.wait(&Deadline::infinite());
            assert_ok!(result);

            assert_eq!(POST_COUNT - i - 1, sema.count());
            assert_eq!(0u64, sema.num_waiters());
        }
    }

    /// Mirrors the C++ `semaphore::timeout_test` against a Rust-initialized semaphore.
    #[test]
    fn timeout_test() {
        let deadline = Deadline::after_mono(DurationMono(zx_usec(10)), TimerSlack::none());

        stack_pin_init!(let sema = Semaphore::new(0));
        assert_eq!(0, sema.count());
        assert_eq!(0u64, sema.num_waiters());
        assert_err!(sema.wait(&deadline), Status::TIMED_OUT);
        assert_ge!(0, sema.count());
        assert_eq!(0u64, sema.num_waiters());

        // The timed-out wait left a non-positive count with an empty queue. The next `post` must
        // reset the count to 1 (rather than only incrementing a negative count) since there is
        // no waiter to wake.
        sema.post(None);
        assert_eq!(1, sema.count());
        assert_eq!(0u64, sema.num_waiters());
    }

    /// A `wait_infinite` on a posted semaphore consumes the count without blocking.
    #[test]
    fn wait_infinite_no_block_test() {
        stack_pin_init!(let sema = Semaphore::new(0));
        sema.post(None);
        assert_eq!(1, sema.count());
        assert_ok!(sema.wait_infinite());
        assert_eq!(0, sema.count());
        assert_eq!(0u64, sema.num_waiters());
    }

    /// Returns the owner of `owq` as observed while holding its lock.
    fn observe_owner(owq: &OwnedWaitQueue) -> Option<ThreadPtr> {
        single_chain_lock_guard_owned!(
            scope,
            owq,
            IRQ_SAVE_OPTION,
            clt_tag!("semaphore_rust_test_observe_owner")
        );
        scope.proof().queue_owner()
    }

    /// A `post` with a `queue_to_own` but no waiter increments the count and assigns no owner.
    #[test]
    fn post_queue_no_waiter_test() {
        stack_pin_init!(let sema = Semaphore::new(0));
        stack_pin_init!(let queue = OwnedWaitQueue::init());
        sema.post(Some(&queue));
        assert_eq!(1, sema.count());
        assert_eq!(0u64, sema.num_waiters());
        assert_true!(observe_owner(&queue).is_none());
    }

    struct WaiterArgs<'a> {
        sema: &'a Semaphore,
        interruptible: Interruptible,
    }

    /// Creates and resumes a thread running `entry(arg)`.
    ///
    /// # Safety
    ///
    /// `arg` must remain valid for `entry` until the returned thread has been joined.
    unsafe fn spawn_waiter(
        entry: extern "C" fn(*mut c_void) -> i32,
        arg: *mut c_void,
    ) -> Result<ThreadPtr, Status> {
        // SAFETY: Forwarded from the caller.
        let waiter = unsafe {
            thread::create_with_priority(c"test semaphore".as_ptr(), entry, arg, DEFAULT_PRIORITY)
        }?;
        // SAFETY: `waiter` was just created and has not been resumed or joined yet.
        unsafe { waiter.resume() };
        Ok(waiter)
    }

    /// Each `post` wakes exactly one of several waiters; the count stays negative until the last.
    #[test]
    fn post_multiple_waiters_test() {
        stack_pin_init!(let sema = Semaphore::new(0));
        let args = WaiterArgs { sema: &sema, interruptible: Interruptible::No };
        let arg = core::ptr::from_ref(&args).cast_mut().cast();
        // SAFETY: `wait_sema_thread` only reads `args`, and both waiters are joined below before
        // `args` or `sema` go out of scope.
        let first = unwrap_ok!(unsafe { spawn_waiter(wait_sema_thread, arg) });
        // SAFETY: As above.
        let second = match unsafe { spawn_waiter(wait_sema_thread, arg) } {
            Ok(second) => second,
            Err(status) => {
                // Release and join `first` so that it never outlives `sema`.
                sema.post(None);
                // SAFETY: `first` has not been joined yet.
                let _ = unsafe { first.join(InstantMono::INFINITE) };
                assert_ok!(Err::<(), Status>(status));
                return false;
            }
        };
        let waiters = [first, second];
        while sema.num_waiters() != 2 {
            let _ = thread::sleep_relative(DurationMono(zx_msec(1)));
        }

        // Use non-fatal checks until the waiters have been woken and joined, so that they never
        // outlive `sema`.
        expect_eq!(-2, sema.count());

        // Waking a waiter which is not the last one leaves the (negative) count unchanged.
        sema.post(None);
        expect_eq!(-2, sema.count());
        expect_eq!(1u64, sema.num_waiters());

        sema.post(None);
        for waiter in waiters {
            // SAFETY: `waiter` has not been joined yet.
            let retcode = unwrap_ok!(unsafe { waiter.join(InstantMono::INFINITE) });
            expect_eq!(zx_types::ZX_OK, retcode);
        }
        assert_eq!(0, sema.count());
        assert_eq!(0u64, sema.num_waiters());
    }

    struct OwnerWaiterArgs<'a> {
        sema: &'a Semaphore,
        done: &'a Semaphore,
    }

    extern "C" fn wait_then_park_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `OwnerWaiterArgs` that outlives this thread, and is
        // only read.
        let args = unsafe { &*arg.cast::<OwnerWaiterArgs<'_>>() };
        let result = args.sema.wait_infinite();
        // Stay alive (and so keep ownership of any queue assigned by `post`) until released.
        let _ = args.done.wait_infinite();
        Status::result_into_raw(result)
    }

    /// A `post` with a `queue_to_own` and a waiter makes the woken thread the queue's owner.
    #[test]
    fn post_queue_assigns_owner_test() {
        stack_pin_init!(let sema = Semaphore::new(0));
        stack_pin_init!(let done = Semaphore::new(0));
        stack_pin_init!(let queue = OwnedWaitQueue::init());
        let args = OwnerWaiterArgs { sema: &sema, done: &done };
        // SAFETY: `wait_then_park_thread` only reads `args`, and `waiter` is joined below before
        // `args`, `sema` or `done` go out of scope.
        let waiter = unwrap_ok!(unsafe {
            spawn_waiter(wait_then_park_thread, core::ptr::from_ref(&args).cast_mut().cast())
        });
        while sema.num_waiters() != 1 {
            let _ = thread::sleep_relative(DurationMono(zx_msec(1)));
        }

        // The owner is assigned under the queue locks inside `post`, and the waiter cannot exit
        // (which would disown the queue) until `done` is posted, so this check is not racy. Use
        // non-fatal checks until the waiter has been joined.
        sema.post(Some(&queue));
        expect_true!(observe_owner(&queue) == Some(waiter));
        queue.reset_owner_if_no_waiters();
        expect_true!(observe_owner(&queue).is_none());

        done.post(None);
        // SAFETY: `waiter` has not been joined yet.
        let retcode = unwrap_ok!(unsafe { waiter.join(InstantMono::INFINITE) });
        assert_eq!(zx_types::ZX_OK, retcode);
        assert_eq!(0, sema.count());
        assert_eq!(0, done.count());
    }

    extern "C" fn wait_sema_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `WaiterArgs` that outlives this thread, and is only read.
        let args = unsafe { &*arg.cast::<WaiterArgs<'_>>() };
        let result = match args.interruptible {
            Interruptible::Yes => args.sema.wait(&Deadline::infinite()),
            Interruptible::No => args.sema.wait_infinite(),
        };
        Status::result_into_raw(result)
    }

    #[derive(Clone, Copy)]
    enum Signal {
        Post,
        PostQueue,
        Kill,
        Suspend,
    }

    /// Mirrors the C++ `semaphore::signal_test`: tests waking or signaling a thread blocked in
    /// `wait` (`Interruptible::Yes`) or `wait_infinite` (`Interruptible::No`).
    fn run_signal_test(signal: Signal, interruptible: Interruptible) -> bool {
        (subtest!(|signal: Signal, interruptible: Interruptible| {
            stack_pin_init!(let sema = Semaphore::new(0));
            assert_eq!(0, sema.count());
            assert_eq!(0u64, sema.num_waiters());

            let args = WaiterArgs { sema: &sema, interruptible };
            // SAFETY: `wait_sema_thread` only reads `args`, and `waiter` is joined below before
            // `args` or `sema` go out of scope.
            let waiter = unwrap_ok!(unsafe {
                thread::create_with_priority(
                    c"test semaphore".as_ptr(),
                    wait_sema_thread,
                    core::ptr::from_ref(&args).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            });
            // SAFETY: `waiter` was just created and has not been resumed or joined yet.
            unsafe { waiter.resume() };

            // SAFETY: `waiter` has not been joined yet.
            while !unsafe { waiter.is_blocked() } {
                let _ = thread::sleep_relative(DurationMono(zx_msec(1)));
            }

            // Use non-fatal checks until the waiter has been woken and joined, so that it never
            // outlives `sema`.
            expect_eq!(-1, sema.count());
            expect_eq!(1u64, sema.num_waiters());

            let (expected_error, expected_count) = match signal {
                Signal::Post => {
                    sema.post(None);
                    (zx_types::ZX_OK, 0)
                }
                Signal::PostQueue => {
                    stack_pin_init!(let queue = OwnedWaitQueue::init());
                    sema.post(Some(&queue));
                    // `queue` now has the waiter as its owner, which must be released before
                    // `queue` is dropped (an `OwnedWaitQueue` must not be destroyed while owned).
                    queue.reset_owner_if_no_waiters();
                    (zx_types::ZX_OK, 0)
                }
                Signal::Kill => {
                    // SAFETY: `waiter` has not been joined yet.
                    unsafe { waiter.kill() };
                    if interruptible == Interruptible::Yes {
                        (zx_types::ZX_ERR_INTERNAL_INTR_KILLED, -1)
                    } else {
                        sema.post(None);
                        (zx_types::ZX_OK, 0)
                    }
                }
                Signal::Suspend => {
                    // SAFETY: `waiter` has not been joined yet.
                    let _ = unsafe { waiter.suspend() };
                    if interruptible == Interruptible::Yes {
                        (zx_types::ZX_ERR_INTERRUPTED_RETRY, -1)
                    } else {
                        sema.post(None);
                        (zx_types::ZX_OK, 0)
                    }
                }
            };

            // SAFETY: `waiter` has not been joined yet.
            let retcode = unwrap_ok!(unsafe { waiter.join(InstantMono::INFINITE) });
            assert_eq!(expected_error, retcode);
            assert_eq!(expected_count, sema.count());
            assert_eq!(0u64, sema.num_waiters());

            if expected_count < 0 {
                // The interrupted waiter left a negative count with an empty queue, which the
                // next `post` repairs.
                sema.post(None);
                assert_eq!(1, sema.count());
                assert_eq!(0u64, sema.num_waiters());
            }
        }))(signal, interruptible)
    }

    /// A thread blocked in `wait` is woken by `post`.
    #[test]
    fn post_signal_test() {
        assert_true!(run_signal_test(Signal::Post, Interruptible::Yes));
    }

    /// A thread blocked in `wait` is woken by `post` with a `queue_to_own`.
    #[test]
    fn post_queue_signal_test() {
        assert_true!(run_signal_test(Signal::PostQueue, Interruptible::Yes));
    }

    /// A thread blocked in `wait` is woken by `kill` with `ZX_ERR_INTERNAL_INTR_KILLED`.
    #[test]
    fn kill_signal_test() {
        assert_true!(run_signal_test(Signal::Kill, Interruptible::Yes));
    }

    /// A thread blocked in `wait` is woken by `suspend` with `ZX_ERR_INTERRUPTED_RETRY`.
    #[test]
    fn suspend_signal_test() {
        assert_true!(run_signal_test(Signal::Suspend, Interruptible::Yes));
    }

    /// A thread blocked in `wait_infinite` is woken by `post`.
    #[test]
    fn post_uninterruptible_signal_test() {
        assert_true!(run_signal_test(Signal::Post, Interruptible::No));
    }

    /// A thread blocked in `wait_infinite` is woken by `post` with a `queue_to_own`.
    #[test]
    fn post_queue_uninterruptible_signal_test() {
        assert_true!(run_signal_test(Signal::PostQueue, Interruptible::No));
    }

    /// A thread blocked in `wait_infinite` ignores `kill` and subsequently unblocks on `post`.
    #[test]
    fn kill_uninterruptible_signal_test() {
        assert_true!(run_signal_test(Signal::Kill, Interruptible::No));
    }

    /// A thread blocked in `wait_infinite` ignores `suspend` and subsequently unblocks on `post`.
    #[test]
    fn suspend_uninterruptible_signal_test() {
        assert_true!(run_signal_test(Signal::Suspend, Interruptible::No));
    }
}
