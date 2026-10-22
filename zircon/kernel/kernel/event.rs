// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Event wait and signal primitives for threads.
//!
//! An event is built on a wait queue. Threads wait for events, with optional timeouts.
//!
//! Events are "signaled", releasing waiting threads to continue. Signals may be one-shot signals
//! ([`AutounsignalEvent`]), in which case one signal releases only one thread, at which point it
//! is automatically cleared. Otherwise ([`Event`]), signals release all waiting threads to
//! continue immediately until the signal is manually cleared with [`Event::unsignal`].
//!
//! Rules for Events and AutounsignalEvents:
//! - Events may be signaled from interrupt context *but* preemption must be
//!   disabled.
//! - Events may not be waited upon from interrupt context.
//! - Standard Events:
//!   - Wake up any waiting threads when signaled.
//!   - Continue to do so (no threads will wait) until unsignaled.
//!   - Stores a single result value when first signaled. This result is
//!     returned to waiters and cleared when unsignaled.
//! - AutounsignalEvents:
//!   - If one or more threads are waiting when signaled, one thread will
//!     be woken up and return.  The signaled state will not be set.
//!   - If no threads are waiting when signaled, the AutounsignalEvent will remain
//!     in the signaled state until a thread attempts to wait (at which
//!     time it will unsignal atomicly and return immediately) or
//!     [`Event::unsignal`] is called.
//!   - Stores a single result value when signaled until a thread is woken.

use core::cell::UnsafeCell;
use core::mem::{align_of, offset_of, size_of};
use core::ops::Deref;
use core::sync::atomic::{AtomicI32, Ordering};
use event_bindings as bindings;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zr::static_assert;
use zx_status::Status;
use zx_types::{ZX_OK, zx_status_t};

use crate::arch_rs::blocking_disallowed;
use crate::kconcurrent::{
    ChainLockProof, ChainLockTransaction, ChainLockTransactionToken, ChainLocked,
    EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION, IRQ_SAVE_OPTION, OwnerGuard, TransactionResult,
    assert_no_transaction_active, clt_tag,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::{OwnedWaitQueue, ThreadUnblockList};
use crate::kernel::scheduler;
use crate::kernel::thread::{self, Interruptible, PreemptDisabledToken, Thread};
use crate::kernel::wait_queue::{ResourceOwnership, WaitQueue, WaitQueueBase, WaitQueueLockOps};
use crate::platform_rs::timer::{InstantMono, InstantUnknown};

/// Magic value for a live [`Event`].
const MAGIC: u32 = fbl::magic(b"evnt");
static_assert!(MAGIC == bindings::EventLayout_kMagic);

/// The `result` value of an unsignaled [`Event`].
const NOT_SIGNALED: zx_status_t = i32::MAX;
static_assert!(NOT_SIGNALED == bindings::EventLayout_kNotSignaled);

/// Standard event with no auto-unsignal behavior.
const FLAGS_NONE: u32 = 0;
/// Event flag that unsignals automatically after releasing one waiting thread.
const FLAGS_AUTOUNSIGNAL: u32 = 1;
static_assert!(FLAGS_NONE == bindings::EventLayout_kFlagsNone);
static_assert!(FLAGS_AUTOUNSIGNAL == bindings::EventLayout_kFlagsAutounsignal);

/// A synchronization event that allows threads to wait and signal.
///
/// An `Event` can be signaled, waking all waiting threads, and remains signaled
/// until explicitly reset via [`Event::unsignal`].
///
/// Same layout as the C++ `Event` (asserted against `EventLayout` below); C++ constructs and
/// destroys instances too, so both sides initialize the same bytes identically.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct Event {
    // Safety invariant: `MAGIC` while live, 0 after drop. Wrapped in `UnsafeCell` so tearing down
    // via `&self` does not conflict with concurrent shared borrows and `Event` has no `Freeze`
    // bytes.
    magic: UnsafeCell<u32>,
    // A `zx_status_t`; `NOT_SIGNALED` while unsignaled. Relaxed loads/stores: the unsignaled ->
    // signaled transition happens only under `wait.lock()`.
    result: AtomicI32,
    // `FLAGS_NONE` or `FLAGS_AUTOUNSIGNAL`; written only at init (and zeroed on drop).
    flags: UnsafeCell<u32>,
    #[pin]
    wait: WaitQueue,
}

// Compile-time layout assertions against the C++ Event type via bindgen of
// <kernel/event_layout.h>.
static_assert!(size_of::<Event>() == bindings::EventLayout_kSize);
static_assert!(align_of::<Event>() == bindings::EventLayout_kAlign);
static_assert!(offset_of!(Event, magic) == bindings::EventLayout_kMagicOffset);
static_assert!(offset_of!(Event, result) == bindings::EventLayout_kResultOffset);
static_assert!(offset_of!(Event, flags) == bindings::EventLayout_kFlagsOffset);
static_assert!(offset_of!(Event, wait) == bindings::EventLayout_kWaitOffset);

// SAFETY: `Event` has no thread-local ownership invariants and is `!Unpin`, so ownership of a
// pinned `Event` can be transferred across threads.
unsafe impl Send for Event {}
// SAFETY: All interior mutations through shared references are either on the `result` atomic,
// synchronized by the embedded `WaitQueue`'s `ChainLock`, or (for `magic` and `flags`) confined to
// `destroy_fields` during teardown, when no other reference to the `Event` exists.
unsafe impl Sync for Event {}

#[pinned_drop]
impl PinnedDrop for Event {
    /// Destruct an Event object.
    ///
    /// Event's resources are freed and it may no longer be used.
    /// Will panic if there are any threads still waiting (the embedded `WaitQueue`'s drop checks).
    fn drop(self: core::pin::Pin<&mut Self>) {
        // SAFETY: `self` is being dropped once, so no other thread can concurrently read or write
        // `magic` or `flags`.
        unsafe { self.as_ref().get_ref().destroy_fields() };
    }
}

impl Event {
    /// The `Event`-specific half of `~Event`; the embedded `WaitQueue` is destroyed separately by
    /// whichever language owns the instance.
    ///
    /// # Safety
    ///
    /// Must be called at most once when the `Event` is being destroyed and no other thread can
    /// concurrently read or write `magic` or `flags`.
    unsafe fn destroy_fields(&self) {
        // SAFETY: The caller guarantees exclusive teardown access to `magic` and `flags`.
        debug_assert!(unsafe { *self.magic.get() } == MAGIC);

        // SAFETY: The caller guarantees exclusive teardown access to `magic` and `flags`.
        unsafe {
            *self.magic.get() = 0;
            self.result.store(NOT_SIGNALED, Ordering::Relaxed);
            *self.flags.get() = FLAGS_NONE;
        }
    }

    /// Domain-specific conversion: returns raw pointer for `Event`.
    pub fn as_raw(&self) -> *mut bindings::Event {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// Domain-specific conversion: constructs a `&Event` from a raw bindings pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null, properly aligned pointer to a live, initialized `Event`
    /// that remains valid for the duration of `'a`, and no mutable references to it may exist for
    /// the duration of `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::Event) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::Event` is layout-compatible with `Event`, and the caller guarantees
        // `ptr` is valid, aligned, and unaliased by mutable references for `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Returns a raw `Event` pointer from an underlying bindings pointer.
    ///
    /// Provides additional type safety when used instead of a `.cast()`.
    pub fn ptr_from_raw(raw: *mut bindings::Event) -> *mut Event {
        raw.cast()
    }

    /// Returns a pointer to the underlying `Event` structure.
    ///
    /// This method is helpful when you don't have a reference to the `Event`. If you do, then
    /// use `Event::as_raw` instead.
    pub fn cast_raw(ptr: *mut Event) -> *mut bindings::Event {
        ptr.cast()
    }

    /// In-place pinned initializer with the given `flags`.
    fn init_with_flags(initial: bool, flags: u32) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            magic: UnsafeCell::new(MAGIC),
            result: AtomicI32::new(if initial { ZX_OK } else { NOT_SIGNALED }),
            flags: UnsafeCell::new(flags),
            wait <- WaitQueue::init(),
        })
    }

    /// Creates an in-place initializer for `Event`.
    pub fn init(initial: bool) -> impl PinInit<Self, core::convert::Infallible> {
        Self::init_with_flags(initial, FLAGS_NONE)
    }

    /// Creates an in-place initializer for an initially unsignaled `Event`.
    pub fn init_unsignaled() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init(false)
    }

    /// Creates an in-place initializer for an initially signaled `Event`.
    pub fn init_signaled() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init(true)
    }

    /// Waits for the event to be signaled, with a deadline.
    ///
    /// [`Event::wait`] and other wait functions will return `Ok(())` if already signaled, even if
    /// the deadline has passed. They will return `Err(Status::TIMED_OUT)` after the deadline
    /// passes if the event has not been signaled.
    ///
    /// May also return `Err(Status::INTERNAL_INTR_KILLED)` or `Err(Status::INTERNAL_INTR_RETRY)`
    /// if the waiting thread is killed or suspended.
    pub fn wait(&self, deadline: &Deadline) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Event`, kept alive at least until `wait` returns;
        // `wait_worker` asserts no transaction is active and (in debug) that blocking is allowed.
        unsafe { Self::wait_worker(self, deadline, Interruptible::Yes, 0) }
    }

    /// Same as [`Event::wait`] but gives a mask of signals to ignore. The `signal_mask` only
    /// applies to existing signals, not future ones that might be signaled while waiting. The
    /// caller must be interruptible.
    pub fn wait_mask(&self, deadline: &Deadline, signal_mask: u32) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Event`, kept alive at least until `wait_mask` returns;
        // `wait_worker` asserts no transaction is active and (in debug) that blocking is allowed.
        unsafe { Self::wait_worker(self, deadline, Interruptible::Yes, signal_mask) }
    }

    /// Waits for the event indefinitely without interruption.
    pub fn wait_infinite(&self) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Event`, kept alive at least until `wait_infinite` returns;
        // `wait_worker` asserts no transaction is active and (in debug) that blocking is allowed.
        unsafe { Self::wait_worker(self, &const { Deadline::infinite() }, Interruptible::No, 0) }
    }

    /// Wait until a InstantMono deadline.
    /// Interruptible arg allows it to return early with ZX_ERR_INTERNAL_INTR_KILLED if thread
    /// is signaled for kill or with ZX_ERR_INTERNAL_INTR_RETRY if the thread is suspended.
    pub fn wait_deadline(
        &self,
        deadline: InstantMono,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        // SAFETY: `self` is a valid `Event`, kept alive at least until `wait_deadline` returns;
        // `wait_worker` asserts no transaction is active and (in debug) that blocking is allowed.
        unsafe {
            Self::wait_worker(
                self,
                &Deadline::no_slack(InstantUnknown(deadline.0)),
                interruptible,
                0,
            )
        }
    }

    /// Internal wait implementation shared by all `wait*` entry points.
    ///
    /// # Safety
    /// - `this` must point to a valid, initialized `Event` that remains alive at least until the
    ///   thread either observes the event signaled or is inserted into `wait` and
    ///   `block_etc_preamble` releases the queue lock.
    /// - If `deadline` is not infinite it must use the monotonic timeline.
    /// - The caller must be in a blockable thread context after threading initialization
    ///   (`!blocking_disallowed()`).
    /// - No `ChainLockTransaction` may be active on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token)`).
    unsafe fn wait_worker(
        this: *const Self,
        deadline: &Deadline,
        interruptible: Interruptible,
        signal_mask: u32,
    ) -> Result<(), Status> {
        // SAFETY: `this` points to a valid, initialized `Event` (caller's contract); `magic` is
        // immutable while the `Event` is live.
        debug_assert!(unsafe { *(*this).magic.get() } == MAGIC);
        debug_assert!(!blocking_disallowed());
        assert_no_transaction_active();

        // Start by grabbing our wait queue's lock.  The state of the event is only
        // allowed to change from un-signaled to signaled when we are holding this
        // lock, so by holding it here, we can check the state of the signal and fast
        // abort if we need to, or descend into the wait queue and be certain to fully
        // block in the queue before releasing the lock.
        let do_transaction =
            |token: &ChainLockTransactionToken| -> TransactionResult<Result<(), Status>> {
                let (preamble_res, mut t_guard) = {
                    // SAFETY: The caller guarantees `this` points to a valid `Event`.
                    let event: &Self = unsafe { &*this };
                    let wq_guard = OwnerGuard::new(&event.wait, token);

                    let result = event.result.load(Ordering::Relaxed);
                    if result == NOT_SIGNALED {
                        // Looks like we are not currently signaled.  Now try to obtain the
                        // current thread's lock so we can block it.
                        //
                        // SAFETY: Waiting happens in thread context, after threading is
                        // initialized.
                        let current_thread: &Thread = unsafe { &*thread::current_get() };
                        let Some(mut guard) = OwnerGuard::acquire_or_backoff(current_thread, token)
                        else {
                            wq_guard.release();
                            return TransactionResult::BACKOFF;
                        };

                        ChainLockTransaction::finalize(token);

                        // We got the lock, go ahead and block the thread.  This will
                        // automatically release the queue's lock after the thread has been added
                        // to the queue and is committed to blocking.  We will need release the
                        // thread's lock ourselves after it wakes up, as it will be obtained as it
                        // becomes scheduled.
                        //
                        // The preamble/postamble split is used instead of `WaitQueue::block_etc`
                        // because the waker may destroy this `Event` before `scheduler::block`
                        // returns, so no `&Event` or `&WaitQueue` may outlive the preamble.
                        //
                        // SAFETY: `current_thread` is the running thread and is in no wait queue
                        // (`blocking_disallowed()` is false, so we are in blockable thread
                        // context); the transaction is finalized with the queue's and the
                        // thread's locks as the only chain locks held; `deadline` is the caller's
                        // (`wait`/`wait_mask`/FFI) or built from an `InstantMono`
                        // (`wait_deadline`/`wait_infinite`), and `block_etc_preamble`'s timeline
                        // requirement is the caller's; `event.wait` is kept alive by `this` until
                        // `block_etc_preamble` releases the queue lock (nothing reads `event`
                        // after this block).
                        let res = unsafe {
                            WaitQueue::block_etc_preamble(
                                wq_guard,
                                &mut guard.proof(),
                                deadline,
                                signal_mask,
                                ResourceOwnership::Normal,
                                interruptible,
                                token,
                            )
                        };
                        (res, guard)
                    } else {
                        /* signaled, we're going to fall through */
                        // SAFETY: `flags` is immutable while the `Event` is live.
                        let flags = unsafe { *event.flags.get() };
                        if (flags & FLAGS_AUTOUNSIGNAL) != 0 {
                            /* autounsignal flag lets one thread fall through before unsignaling */
                            event.result.store(NOT_SIGNALED, Ordering::Relaxed);
                        }

                        wq_guard.release();
                        return TransactionResult::Value(Status::ok(result));
                    }
                };

                let block_result = preamble_res.and_then(|base_ptr| {
                    // SAFETY: `base_ptr` is the wait queue returned by `block_etc_preamble` above,
                    // which keeps it alive, and `t_guard` is the proof it requires.
                    unsafe {
                        WaitQueueBase::block_etc_postamble(
                            base_ptr,
                            t_guard.proof(),
                            deadline,
                            token,
                        )
                    }
                });

                t_guard.release();
                TransactionResult::Value(block_result)
            };

        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("Event::WaitWorker"),
            do_transaction,
        )
    }

    /// Signals the event with `ZX_OK` and wakes all waiting threads.
    pub fn signal(&self) {
        self.signal_result(Ok(()));
    }

    /// Signals the event with a specific status result.
    pub fn signal_result(&self, wait_result: impl Into<Result<(), Status>>) {
        self.signal_etc(wait_result, None);
    }

    /// Signals an event.
    ///
    /// If `FLAGS_AUTOUNSIGNAL` is set in the event object's flags, only one waiting thread is
    /// allowed to proceed. Otherwise, all waiting threads are allowed to proceed until such time
    /// as [`Event::unsignal`] is called.
    ///
    /// `wait_result` is what a wait call will return to the thread or threads that are woken up.
    /// `queue_to_own` is the `OwnedWaitQueue` that the first unblocked thread will take ownership
    /// of. If this is `Some`, then we will perform an assign-owner operation on the queue. If
    /// there is no thread waiting, then no assign-owner operation will occur.
    #[inline]
    pub fn signal_etc(
        &self,
        wait_result: impl Into<Result<(), Status>>,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        // SAFETY: `self` is a valid `Event`, kept alive by the caller's borrow for the whole call;
        // `signal_worker` itself drops its `&Event` before any waiter can be released.
        unsafe { Self::signal_worker(self, wait_result.into(), queue_to_own) }
    }

    /// Non-generic implementation of [`Self::signal_etc`], taking `this: *const Self` so no
    /// `&Event` is held after `wait.lock()` is released (allowing a woken waiter to immediately
    /// destroy a stack-allocated `Event`).
    ///
    /// # Safety
    ///
    /// - `this` must point to a valid, initialized `Event` that remains alive at least until the
    ///   event is observed already-signaled, marked signaled with no waiters, or all waiters are
    ///   dequeued and `guard.release()` releases `wait.lock()`.
    /// - No `ChainLockTransaction` may be active on the current CPU.
    pub(crate) unsafe fn signal_worker(
        this: *const Self,
        wait_result: Result<(), Status>,
        queue_to_own: Option<&OwnedWaitQueue>,
    ) {
        // SAFETY: `this` points to a valid, initialized `Event` (caller's contract); `magic` is
        // immutable while the `Event` is live.
        debug_assert!(unsafe { *(*this).magic.get() } == MAGIC);
        assert_no_transaction_active();
        debug_assert!(Status::result_into_raw(wait_result) != NOT_SIGNALED);

        // In order to transition from not-signaled to signaled, we must be
        // holding our wait queue's lock.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult {
            // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables eager rescheduling,
            // which counts as preemption being disabled for the duration of this transaction.
            let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
            pin_init::stack_pin_init!(let unblock_list = ThreadUnblockList::init());
            let has_threads_to_wake = {
                // SAFETY: The caller guarantees `this` points to a valid `Event` that stays live
                // at least until `guard.release()` at the end of this block.
                let event: &Self = unsafe { &*this };
                let mut guard = OwnerGuard::new(&event.wait, token);

                // If we are already signaled, we are finished.  We should be able to assert
                // that there are no waiters right now.
                if event.result.load(Ordering::Relaxed) != NOT_SIGNALED {
                    debug_assert!(guard.proof().count() == 0);
                    return TransactionResult::DONE;
                }

                // If there are no threads waiting in the event, we can just mark it
                // signaled and get out.
                if guard.proof().count() == 0 {
                    event.result.store(Status::result_into_raw(wait_result), Ordering::Relaxed);
                    return TransactionResult::DONE;
                }

                // SAFETY: `flags` is immutable while the `Event` is live.
                let flags = unsafe { *event.flags.get() };

                // Try to lock with one or all of the threads for wake.
                //
                // SAFETY: `unblock_list` is empty, and every thread lock acquired on `true` is
                // released below (either on `assign_owner_locked` backoff or via
                // `unblock_synchronous` / `unblock_list`).
                let locked = unsafe {
                    if (flags & FLAGS_AUTOUNSIGNAL) != 0 {
                        WaitQueueLockOps::lock_for_wake_one_in_place(
                            &mut guard.proof(),
                            unblock_list.as_mut(),
                            token,
                        )
                    } else {
                        WaitQueueLockOps::lock_for_wake_all_in_place(
                            &mut guard.proof(),
                            unblock_list.as_mut(),
                            token,
                        )
                    }
                };

                // If we failed to lock, we need to drop the queue lock, then try again.
                if !locked {
                    return TransactionResult::BACKOFF;
                }

                // Success.  If we not an auto-reset event, or we failed to find anyone to
                // wake, make sure to set the event to the signaled state.
                let has_threads_to_wake = !unblock_list.is_empty();
                if (flags & FLAGS_AUTOUNSIGNAL) == 0 || !has_threads_to_wake {
                    event.result.store(Status::result_into_raw(wait_result), Ordering::Relaxed);
                }

                if has_threads_to_wake {
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
                    // intermediate state. AssignOwnerLocked performs the "Q_o -> P_w" join, and
                    // DequeueThread performs the "P_w -> Q_w" split.
                    //
                    // To reach the target state, BlockAndAssignOwnerLocked must perform the
                    // "P_c -> Q_o -> P_w" join. This occurs when in
                    // ChannelDispatcher::MessageWaiter::Wait.
                    if let Some(queue_to_own) = queue_to_own {
                        // We grab the first thread from unblock_list, and assert that its lock is
                        // held, to convince static analysis.
                        let front =
                            unblock_list.as_list_ref().front().expect("non-empty list has a front");
                        debug_assert!(front.lock().is_held(token));
                        // SAFETY: `front`'s lock was acquired by `lock_for_wake_*_in_place` in
                        // this transaction (checked above in debug builds), and no proof for it is
                        // live.
                        let mut front_proof =
                            unsafe { ChainLockProof::assume_held_unchecked(front) };
                        // SAFETY: `assert_no_transaction_active()` above ensures `queue_to_own`'s
                        // lock was not held before this transaction; the transaction is not
                        // finalized; and `front` is a started thread blocked in `event.wait` (a
                        // plain `WaitQueue` with no owner, so `front`'s PI chain ends at
                        // `event.wait` and `queue_to_own` cannot be downstream of it).
                        let assigned = unsafe {
                            queue_to_own.assign_owner_locked(
                                &mut front_proof,
                                token,
                                &preempt_disabled_token,
                            )
                        };
                        drop(front_proof);
                        if assigned.is_err() {
                            while let Some(t) = unblock_list.as_mut().as_list().pop_front() {
                                // SAFETY: `t` is blocked in `event.wait` (so live); its lock was
                                // acquired by `lock_for_wake_*_in_place` in this transaction with
                                // no guard or proof left, and is released exactly once here.
                                unsafe { t.as_ref().lock().as_chain_lock() }.release(token);
                            }
                            return TransactionResult::BACKOFF;
                        }
                    }

                    for t in unblock_list.as_list_ref().iter() {
                        debug_assert!(t.lock().is_held(token));
                        // SAFETY: `t`'s lock was acquired by `lock_for_wake_*_in_place` in this
                        // transaction (checked above in debug builds) and no proof for it is live.
                        let mut t_proof = unsafe { ChainLockProof::assume_held_unchecked(t) };
                        // SAFETY: `t` is blocked in `event.wait` (it came from its collection).
                        unsafe { guard.proof().dequeue_thread(&mut t_proof, wait_result) };
                    }
                    guard.release();
                }
                has_threads_to_wake
            };

            if has_threads_to_wake {
                ChainLockTransaction::finalize(token);

                if queue_to_own.is_some() {
                    let t = unblock_list
                        .as_mut()
                        .as_list()
                        .pop_front()
                        .expect("non-empty list has a front");
                    // SAFETY: `t` was just dequeued with its lock held (so it is live).
                    let t: &Thread = unsafe { t.as_ref() };
                    // SAFETY: `t`'s lock is held by this transaction (checked by `adopt`) with no
                    // guard or proof for it; `unblock_synchronous` releases it.
                    let t_guard = unsafe { OwnerGuard::adopt(t, token) };
                    // The new owner of the queue will unblock synchronously. This causes us to
                    // defer rescheduling until we block in ChannelDispatcher::MessageWaiter::Wait.
                    //
                    // SAFETY: `t` was dequeued above with its `blocked_status` set.
                    unsafe { scheduler::unblock_synchronous(t_guard, token) };
                }

                // We have all of our locks now, time to proceed with the wake operations.
                if !unblock_list.is_empty() {
                    // SAFETY: Every remaining thread's lock was acquired by
                    // `lock_for_wake_*_in_place` in this transaction with no guard or proof left,
                    // and each was dequeued above with its `blocked_status` set.
                    unsafe { scheduler::unblock_list(unblock_list, token) };
                }
            }
            TransactionResult::DONE
        };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("Event::Signal"),
            do_transaction,
        );
    }

    /// Clear the "signaled" property of an event.
    ///
    /// Used mainly for event objects without the `FLAGS_AUTOUNSIGNAL` flag. Once this function is
    /// called, threads that call `wait*` functions will once again need to wait until the event
    /// object is signaled.
    ///
    /// Returns `Ok(())` on success.
    pub fn unsignal(&self) -> Result<(), Status> {
        // SAFETY: `magic` is immutable while the `Event` is live.
        debug_assert!(unsafe { *self.magic.get() } == MAGIC);
        self.result.store(NOT_SIGNALED, Ordering::Relaxed);
        Ok(())
    }

    /// Returns `true` if the event is currently signaled.
    pub fn is_signaled(&self) -> bool {
        self.result.load(Ordering::Relaxed) != NOT_SIGNALED
    }
}

/// An autounsignal event where signaling releases at most one waiting thread before
/// atomically resetting to unsignaled.
///
/// Same layout as the C++ `AutounsignalEvent`, which adds nothing to `Event`.
#[pin_data]
#[repr(transparent)]
pub struct AutounsignalEvent {
    #[pin]
    event: Event,
}

static_assert!(size_of::<AutounsignalEvent>() == bindings::EventLayout_kAutounsignalSize);
static_assert!(align_of::<AutounsignalEvent>() == bindings::EventLayout_kAutounsignalAlign);

impl AutounsignalEvent {
    /// Domain-specific conversion: returns raw pointer for `AutounsignalEvent`.
    pub fn as_raw(&self) -> *mut bindings::AutounsignalEvent {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// Domain-specific conversion: constructs a `&AutounsignalEvent` from a raw bindings pointer.
    ///
    /// # Safety
    ///
    /// `ptr` must be a valid, non-null, properly aligned pointer to a live, initialized
    /// `AutounsignalEvent` that remains valid for the duration of `'a`, and no mutable references
    /// to it may exist for the duration of `'a`.
    pub unsafe fn from_raw_ref<'a>(ptr: *const bindings::AutounsignalEvent) -> &'a Self {
        let ptr: *const Self = ptr.cast();
        // SAFETY: `bindings::AutounsignalEvent` is layout-compatible with `AutounsignalEvent`, and
        // the caller guarantees `ptr` is valid, aligned, and unaliased by mutable references for
        // `'a`.
        unsafe { ptr.as_ref_unchecked() }
    }

    /// Returns a raw `AutounsignalEvent` pointer from an underlying bindings pointer.
    ///
    /// Provides additional type safety when used instead of a `.cast()`.
    pub fn ptr_from_raw(raw: *mut bindings::AutounsignalEvent) -> *mut AutounsignalEvent {
        raw.cast()
    }

    /// Returns a pointer to the underlying `AutounsignalEvent` structure.
    ///
    /// This method is helpful when you don't have a reference to the `AutounsignalEvent`. If you
    /// do, then use `AutounsignalEvent::as_raw` instead.
    pub fn cast_raw(ptr: *mut AutounsignalEvent) -> *mut bindings::AutounsignalEvent {
        ptr.cast()
    }

    /// Creates an in-place initializer for `AutounsignalEvent`.
    pub fn init(initial: bool) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self { event <- Event::init_with_flags(initial, FLAGS_AUTOUNSIGNAL) })
    }

    /// Creates an in-place initializer for an initially unsignaled `AutounsignalEvent`.
    pub fn init_unsignaled() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init(false)
    }

    /// Creates an in-place initializer for an initially signaled `AutounsignalEvent`.
    pub fn init_signaled() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init(true)
    }

    /// Returns a reference to the underlying [`Event`].
    pub fn as_event(&self) -> &Event {
        self.deref()
    }
}

impl Deref for AutounsignalEvent {
    type Target = Event;

    fn deref(&self) -> &Self::Target {
        &self.event
    }
}

// Rust implementations of the C++ `Event` methods; the C++ methods of the same name are thin
// forwarders to these.

/// FFI entry point for `Event::WaitWorker`.
///
/// # Safety
/// `event` must be a valid `Event` and `deadline` a valid `Deadline` (if not infinite, on the
/// monotonic timeline); no `ChainLockTransaction` may be active on this CPU, and the caller must
/// be in blockable thread context.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_event_wait_worker(
    event: *const Event,
    deadline: *const Deadline,
    interruptible: Interruptible,
    signal_mask: u32,
) -> zx_status_t {
    // SAFETY: The caller guarantees `deadline` is valid; only a shared reference is formed.
    let deadline = unsafe { &*deadline };
    // SAFETY: The caller guarantees `event` points to a valid `Event`, `deadline` is on the
    // monotonic timeline (if finite), and we are in blockable thread context with no active
    // `ChainLockTransaction`.
    Status::result_into_raw(unsafe {
        Event::wait_worker(event, deadline, interruptible, signal_mask)
    })
}

/// FFI entry point for `Event::Signal`.
///
/// # Safety
/// `event` must point to a valid, initialized `Event` that stays live through the signal operation
/// (see [`Event::signal_worker`]), `queue_to_own` must be null or point to a valid, initialized
/// `OwnedWaitQueue`, and no `ChainLockTransaction` may be active on this CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_event_signal(
    event: *const Event,
    wait_result: zx_status_t,
    queue_to_own: *mut OwnedWaitQueue,
) {
    // SAFETY: The caller guarantees `queue_to_own` is null or a valid `OwnedWaitQueue`; only a
    // shared reference is formed, all mutation goes through its locked cells.
    let queue_to_own: Option<&OwnedWaitQueue> = unsafe { queue_to_own.as_ref() };
    // SAFETY: Forwarded from the caller; `event` is passed as `*const Event` so no `&Event`
    // outlives `wait.lock()` inside `signal_worker`.
    unsafe { Event::signal_worker(event, Status::ok(wait_result), queue_to_own) };
}

/// FFI entry point for `Event::Unsignal`.
///
/// # Safety
/// `event` must be a valid `Event`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_event_unsignal(event: *const Event) -> zx_status_t {
    // SAFETY: The caller guarantees `event` is valid; only a shared reference is formed.
    let event = unsafe { &*event };
    Status::result_into_raw(event.unsignal())
}

/// The `Event`-specific half of C++ `~Event`; C++ destroys the embedded `WaitQueue` itself.
///
/// # Safety
/// `event` must point to a live `Event` that C++ is destroying and that no other thread can
/// concurrently read or write.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_event_destroy(event: *mut Event) {
    // SAFETY: The caller guarantees `event` is a live `Event` being destroyed with exclusive
    // teardown access to `magic` and `flags`.
    unsafe { (*event).destroy_fields() };
}

/// [`Event`] and [`AutounsignalEvent`] unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "event_rust")]
mod tests {
    use super::{AutounsignalEvent, Event, FLAGS_AUTOUNSIGNAL, FLAGS_NONE, MAGIC, NOT_SIGNALED};
    use crate::kconcurrent::{IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned};
    use crate::kernel::deadline::Deadline;
    use crate::kernel::owned_wait_queue::OwnedWaitQueue;
    use crate::kernel::thread::{self, DEFAULT_PRIORITY, Interruptible, ThreadPtr};
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time, zx_usec};
    use core::ffi::c_void;
    use core::sync::atomic::Ordering;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_eq, assert_true, expect_eq, expect_err, expect_false, expect_true, subtest,
        unwrap_ok,
    };
    use zx_status::Status;
    use zx_types::{ZX_ERR_BAD_STATE, ZX_ERR_CANCELED, ZX_OK, ZX_TIME_INFINITE, zx_status_t};

    /// A result passed to `signal_etc` before any thread waits is propagated to the waiter.
    #[test]
    fn signal_result_before_wait_test() {
        stack_pin_init!(let event = Event::init_unsignaled());

        let signal_result = Status::err_from_raw(42);
        event.signal_result(signal_result);

        let wake_result = event.wait_infinite();

        expect_err!(wake_result, signal_result, "");
    }

    struct EventWaiterArgs<'a> {
        event: &'a Event,
        wake_result: Result<(), Status>,
    }

    extern "C" fn event_waiter_thread(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a valid `EventWaiterArgs` that outlives this thread, and no
        // other thread accesses `wake_result` until after this thread is joined.
        let event_args = unsafe { &mut *arg.cast::<EventWaiterArgs<'_>>() };
        event_args.wake_result = event_args.event.wait_infinite();
        0
    }

    #[derive(Clone, Copy)]
    enum Signal {
        Only,
        Queue,
    }

    /// Tests that the result in a `signal_etc` call is propagated to the waiter when the waiter
    /// enters a blocking state before the event is signaled.
    fn run_signal_result_after_wait_test(signal: Signal) -> bool {
        (subtest!(|signal: Signal| {
            stack_pin_init!(let event = Event::init_unsignaled());

            let signal_result = Status::err_from_raw(42);

            let mut args = EventWaiterArgs { event: &event, wake_result: Ok(()) };

            // SAFETY: `event_waiter_thread` and `&mut args` are valid, and `waiter` is joined
            // below before `args` or `event` go out of scope.
            let waiter = unwrap_ok!(unsafe {
                thread::create_with_priority(
                    c"event waiter thread".as_ptr(),
                    event_waiter_thread,
                    (&raw mut args).cast(),
                    DEFAULT_PRIORITY,
                )
            });
            // SAFETY: `waiter` was just created and has not been resumed or joined yet.
            unsafe { waiter.resume() };

            let mut wait_duration = DurationMono(zx_usec(1));
            loop {
                // Check if the waiter thread is in the blocked state, indicating that the event
                // has latched.
                // SAFETY: `waiter` has not been joined yet.
                if unsafe { waiter.is_blocked() } {
                    break;
                }
                // Nope - sleep and try again.
                let _ = thread::sleep_relative(wait_duration);
                wait_duration = wait_duration * 2;
            }

            match signal {
                Signal::Queue => {
                    stack_pin_init!(let queue = OwnedWaitQueue::init());
                    event.signal_etc(signal_result, Some(&*queue));
                    queue.reset_owner_if_no_waiters();
                }
                Signal::Only => {
                    event.signal_result(signal_result);
                }
            }

            // SAFETY: `waiter` has not been joined yet.
            let thread_retcode = unwrap_ok!(unsafe { waiter.join(InstantMono::INFINITE) });

            assert_eq!(thread_retcode, 0, "");

            expect_err!(args.wake_result, signal_result, "");
        }))(signal)
    }

    /// test signaling event with result after waiting
    #[test]
    fn signal_result_after_wait_test() {
        assert_true!(run_signal_result_after_wait_test(Signal::Only));
    }

    /// test signaling event with result after waiting (using OwnedWaitQueue)
    #[test]
    fn signal_etc_after_wait_test() {
        assert_true!(run_signal_result_after_wait_test(Signal::Queue));
    }

    /// Layout and constants: a fresh `Event`/`AutounsignalEvent` initializes as expected.
    #[test]
    fn test_event_init_fields() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        // SAFETY: `magic` and `flags` are immutable while the `Event` is live.
        expect_eq!(unsafe { *event.magic.get() }, MAGIC);
        expect_eq!(event.result.load(Ordering::Relaxed), NOT_SIGNALED);
        // SAFETY: `magic` and `flags` are immutable while the `Event` is live.
        expect_eq!(unsafe { *event.flags.get() }, FLAGS_NONE);
        expect_false!(event.is_signaled());

        pin_init::stack_pin_init!(let signaled = Event::init_signaled());
        expect_eq!(signaled.result.load(Ordering::Relaxed), ZX_OK);
        expect_true!(signaled.is_signaled());

        pin_init::stack_pin_init!(let auto_event = AutounsignalEvent::init_unsignaled());
        // SAFETY: `magic` and `flags` are immutable while the `AutounsignalEvent` is live.
        expect_eq!(unsafe { *auto_event.magic.get() }, MAGIC);
        // SAFETY: `magic` and `flags` are immutable while the `AutounsignalEvent` is live.
        expect_eq!(unsafe { *auto_event.flags.get() }, FLAGS_AUTOUNSIGNAL);
        expect_true!(core::ptr::eq(auto_event.as_event(), &**auto_event));
        expect_true!(core::ptr::eq(auto_event.as_raw().cast::<Event>(), auto_event.as_event()));
    }

    /// Signal, unsignal, custom results, and waits that never block.
    #[test]
    fn test_event_signal_unsignal() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        expect_false!(event.is_signaled());

        // Signal before wait: the wait returns immediately, even with a deadline in the past, and
        // a standard event stays signaled.
        event.signal();
        expect_true!(event.is_signaled());
        expect_true!(event.wait(&Deadline::infinite_past()).is_ok());
        expect_true!(event.wait_infinite().is_ok());
        expect_true!(event.is_signaled());

        // Unsignal after signal makes the next wait block (here: time out).
        expect_true!(event.unsignal().is_ok());
        expect_false!(event.is_signaled());
        expect_true!(event.wait(&Deadline::infinite_past()) == Err(Status::TIMED_OUT));
        expect_true!(event.wait_mask(&Deadline::infinite_past(), 0) == Err(Status::TIMED_OUT));
        expect_true!(
            event.wait_deadline(InstantMono(1), Interruptible::No) == Err(Status::TIMED_OUT)
        );
        expect_true!(
            event.wait_deadline(InstantMono(1), Interruptible::Yes) == Err(Status::TIMED_OUT)
        );

        // A custom result is stored and returned to waiters.
        event.signal_result(Err(Status::CANCELED));
        expect_true!(event.is_signaled());
        expect_true!(event.wait_infinite() == Err(Status::CANCELED));
        // Signaling an already signaled event keeps the stored result.
        event.signal();
        expect_true!(event.wait_infinite() == Err(Status::CANCELED));
        expect_true!(event.unsignal().is_ok());

        // `signal_etc` with no queue to own.
        event.signal_etc(Ok(()), None);
        expect_true!(event.is_signaled());
        expect_eq!(event.result.load(Ordering::Relaxed), ZX_OK);
    }

    /// An `AutounsignalEvent` lets one waiter through and resets.
    #[test]
    fn test_autounsignal_event_no_waiters() {
        pin_init::stack_pin_init!(let event = AutounsignalEvent::init_signaled());
        expect_true!(event.is_signaled());

        // Signaling while signaled is a no-op.
        event.signal();
        expect_true!(event.is_signaled());

        expect_true!(event.wait_infinite().is_ok());
        // AutounsignalEvent automatically resets to unsignaled.
        expect_false!(event.is_signaled());
        expect_eq!(event.result.load(Ordering::Relaxed), NOT_SIGNALED);
        expect_true!(event.wait(&Deadline::infinite_past()) == Err(Status::TIMED_OUT));
    }

    /// A short deadline wait on an unsignaled event times out via the wait queue's timer.
    #[test]
    fn test_event_wait_times_out() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        let deadline = InstantMono(current_mono_time().0 + 1_000_000); // 1ms
        expect_true!(event.wait_deadline(deadline, Interruptible::No) == Err(Status::TIMED_OUT));
        expect_false!(event.is_signaled());
        expect_eq!(queue_count(&event), 0);
    }

    fn queue_count(event: &Event) -> u32 {
        single_chain_lock_guard_owned!(scope, &event.wait, IRQ_SAVE_OPTION, clt_tag!("evt_cnt"));
        scope.proof().count()
    }

    /// Shared with a helper thread that waits on `event` until `deadline` (monotonic), parks on
    /// `gate` until the test releases it, and exits with the raw status `wait` returned.
    struct WaitCtx {
        event: *const Event,
        /// A plain `Event` (not autounsignal), so a `signal()` that lands before the waiter parks
        /// still releases it. Keeps the waiter alive until the test has inspected state that
        /// thread exit would clear, such as the owner of an `OwnedWaitQueue`.
        gate: *const Event,
        deadline: InstantMono,
    }

    impl WaitCtx {
        /// A generous but finite deadline, so a failed test self-terminates via the wait queue's
        /// timeout instead of wedging the join.
        fn ten_seconds(event: &Event, gate: &Event) -> Self {
            Self {
                event: core::ptr::from_ref(event),
                gate: core::ptr::from_ref(gate),
                deadline: InstantMono(current_mono_time().0 + 10 * 1_000_000_000),
            }
        }
    }

    extern "C" fn wait_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` is the `WaitCtx` the spawning test keeps alive until it joins us, as are
        // the events it points at.
        let (ctx, event, gate) = unsafe {
            let ctx = &*arg.cast::<WaitCtx>();
            (ctx, &*ctx.event, &*ctx.gate)
        };
        let res = event.wait_deadline(ctx.deadline, Interruptible::No);
        let _ = gate.wait_infinite();
        Status::result_into_raw(res)
    }

    /// `N` waiters spawned on one `WaitCtx`, joined on drop (after opening the gate and signaling
    /// the event once per remaining waiter) so an early test return never leaves one live.
    struct Waiters<'a, const N: usize> {
        ctx: &'a WaitCtx,
        threads: [Option<ThreadPtr>; N],
    }

    impl<'a, const N: usize> Waiters<'a, N> {
        fn spawn(ctx: &'a WaitCtx) -> Self {
            let threads = core::array::from_fn(|_| {
                // SAFETY: `ctx` outlives the thread, which `Drop` joins.
                unsafe {
                    thread::spawn(
                        c"event_rust_waiter".as_ptr(),
                        wait_entry,
                        core::ptr::from_ref(ctx).cast_mut().cast(),
                    )
                }
                .ok()
            });
            Self { ctx, threads }
        }

        /// Waits for every waiter to be blocked in the event's queue.
        fn all_spawned_and_blocked(&self) -> bool {
            self.threads.iter().all(|t| {
                t.is_some_and(|t| {
                    for _ in 0..5000 {
                        // SAFETY: `t` has not been joined yet.
                        if unsafe { t.is_blocked() } {
                            return true;
                        }
                        let _ = thread::sleep_relative(DurationMono(1_000_000)); // 1ms
                    }
                    false
                })
            })
        }

        fn get(&self, i: usize) -> ThreadPtr {
            self.threads[i].expect("waiter was not spawned or was already joined")
        }

        /// Joins waiter `i` (the gate must already be open) and returns its raw exit status.
        fn join(&mut self, i: usize) -> zx_status_t {
            match self.threads[i].take() {
                Some(t) => {
                    // SAFETY: Each waiter is joined exactly once.
                    unsafe { t.join(InstantMono(ZX_TIME_INFINITE)) }.unwrap_or(ZX_ERR_BAD_STATE)
                }
                None => ZX_ERR_BAD_STATE,
            }
        }
    }

    impl<const N: usize> Drop for Waiters<'_, N> {
        fn drop(&mut self) {
            // SAFETY: The context and its events outlive `self` (the test declares them first).
            let (event, gate) = unsafe { (&*self.ctx.event, &*self.ctx.gate) };
            gate.signal();
            for t in self.threads.iter_mut().filter_map(Option::take) {
                // An AutounsignalEvent wakes only one thread per signal.
                event.signal();
                // SAFETY: Each waiter is joined exactly once.
                let _ = unsafe { t.join(InstantMono(ZX_TIME_INFINITE)) };
            }
        }
    }

    /// The owner of `owq`, read under its lock.
    fn owq_owner(owq: &OwnedWaitQueue) -> Option<ThreadPtr> {
        single_chain_lock_guard_owned!(scope, owq, IRQ_SAVE_OPTION, clt_tag!("evt_owq_owner"));
        scope.proof().queue_owner()
    }

    /// Waits for the number of threads blocked in `event`'s queue to drop to `count`.
    fn wait_until_count(event: &Event, count: u32) -> bool {
        for _ in 0..5000 {
            if queue_count(event) == count {
                return true;
            }
            let _ = thread::sleep_relative(DurationMono(1_000_000)); // 1ms
        }
        false
    }

    /// Each signal of an `AutounsignalEvent` wakes exactly one waiter and leaves it unsignaled.
    #[test]
    fn test_autounsignal_event_wakes_one_per_signal() {
        pin_init::stack_pin_init!(let event = AutounsignalEvent::init_unsignaled());
        // Nothing to inspect before the waiters exit, so the gate is open from the start.
        pin_init::stack_pin_init!(let gate = Event::init_signaled());
        let ctx = WaitCtx::ten_seconds(&event, &gate);
        let mut waiters = Waiters::<2>::spawn(&ctx);
        unittest::assert_true!(waiters.all_spawned_and_blocked());
        expect_eq!(queue_count(&event), 2);

        // Each signal wakes exactly one thread and leaves the event unsignaled.
        event.signal();
        expect_true!(wait_until_count(&event, 1));
        expect_false!(event.is_signaled());
        event.signal();
        expect_true!(wait_until_count(&event, 0));
        expect_false!(event.is_signaled());

        expect_eq!(waiters.join(0), ZX_OK);
        expect_eq!(waiters.join(1), ZX_OK);
        expect_false!(event.is_signaled());
    }

    /// `signal_etc` with a queue to own wakes the waiter and makes it the queue's owner.
    #[test]
    fn test_event_signal_etc_assigns_owner() {
        pin_init::stack_pin_init!(let event = Event::init_unsignaled());
        pin_init::stack_pin_init!(let owq = OwnedWaitQueue::init());
        // Keeps the woken waiter from exiting (and disowning `owq`) before the owner is checked.
        pin_init::stack_pin_init!(let gate = Event::init_unsignaled());
        let ctx = WaitCtx::ten_seconds(&event, &gate);
        let mut waiters = Waiters::<1>::spawn(&ctx);
        unittest::assert_true!(waiters.all_spawned_and_blocked());
        let waiter = waiters.get(0);

        event.signal_etc(Err(Status::CANCELED), Some(&*owq));
        expect_true!(owq_owner(&owq) == Some(waiter));
        owq.reset_owner_if_no_waiters();
        expect_true!(owq_owner(&owq).is_none());

        gate.signal();
        expect_eq!(waiters.join(0), ZX_ERR_CANCELED);
        expect_true!(event.is_signaled());
        expect_eq!(queue_count(&event), 0);
    }
}
