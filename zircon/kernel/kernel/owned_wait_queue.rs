// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kconcurrent::{ChainLock, ChainLockTransactionToken};
use crate::kernel::deadline::Deadline;
use crate::kernel::scheduler_state::WaitQueueInheritedSchedulerState;
use crate::kernel::thread::{Thread, ThreadPtr};
use crate::kernel::wait_queue::{Interruptible, ResourceOwnership, WaitQueueBase};
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{DoublyLinkedListContainable, DoublyLinkedListNode};
use owned_wait_queue_bindings as bindings;
use pin_init::{PinInit, pin_data, pinned_drop};
use zr::static_assert;
use zx_status::Status;
use zx_types::zx_status_t;

/// Magic value identifying an [`OwnedWaitQueue`].
pub const OWNED_MAGIC: u32 = fbl::magic(b"ownq");
static_assert!(OWNED_MAGIC == bindings::OwnedWaitQueue_kOwnedMagic);

/// A small struct which contains details about the locking used in order to prepare for a
/// `BlockAndAssignOwner` operation, matching C++ `OwnedWaitQueue::BAAOLockingDetails`.
///
/// Produced by [`OwnedWaitQueue::lock_for_baao_operation_or_backoff`] or
/// [`OwnedWaitQueue::try_lock_for_baao_operation_locked`] and consumed by
/// [`OwnedWaitQueue::block_and_assign_owner_locked`].
///
/// The details describe chain locks held by the active `ChainLockTransaction` on the current CPU,
/// so this type is neither `Send` nor `Sync` (it only contains raw pointers).
#[must_use]
#[repr(C)]
pub struct BAAOLockingDetails {
    // The new owner selected by the locking operation (C++ `new_owner`). Only read by C++.
    _new_owner: *mut Thread,
    // The point at which unlocking the old owner chain must stop (C++ `stop_point`). Only read by
    // C++.
    _stop_point: *const c_void,
}

// Keep in sync with the `static_assert`s in owned_wait_queue_ffi.cc.
static_assert!(size_of::<BAAOLockingDetails>() == 16);
static_assert!(align_of::<BAAOLockingDetails>() == 8);
static_assert!(offset_of!(BAAOLockingDetails, _new_owner) == 0);
static_assert!(offset_of!(BAAOLockingDetails, _stop_point) == 8);

/// Behavior control for wake/requeue operations, matching C++ `OwnedWaitQueue::WakeOption`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WakeOption {
    /// Wake threads and clear queue ownership if no threads remain waiting.
    None = 0,
    /// Wake one thread and assign it as the new owner of the queue.
    AssignOwner = 1,
}

/// The state of the queue as observed just before the queue lock was dropped at the end of a wake
/// or wake-and-requeue operation, matching C++ `OwnedWaitQueue::WakeThreadsResult`.
///
/// Used by futex and kernel mutex implementations to update their bookkeeping before unwinding
/// after a wake operation.
///
/// For [`OwnedWaitQueue::wake_threads`], [`OwnedWaitQueue::wake_thread_and_assign_owner`], and
/// [`OwnedWaitQueue::wake_and_requeue`], the queue lock and the `ChainLockTransaction` have
/// already been released by the time the result is returned, so `owner` is only a historical
/// snapshot. It must not be dereferenced unless the caller independently guarantees that the
/// thread is still alive.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WakeThreadsResult {
    /// Number of threads woken by the operation.
    pub woken: u32,
    /// Number of threads still waiting in the queue after the operation.
    pub still_waiting: u32,
    /// Owner of the queue as observed just before the queue lock was dropped, if any.
    pub owner: Option<ThreadPtr>,
}

/// Hook interface injected into wake and requeue operations, matching C++
/// `OwnedWaitQueue::IWakeRequeueHook`.
///
/// Hooks are used during thread selection and to update external bookkeeping. Both methods are
/// invoked by C++ `OwnedWaitQueue` while it holds chain locks inside an active
/// `ChainLockTransaction`, so implementations must not block, must not acquire chain locks, and
/// must not call back into any wake, requeue, or BAAO operation of any [`OwnedWaitQueue`].
pub trait WakeRequeueHook {
    /// Returns whether `thread` should be woken (or requeued).
    ///
    /// Called during the locking phase of a wake or a requeue operation. The locking code is
    /// holding the queue's lock at this point, but not `thread`'s lock yet. `thread` is still a
    /// member of the queue, meaning implementations only have read-only access to `thread`'s
    /// state.
    ///
    /// If `true` is returned, the locking operation will attempt to lock `thread` in order to
    /// commit it to waking, backing off if needed. If `false` is returned, `thread` will be left in
    /// the queue and the enumeration of threads in the queue ceases.
    ///
    /// May be called multiple times for the same thread during a single operation if the locking
    /// code needs to back off and attempt to lock the set of threads to be woken multiple times.
    #[inline]
    fn allow(&mut self, _thread: ThreadPtr) -> bool {
        true
    }

    /// Called in the second phase of a wake/requeue operation.
    ///
    /// At this point, all of the threads to wake have been identified and successfully locked.
    /// This hook is called while holding `thread`'s chain lock, _just_ before `thread` is removed
    /// from its queue, and either unblocked (wake operation) or moved to a different queue
    /// (requeue operation).
    #[inline]
    fn on_wake_or_requeue(&mut self, _thread: ThreadPtr) {}
}

/// A [`WakeRequeueHook`] which allows every thread and performs no extra bookkeeping, matching
/// C++ `OwnedWaitQueue::default_wake_hooks()`.
#[derive(Default, Debug)]
pub struct DefaultWakeHooks;

impl WakeRequeueHook for DefaultWakeHooks {}

extern "C" fn hook_allow_trampoline<H: WakeRequeueHook>(
    ctx: *mut c_void,
    thread: *const Thread,
) -> bool {
    // SAFETY: `ctx` was created from a `&mut H` which outlives the C++ wake/requeue call invoking
    // this callback, and no other reference to it is live during the callback. `thread` is
    // non-null because C++ `OwnedWaitQueue` passes a `const Thread&` to a live queue member.
    let (hook, thread_ptr) = unsafe {
        (&mut *ctx.cast::<H>(), ThreadPtr::from_raw(thread.cast_mut()).unwrap_unchecked())
    };
    hook.allow(thread_ptr)
}

extern "C" fn hook_on_wake_trampoline<H: WakeRequeueHook>(ctx: *mut c_void, thread: *mut Thread) {
    // SAFETY: `ctx` was created from a `&mut H` which outlives the C++ wake/requeue call invoking
    // this callback, and no other reference to it is live during the callback. `thread` is
    // non-null because C++ `OwnedWaitQueue` passes a `Thread&` whose chain lock it holds.
    let (hook, thread_ptr) =
        unsafe { (&mut *ctx.cast::<H>(), ThreadPtr::from_raw(thread).unwrap_unchecked()) };
    hook.on_wake_or_requeue(thread_ptr);
}

// These routines are defined in owned_wait_queue_ffi.cc. They are declared here rather than being
// generated by the owned-wait-queue-bindings crate, since they refer to types which that crate
// does not bind.
#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_owned_wait_queue_assert_not_owned(wait_queue: *const bindings::OwnedWaitQueue);
    fn cpp_owned_wait_queue_lock_for_baao_operation_or_backoff(
        wait_queue: *mut bindings::OwnedWaitQueue,
        current_thread: *mut Thread,
        new_owner: *mut Thread,
        details_out: *mut MaybeUninit<BAAOLockingDetails>,
    ) -> bool;
    fn cpp_owned_wait_queue_try_lock_for_baao_operation_locked(
        wait_queue: *mut bindings::OwnedWaitQueue,
        current_thread: *mut Thread,
        new_owner: *mut Thread,
        details_out: *mut MaybeUninit<BAAOLockingDetails>,
    ) -> bool;
    fn cpp_owned_wait_queue_block_and_assign_owner_locked(
        wait_queue: *mut bindings::OwnedWaitQueue,
        current_thread: *mut Thread,
        deadline: *const Deadline,
        details: *const BAAOLockingDetails,
        resource_ownership: ResourceOwnership,
        interruptible: Interruptible,
    ) -> zx_status_t;
    fn cpp_owned_wait_queue_wake_threads(
        wait_queue: *mut bindings::OwnedWaitQueue,
        wake_count: u32,
        allow_fn: extern "C" fn(*mut c_void, *const Thread) -> bool,
        on_wake_fn: extern "C" fn(*mut c_void, *mut Thread),
        hook_ctx: *mut c_void,
        out_woken: *mut u32,
        out_still_waiting: *mut u32,
        out_owner: *mut *mut Thread,
    );
    fn cpp_owned_wait_queue_wake_thread_and_assign_owner(
        wait_queue: *mut bindings::OwnedWaitQueue,
        allow_fn: extern "C" fn(*mut c_void, *const Thread) -> bool,
        on_wake_fn: extern "C" fn(*mut c_void, *mut Thread),
        hook_ctx: *mut c_void,
        out_woken: *mut u32,
        out_still_waiting: *mut u32,
        out_owner: *mut *mut Thread,
    );
    fn cpp_owned_wait_queue_wake_and_requeue(
        wait_queue: *mut bindings::OwnedWaitQueue,
        requeue_target: *mut bindings::OwnedWaitQueue,
        new_requeue_owner: *mut Thread,
        wake_count: u32,
        requeue_count: u32,
        wake_allow_fn: extern "C" fn(*mut c_void, *const Thread) -> bool,
        wake_fn: extern "C" fn(*mut c_void, *mut Thread),
        wake_ctx: *mut c_void,
        requeue_allow_fn: extern "C" fn(*mut c_void, *const Thread) -> bool,
        requeue_fn: extern "C" fn(*mut c_void, *mut Thread),
        requeue_ctx: *mut c_void,
        wake_option: WakeOption,
        out_woken: *mut u32,
        out_still_waiting: *mut u32,
        out_owner: *mut *mut Thread,
    );
}

/// Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is active on the
/// current CPU.
///
/// Used by the operations which internally run their own `ChainLockTransaction::UntilDone`
/// (`TA_EXCL(chainlock_transaction_token)`). Registering a transaction while another one is
/// active is only checked by a `DEBUG_ASSERT` in C++, so in release builds a nested call would
/// silently overwrite the outer transaction.
#[inline]
#[track_caller]
fn assert_no_active_transaction(operation: &str) {
    // A registered transaction always keeps interrupts disabled, so the per-CPU slot only needs to
    // be checked (without the risk of migrating to another CPU) when interrupts are disabled.
    assert!(
        !crate::arch_rs::ints_disabled()
            || crate::kernel::percpu::PerCpu::get_current().active_cl_transaction().is_null(),
        "{operation} must not be called with an active ChainLockTransaction"
    );
}

/// Owned wait queues are an extension of wait queues which adds the concept of
/// ownership for use when profile inheritance semantics are needed.
///
/// An owned wait queue maintains an unmanaged pointer to a `Thread` in order to
/// track who owns it at any point in time. In addition, it contains node state
/// which can be used by the owning thread in order to track the wait queues that
/// the thread is currently an owner of. This also makes use of unmanaged
/// pointer.
///
/// It should be an error for any thread to destruct while it owns an
/// `OwnedWaitQueue`. Likewise, it should be an error for any wait queue to
/// destruct while it has an owner. These invariants are enforced in the
/// destructor for `OwnedWaitQueue` and `Thread`. This enforcement is considered
/// to be the reasoning why holding unmanaged pointers is considered to be safe.
#[repr(C)]
#[derive(DoublyLinkedListContainable)]
#[pin_data(PinnedDrop)]
pub struct OwnedWaitQueue {
    // Safety invariant: Initialized as the `WaitQueueBase` subobject of this `OwnedWaitQueue`
    // with `magic == OWNED_MAGIC`.
    #[pin]
    base: WaitQueueBase,
    // Safety invariant: Linked into the owning `Thread`'s `owned_wait_queues_` list if and only
    // if `owner` is `Some(...)`; guarded by `base`'s `ChainLock` and the owning `Thread`'s
    // `ChainLock`.
    #[pin]
    #[dll_node]
    dll_node: DoublyLinkedListNode<OwnedWaitQueue>,
    // Safety invariant: Guarded by `base`'s `ChainLock`; either `None` or points to a live
    // `Thread` that currently owns this queue. Must be `None` when dropped.
    owner: UnsafeCell<Option<NonNull<Thread>>>,
    /// A pointer to a thread (which _must_ be a current member of this owned wait queue's)
    /// collection which holds the current inherited scheduler state storage for
    /// this wait queue.
    ///
    /// Anytime that a thread is added to this queue, if the collection was
    /// empty, then the thread added becomes the new location for the storage.
    ///
    /// Anytime that a thread is removed from this queue, if the thread being
    /// removed was the active location of the storage, then a new thread needs to
    /// be selected and the values moved into the new storage location.
    ///
    /// The primary purpose of this indirection is to keep the `OwnedWaitQueue` object
    /// relatively-lightweight. The only time that a wait queue has defined
    /// inherited scheduler state is when there are one or more threads blocked in
    /// the queue, so storing this bookkeeping in a blocked thread object seems
    /// reasonable.
    // Safety invariant: Guarded by `base`'s `ChainLock`; `None` when `base`'s collection is empty,
    // or points to the `inherited_scheduler_state_storage` of a `Thread` currently blocked in
    // `base`'s collection.
    inherited_scheduler_state_storage:
        UnsafeCell<Option<NonNull<WaitQueueInheritedSchedulerState>>>,
}

// Compile-time layout assertions against the C++ OwnedWaitQueue type via bindgen.
static_assert!(size_of::<OwnedWaitQueue>() == size_of::<bindings::OwnedWaitQueue>());
static_assert!(align_of::<OwnedWaitQueue>() == align_of::<bindings::OwnedWaitQueue>());
static_assert!(offset_of!(OwnedWaitQueue, base) == offset_of!(bindings::OwnedWaitQueue, _base));
static_assert!(
    offset_of!(OwnedWaitQueue, dll_node) == offset_of!(bindings::OwnedWaitQueue, _base_1)
);
static_assert!(offset_of!(OwnedWaitQueue, owner) == offset_of!(bindings::OwnedWaitQueue, owner_));
static_assert!(
    offset_of!(OwnedWaitQueue, inherited_scheduler_state_storage)
        == offset_of!(bindings::OwnedWaitQueue, inherited_scheduler_state_storage_)
);

// SAFETY: `OwnedWaitQueue` has no thread-local ownership invariants and is `!Unpin`, so ownership
// of a pinned `OwnedWaitQueue` can be transferred across threads.
unsafe impl Send for OwnedWaitQueue {}
// SAFETY: All interior mutations to `OwnedWaitQueue` fields through shared references are
// synchronized by the embedded `WaitQueueBase`'s `ChainLock`.
unsafe impl Sync for OwnedWaitQueue {}

#[pinned_drop]
impl PinnedDrop for OwnedWaitQueue {
    fn drop(self: Pin<&mut Self>) {
        // Something is very very wrong if we have been allowed to destruct while we
        // still have an owner.
        // The remainder of the C++ destructor, i.e. tearing down the base wait queue and the
        // intrusive list node, is performed by the drop glue of the individual members.
        // SAFETY: We do not move `self` or any pinned fields out of `this`.
        let this = unsafe { self.get_unchecked_mut() };
        debug_assert!(this.owner.get_mut().is_none());
    }
}

impl OwnedWaitQueue {
    pub const OWNED_MAGIC: u32 = OWNED_MAGIC;

    /// In-place pinned initializer for `OwnedWaitQueue`.
    pub fn init() -> impl PinInit<Self, Infallible> {
        // SAFETY: `bindings::cpp_owned_wait_queue_init` placement-new initializes a valid
        // `OwnedWaitQueue` in-place in the provided slot.
        zr::pin_init_ffi!(bindings::cpp_owned_wait_queue_init)
    }

    /// Domain-specific conversion: returns raw pointer for `OwnedWaitQueue`.
    pub fn as_raw(&self) -> *mut bindings::OwnedWaitQueue {
        (self as *const Self).cast_mut().cast()
    }

    /// Returns a raw mutable pointer to the underlying C++ `OwnedWaitQueue`.
    pub fn as_ptr(&self) -> *mut c_void {
        self.as_raw().cast()
    }

    /// Returns a reference to this wait queue's [`ChainLock`].
    #[inline]
    pub fn get_lock(&self) -> &ChainLock {
        self.base.get_lock()
    }

    /// Returns the current owner of this wait queue, if any.
    ///
    /// # Panics
    ///
    /// Panics if this wait queue's lock is not held by the transaction identified by `token`
    /// (`TA_REQ(get_lock())`).
    #[inline]
    pub fn owner_locked(&self, token: &ChainLockTransactionToken) -> Option<ThreadPtr> {
        assert!(
            self.get_lock().is_held(token),
            "owner_locked requires the OwnedWaitQueue's lock to be held"
        );
        // SAFETY: `owner` is guarded by `self.get_lock()`, which the assertion above verified is
        // held by the active transaction, so there are no concurrent writes.
        let owner = unsafe { *self.owner.get() };
        owner.and_then(|owner| {
            // SAFETY: By `owner`'s safety invariant, it points to a live `Thread`.
            unsafe { ThreadPtr::from_raw(owner.as_ptr()) }
        })
    }

    /// Asserts (in debug builds) that this wait queue currently has no owner.
    ///
    /// Like the C++ `AssertNotOwned`, this skips the lock requirement, so it must only be used
    /// where no other thread can concurrently change the owner of this queue.
    #[inline]
    pub fn assert_not_owned(&self) {
        // SAFETY: `self.as_raw()` returns a valid pointer to this initialized `OwnedWaitQueue`.
        unsafe { cpp_owned_wait_queue_assert_not_owned(self.as_raw()) }
    }

    /// Resets the queue owner if there are no waiters currently queued.
    ///
    /// # Panics
    ///
    /// Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is already
    /// active on the current CPU.
    pub fn reset_owner_if_no_waiters(&self) {
        assert!(
            !crate::arch_rs::ints_disabled()
                || crate::kernel::percpu::PerCpu::get_current().active_cl_transaction().is_null(),
            "reset_owner_if_no_waiters must not be called with an active ChainLockTransaction"
        );
        // SAFETY:
        // - `self.as_raw()` returns a valid pointer to this initialized `OwnedWaitQueue`.
        // - All fields of `OwnedWaitQueue` and its embedded `WaitQueueBase` that may be mutated
        //   under the queue's `ChainLock` (`base.collection`, `dll_node`, `owner`, and
        //   `inherited_scheduler_state_storage`) are wrapped in `UnsafeCell`.
        // - The assertion above ensures no `ChainLockTransaction` is active on the current CPU
        //   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
        unsafe {
            bindings::cpp_owned_wait_queue_reset_owner_if_no_waiters(self.as_raw());
        }
    }

    /// Obtains all of the chain locks needed for a `BlockAndAssignOwner` operation, handling the
    /// special case of new and old owners which share a PI graph target.
    ///
    /// The proposed `new_owner` may be replaced with no owner if assigning it would produce a cycle
    /// in the PI graph; the final choice is recorded in the returned [`BAAOLockingDetails`].
    ///
    /// # Locking
    ///
    /// - On `Some(details)`, `self.get_lock()`, `current_thread.get_lock()`, and all of the PI
    ///   chain locks for the owner paths described by `details` are held by the transaction
    ///   identified by `token`. They must be consumed by passing `details` to
    ///   [`Self::block_and_assign_owner_locked`].
    /// - On `None`, all locks acquired by this call (including `self.get_lock()`) have been
    ///   released. This method does not relax the transaction itself; the caller must back off
    ///   (e.g. return `Action::Backoff` from a `ChainLockTransaction::until_done` closure) before
    ///   retrying.
    ///
    /// # Safety
    ///
    /// - Neither this queue's lock nor `current_thread`'s lock may be held upon entry.
    /// - `current_thread` must point to the valid currently executing `Thread`, and `new_owner`
    ///   (if `Some`) must point to a valid live `Thread`.
    #[must_use]
    #[inline]
    pub unsafe fn lock_for_baao_operation_or_backoff(
        &self,
        current_thread: ThreadPtr,
        new_owner: Option<ThreadPtr>,
        _token: &ChainLockTransactionToken,
    ) -> Option<BAAOLockingDetails> {
        let mut details = MaybeUninit::<BAAOLockingDetails>::uninit();
        let new_owner_raw = new_owner.map_or(core::ptr::null_mut(), ThreadPtr::as_raw);
        // SAFETY: `self` and `current_thread` are valid, `new_owner_raw` is valid or null,
        // `details` points to writable stack storage, `_token` proves that a transaction is active
        // (`TA_REQ(chainlock_transaction_token)`), and the caller guarantees the remaining locking
        // preconditions.
        let ok = unsafe {
            cpp_owned_wait_queue_lock_for_baao_operation_or_backoff(
                self.as_raw(),
                current_thread.as_raw(),
                new_owner_raw,
                &raw mut details,
            )
        };
        if ok {
            // SAFETY: `details` was initialized by C++ when returning `true`.
            Some(unsafe { details.assume_init() })
        } else {
            None
        }
    }

    /// Attempts to obtain all of the chain locks needed for a `BlockAndAssignOwner` operation
    /// while already holding this queue's lock.
    ///
    /// # Locking
    ///
    /// - On `Some(details)`, `current_thread.get_lock()` and all of the PI chain locks for the
    ///   owner paths described by `details` have been acquired, in addition to the already-held
    ///   `self.get_lock()`. They must be consumed by passing `details` to
    ///   [`Self::block_and_assign_owner_locked`].
    /// - On `None`, any PI chain locks and `current_thread.get_lock()` acquired by this call have
    ///   been released, but `self.get_lock()` is still held. The caller is responsible for
    ///   releasing it and backing off before retrying.
    ///
    /// # Panics
    ///
    /// Panics if this queue's lock is not held by the transaction identified by `token`.
    ///
    /// # Safety
    ///
    /// - `current_thread`'s lock must not be held upon entry.
    /// - `current_thread` must point to the valid currently executing `Thread`, and `new_owner`
    ///   (if `Some`) must point to a valid live `Thread`.
    #[must_use]
    #[inline]
    pub unsafe fn try_lock_for_baao_operation_locked(
        &self,
        current_thread: ThreadPtr,
        new_owner: Option<ThreadPtr>,
        token: &ChainLockTransactionToken,
    ) -> Option<BAAOLockingDetails> {
        assert!(
            self.get_lock().is_held(token),
            "try_lock_for_baao_operation_locked requires the OwnedWaitQueue's lock to be held"
        );
        let mut details = MaybeUninit::<BAAOLockingDetails>::uninit();
        let new_owner_raw = new_owner.map_or(core::ptr::null_mut(), ThreadPtr::as_raw);
        // SAFETY: `self` and `current_thread` are valid, `new_owner_raw` is valid or null,
        // `details` points to writable stack storage, `token` proves that a transaction is active
        // and the assertion above verified that it holds `self.get_lock()`
        // (`TA_REQ(chainlock_transaction_token, get_lock())`), and the caller guarantees the
        // remaining locking preconditions.
        let ok = unsafe {
            cpp_owned_wait_queue_try_lock_for_baao_operation_locked(
                self.as_raw(),
                current_thread.as_raw(),
                new_owner_raw,
                &raw mut details,
            )
        };
        if ok {
            // SAFETY: `details` was initialized by C++ when returning `true`.
            Some(unsafe { details.assume_init() })
        } else {
            None
        }
    }

    /// Blocks `current_thread` on this wait queue and assigns ownership according to `details`.
    ///
    /// Even if the thread does not end up blocking (e.g. because `deadline` has already expired),
    /// ownership is still assigned as recorded in `details`.
    ///
    /// # Locking
    ///
    /// Releases this wait queue's chain lock and all of the PI chain locks described by
    /// `details`. Upon return (whether successful or not), only `current_thread.get_lock()` is
    /// held, and the caller is responsible for releasing it.
    ///
    /// # Safety
    ///
    /// - The transaction identified by `token` must be finalized, and preemption must be disabled
    ///   (`TA_REQ(preempt_disabled_token)`).
    /// - `details` must have been produced by [`Self::lock_for_baao_operation_or_backoff`] or
    ///   [`Self::try_lock_for_baao_operation_locked`] on `self` with the same `current_thread`
    ///   in the same transaction, and all of the locks they acquired (including
    ///   `self.get_lock()` and `current_thread.get_lock()`) must still be held.
    /// - `current_thread` must point to the valid currently executing `Thread`.
    /// - `self` must remain valid until this call returns, i.e. across the entire block until
    ///   `BlockEtcPostamble` returns. The C++ implementation does not access the queue once the
    ///   thread starts blocking (the waker removes the thread from the queue first, after which
    ///   the queue could otherwise be destroyed), but `&self` must remain dereferenceable for the
    ///   whole duration of the call.
    #[inline]
    pub unsafe fn block_and_assign_owner_locked(
        &self,
        current_thread: ThreadPtr,
        deadline: &Deadline,
        details: BAAOLockingDetails,
        resource_ownership: ResourceOwnership,
        interruptible: Interruptible,
        _token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        // SAFETY: `self`, `current_thread`, `deadline`, and `details` are valid, `_token` proves
        // that a transaction is active (`TA_REQ(chainlock_transaction_token)`), and the caller
        // guarantees that the required BAAO chain locks and preemption state are held.
        let status = unsafe {
            cpp_owned_wait_queue_block_and_assign_owner_locked(
                self.as_raw(),
                current_thread.as_raw(),
                deadline,
                &details,
                resource_ownership,
                interruptible,
            )
        };
        Status::ok(status)
    }

    /// Wakes up to `wake_count` threads from this wait queue, removing any current queue owner in
    /// the process.
    ///
    /// # Panics
    ///
    /// Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is already
    /// active on the current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`). In
    /// particular, this method must not be called from inside a [`WakeRequeueHook`] callback,
    /// which runs inside the method's own transaction with the queue lock held.
    #[inline]
    pub fn wake_threads<H: WakeRequeueHook>(
        &self,
        wake_count: u32,
        hook: &mut H,
    ) -> WakeThreadsResult {
        assert_no_active_transaction("wake_threads");
        let mut woken = 0u32;
        let mut still_waiting = 0u32;
        let mut owner: *mut Thread = core::ptr::null_mut();
        // SAFETY: `self` and `hook` are valid for the duration of the call, the assertion above
        // ensures that no `ChainLockTransaction` is active (and hence that `self.get_lock()` is
        // not held), and C++ `OwnedWaitQueue` returns either null or a pointer to a live `Thread`
        // in `owner`.
        unsafe {
            cpp_owned_wait_queue_wake_threads(
                self.as_raw(),
                wake_count,
                hook_allow_trampoline::<H>,
                hook_on_wake_trampoline::<H>,
                core::ptr::from_mut(hook).cast::<c_void>(),
                &raw mut woken,
                &raw mut still_waiting,
                &raw mut owner,
            );
            WakeThreadsResult { woken, still_waiting, owner: ThreadPtr::from_raw(owner) }
        }
    }

    /// Attempts to wake exactly one thread, and assigns ownership of the queue to the woken thread
    /// (if any threads remain waiting).
    ///
    /// # Panics
    ///
    /// Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is already
    /// active on the current CPU (`TA_EXCL(chainlock_transaction_token, get_lock())`). In
    /// particular, this method must not be called from inside a [`WakeRequeueHook`] callback,
    /// which runs inside the method's own transaction with the queue lock held.
    #[inline]
    pub fn wake_thread_and_assign_owner<H: WakeRequeueHook>(
        &self,
        hook: &mut H,
    ) -> WakeThreadsResult {
        assert_no_active_transaction("wake_thread_and_assign_owner");
        let mut woken = 0u32;
        let mut still_waiting = 0u32;
        let mut owner: *mut Thread = core::ptr::null_mut();
        // SAFETY: `self` and `hook` are valid for the duration of the call, the assertion above
        // ensures that no `ChainLockTransaction` is active (and hence that `self.get_lock()` is
        // not held), and C++ `OwnedWaitQueue` returns either null or a pointer to a live `Thread`
        // in `owner`.
        unsafe {
            cpp_owned_wait_queue_wake_thread_and_assign_owner(
                self.as_raw(),
                hook_allow_trampoline::<H>,
                hook_on_wake_trampoline::<H>,
                core::ptr::from_mut(hook).cast::<c_void>(),
                &raw mut woken,
                &raw mut still_waiting,
                &raw mut owner,
            );
            WakeThreadsResult { woken, still_waiting, owner: ThreadPtr::from_raw(owner) }
        }
    }

    /// Wakes up to `wake_count` threads from `self` and requeues up to `requeue_count` threads
    /// onto `requeue_target`, assigning `new_requeue_owner` as owner of `requeue_target`.
    ///
    /// `new_requeue_owner` (if `Some`) must point to a `Thread` that remains alive for the
    /// duration of the call. C++ `OwnedWaitQueue` checks under the thread's lock whether it is
    /// dying or dead before making it the owner.
    ///
    /// # Panics
    ///
    /// - Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is
    ///   already active on the current CPU
    ///   (`TA_EXCL(chainlock_transaction_token, get_lock(), requeue_target.get_lock())`). In
    ///   particular, this method must not be called from inside a [`WakeRequeueHook`] callback,
    ///   which runs inside the method's own transaction with both queue locks held.
    /// - Panics if `self` and `requeue_target` are the same queue, since both queue locks are
    ///   acquired in the same transaction (`AcquireBothOrBackoff`).
    #[inline]
    #[allow(clippy::too_many_arguments)]
    pub fn wake_and_requeue<WH: WakeRequeueHook, RH: WakeRequeueHook>(
        &self,
        requeue_target: &OwnedWaitQueue,
        new_requeue_owner: Option<ThreadPtr>,
        wake_count: u32,
        requeue_count: u32,
        wake_hooks: &mut WH,
        requeue_hooks: &mut RH,
        wake_option: WakeOption,
    ) -> WakeThreadsResult {
        assert_no_active_transaction("wake_and_requeue");
        assert!(
            !core::ptr::eq(self, requeue_target),
            "wake_and_requeue requires distinct OwnedWaitQueues"
        );
        let mut woken = 0u32;
        let mut still_waiting = 0u32;
        let mut owner: *mut Thread = core::ptr::null_mut();
        let new_requeue_owner_raw =
            new_requeue_owner.map_or(core::ptr::null_mut(), ThreadPtr::as_raw);
        // SAFETY: `self`, `requeue_target`, `wake_hooks`, and `requeue_hooks` are valid for the
        // duration of the call, `new_requeue_owner_raw` is null or was obtained from a
        // `ThreadPtr` (whose constructor requires a live `Thread`), the assertions above ensure
        // that no `ChainLockTransaction` is active (and hence that neither queue lock is held) and
        // that the queues are distinct, and C++ `OwnedWaitQueue` returns either null or a pointer
        // to a live `Thread` in `owner`.
        unsafe {
            cpp_owned_wait_queue_wake_and_requeue(
                self.as_raw(),
                requeue_target.as_raw(),
                new_requeue_owner_raw,
                wake_count,
                requeue_count,
                hook_allow_trampoline::<WH>,
                hook_on_wake_trampoline::<WH>,
                core::ptr::from_mut(wake_hooks).cast::<c_void>(),
                hook_allow_trampoline::<RH>,
                hook_on_wake_trampoline::<RH>,
                core::ptr::from_mut(requeue_hooks).cast::<c_void>(),
                wake_option,
                &raw mut woken,
                &raw mut still_waiting,
                &raw mut owner,
            );
            WakeThreadsResult { woken, still_waiting, owner: ThreadPtr::from_raw(owner) }
        }
    }
}

/// Unit tests for OwnedWaitQueue Rust bindings.
#[cfg(ktest)]
#[unittest::suite(name = "owned_wait_queue_rust")]
mod tests {
    use super::{
        DefaultWakeHooks, Interruptible, OwnedWaitQueue, ResourceOwnership, ThreadPtr, WakeOption,
        WakeRequeueHook, WakeThreadsResult,
    };
    use crate::kconcurrent::{
        Action, ChainLockTransaction, IRQ_SAVE_OPTION, SingleChainLockGuard, TransactionResult,
        clt_tag,
    };
    use crate::kernel::deadline::Deadline;
    use crate::kernel::thread::{self, AnnotatedAutoEagerReschedDisabler};
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_err, assert_true, unwrap_ok};
    use zx_status::Status;

    #[derive(Default)]
    struct CountHook {
        deny: bool,
        allow_count: u32,
        wake_count: u32,
        last_thread: Option<ThreadPtr>,
    }

    impl WakeRequeueHook for CountHook {
        fn allow(&mut self, _thread: ThreadPtr) -> bool {
            self.allow_count += 1;
            !self.deny
        }

        fn on_wake_or_requeue(&mut self, thread: ThreadPtr) {
            self.wake_count += 1;
            self.last_thread = Some(thread);
        }
    }

    const EMPTY_RESULT: WakeThreadsResult =
        WakeThreadsResult { woken: 0, still_waiting: 0, owner: None };

    /// Returns the owner of `owq` as observed while holding its lock.
    fn observe_owner(owq: &OwnedWaitQueue) -> Option<ThreadPtr> {
        SingleChainLockGuard::scope(
            IRQ_SAVE_OPTION,
            owq.get_lock(),
            clt_tag!("owq_rust_test_observe_owner"),
            |guard| owq.owner_locked(guard.token()),
        )
    }

    /// Blocks the current thread in `owq` using `lock_for_baao_operation_or_backoff` and
    /// `block_and_assign_owner_locked`, proposing `new_owner` as the queue's owner.
    fn block_current_thread(
        owq: &OwnedWaitQueue,
        new_owner: Option<ThreadPtr>,
        deadline: Deadline,
    ) -> Result<(), Status> {
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };
        let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_block"),
            |token| -> TransactionResult<Result<(), Status>> {
                // SAFETY: The transaction holds no locks, `current` is the current thread, and
                // `new_owner` (if any) is kept alive by the test.
                let Some(details) =
                    (unsafe { owq.lock_for_baao_operation_or_backoff(current, new_owner, token) })
                else {
                    return Action::Backoff.into();
                };
                ChainLockTransaction::finalize(token);
                // SAFETY: The transaction is finalized, preemption is disabled by
                // `_eager_resched_disabler`, `details` holds the BAAO locks for `owq` and
                // `current`, and `owq` outlives this call.
                let result = unsafe {
                    owq.block_and_assign_owner_locked(
                        current,
                        &deadline,
                        details,
                        ResourceOwnership::Normal,
                        Interruptible::NO,
                        token,
                    )
                };
                // SAFETY: `current` is the valid current thread.
                let current_lock = unsafe { current.get_lock() };
                // `block_and_assign_owner_locked` returns holding only `current`'s lock.
                current_lock.release(token);
                TransactionResult::Value(result)
            },
        )
    }

    struct WaiterContext<'a> {
        owq: &'a OwnedWaitQueue,
        new_owner: Option<ThreadPtr>,
        started: AtomicBool,
    }

    impl<'a> WaiterContext<'a> {
        fn new(owq: &'a OwnedWaitQueue, new_owner: Option<ThreadPtr>) -> Self {
            Self { owq, new_owner, started: AtomicBool::new(false) }
        }
    }

    extern "C" fn waiter_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a `WaiterContext` which outlives this thread, since the test
        // joins the thread before the context is destroyed.
        let ctx = unsafe { &*arg.cast::<WaiterContext<'_>>() };
        ctx.started.store(true, Ordering::SeqCst);
        match block_current_thread(ctx.owq, ctx.new_owner, Deadline::infinite()) {
            Ok(()) => 0,
            Err(status) => status.into_raw(),
        }
    }

    /// Spawns a thread which blocks in `ctx.owq`, and waits until it has blocked.
    ///
    /// # Safety
    ///
    /// The caller must join the returned thread before `ctx` is destroyed.
    unsafe fn spawn_waiter(ctx: &WaiterContext<'_>) -> Result<ThreadPtr, Status> {
        let arg = core::ptr::from_ref(ctx).cast_mut().cast::<c_void>();
        // SAFETY: The caller joins the thread before `ctx` is destroyed.
        let thread = unsafe { thread::spawn(c"owq_rust_waiter".as_ptr(), waiter_entry, arg)? };
        while !ctx.started.load(Ordering::SeqCst) {
            thread::r#yield();
        }
        // Nothing else blocks the waiter between setting `started` and blocking in `ctx.owq`.
        // SAFETY: `thread` has not been joined yet.
        while !unsafe { thread.is_blocked() } {
            thread::r#yield();
        }
        Ok(thread)
    }

    /// Verifies OwnedWaitQueue initialization, lock acquisition, and empty wake/requeue operations.
    #[test]
    fn init_and_empty_operations() {
        stack_pin_init!(let owq1 = OwnedWaitQueue::init());
        stack_pin_init!(let owq2 = OwnedWaitQueue::init());

        owq1.assert_not_owned();
        owq2.assert_not_owned();
        assert_true!(observe_owner(&owq1).is_none());

        owq1.reset_owner_if_no_waiters();

        let mut hook = CountHook::default();
        let res = owq1.wake_threads(1, &mut hook);
        assert_true!(res == EMPTY_RESULT);
        assert_eq!(hook.allow_count, 0);
        assert_eq!(hook.wake_count, 0);

        let res = owq1.wake_thread_and_assign_owner(&mut DefaultWakeHooks);
        assert_true!(res == EMPTY_RESULT);

        let mut requeue_hook = CountHook::default();
        let res = owq1.wake_and_requeue(
            &owq2,
            None,
            1,
            1,
            &mut hook,
            &mut requeue_hook,
            WakeOption::None,
        );
        assert_true!(res == EMPTY_RESULT);
        assert_eq!(hook.wake_count, 0);
        assert_eq!(requeue_hook.allow_count, 0);
        assert_eq!(requeue_hook.wake_count, 0);
    }

    /// Verifies both BAAO locking paths with an expired deadline, which returns `TIMED_OUT`.
    #[test]
    fn baao_with_expired_deadline() {
        stack_pin_init!(let owq = OwnedWaitQueue::init());

        assert_err!(block_current_thread(&owq, None, Deadline::infinite_past()), Status::TIMED_OUT);

        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };
        let deadline = Deadline::infinite_past();
        let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        let result = ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_try_lock"),
            |token| -> TransactionResult<Result<(), Status>> {
                owq.get_lock().acquire_first_in_chain(token);
                // SAFETY: `current`'s lock is not held and `current` is the current thread.
                let Some(details) =
                    (unsafe { owq.try_lock_for_baao_operation_locked(current, None, token) })
                else {
                    // `try_lock_for_baao_operation_locked` leaves `owq.get_lock()` held on
                    // failure.
                    owq.get_lock().release(token);
                    return Action::Backoff.into();
                };
                ChainLockTransaction::finalize(token);
                // SAFETY: The transaction is finalized, preemption is disabled by
                // `_eager_resched_disabler`, `details` holds the BAAO locks for `owq` and
                // `current`, and `owq` outlives this call.
                let result = unsafe {
                    owq.block_and_assign_owner_locked(
                        current,
                        &deadline,
                        details,
                        ResourceOwnership::Normal,
                        Interruptible::NO,
                        token,
                    )
                };
                // SAFETY: `current` is the valid current thread.
                let current_lock = unsafe { current.get_lock() };
                // `block_and_assign_owner_locked` returns holding only `current`'s lock.
                current_lock.release(token);
                TransactionResult::Value(result)
            },
        );
        assert_err!(result, Status::TIMED_OUT);

        assert_true!(observe_owner(&owq).is_none());
        let mut hook = CountHook::default();
        let res = owq.wake_threads(1, &mut hook);
        assert_true!(res == EMPTY_RESULT);
    }

    /// Verifies BAAO blocking with an owner, then denied, assign-owner, and plain wakes.
    #[test]
    fn baao_block_and_wake() {
        stack_pin_init!(let owq = OwnedWaitQueue::init());
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };

        // Block a waiter which declares the test thread to be the owner of the queue.
        let ctx_a = WaiterContext::new(&owq, Some(current));
        // SAFETY: `waiter_a` is joined below, before `ctx_a` is destroyed.
        let waiter_a = unwrap_ok!(unsafe { spawn_waiter(&ctx_a) });
        assert_true!(observe_owner(&owq) == Some(current));

        // A hook which denies every thread leaves the waiter blocked, but still removes the owner.
        let mut deny_hook = CountHook { deny: true, ..Default::default() };
        let res = owq.wake_threads(1, &mut deny_hook);
        assert_true!(res == WakeThreadsResult { woken: 0, still_waiting: 1, owner: None });
        assert_true!(deny_hook.allow_count >= 1);
        assert_eq!(deny_hook.wake_count, 0);
        assert_true!(observe_owner(&owq).is_none());

        // Block a second waiter without an owner.
        let ctx_b = WaiterContext::new(&owq, None);
        // SAFETY: `waiter_b` is joined below, before `ctx_b` is destroyed.
        let waiter_b = unwrap_ok!(unsafe { spawn_waiter(&ctx_b) });

        // Wake one thread and make it the owner, since another thread is still waiting.
        let mut assign_hook = CountHook::default();
        let res = owq.wake_thread_and_assign_owner(&mut assign_hook);
        assert_eq!(res.woken, 1);
        assert_eq!(res.still_waiting, 1);
        assert_true!(assign_hook.allow_count >= 1);
        assert_eq!(assign_hook.wake_count, 1);
        assert_true!(res.owner.is_some());
        assert_true!(res.owner == assign_hook.last_thread);
        assert_true!(res.owner == Some(waiter_a) || res.owner == Some(waiter_b));

        // Wake the remaining thread, which removes any remaining owner.
        let mut wake_hook = CountHook::default();
        let res = owq.wake_threads(1, &mut wake_hook);
        assert_true!(res == WakeThreadsResult { woken: 1, still_waiting: 0, owner: None });
        assert_eq!(wake_hook.wake_count, 1);
        assert_true!(wake_hook.last_thread.is_some());
        assert_true!(wake_hook.last_thread != assign_hook.last_thread);

        // SAFETY: Neither waiter has been joined yet.
        assert_eq!(unwrap_ok!(unsafe { waiter_a.join(InstantMono::INFINITE) }), 0);
        // SAFETY: See above.
        assert_eq!(unwrap_ok!(unsafe { waiter_b.join(InstantMono::INFINITE) }), 0);
        assert_true!(observe_owner(&owq).is_none());
    }

    /// Verifies requeueing a BAAO-blocked thread to an owned queue, then waking it from there.
    #[test]
    fn baao_block_requeue_and_wake() {
        stack_pin_init!(let owq1 = OwnedWaitQueue::init());
        stack_pin_init!(let owq2 = OwnedWaitQueue::init());
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };

        let ctx = WaiterContext::new(&owq1, None);
        // SAFETY: `waiter` is joined below, before `ctx` is destroyed.
        let waiter = unwrap_ok!(unsafe { spawn_waiter(&ctx) });

        let mut wake_hook = CountHook::default();
        let mut requeue_hook = CountHook::default();
        let res = owq1.wake_and_requeue(
            &owq2,
            Some(current),
            0,
            1,
            &mut wake_hook,
            &mut requeue_hook,
            WakeOption::None,
        );
        assert_true!(res == EMPTY_RESULT);
        assert_eq!(wake_hook.wake_count, 0);
        assert_true!(requeue_hook.allow_count >= 1);
        assert_eq!(requeue_hook.wake_count, 1);
        assert_true!(requeue_hook.last_thread == Some(waiter));
        assert_true!(observe_owner(&owq2) == Some(current));

        let mut hook = CountHook::default();
        let res = owq2.wake_threads(1, &mut hook);
        assert_true!(res == WakeThreadsResult { woken: 1, still_waiting: 0, owner: None });
        assert_eq!(hook.wake_count, 1);
        assert_true!(hook.last_thread == Some(waiter));

        // SAFETY: `waiter` has not been joined yet.
        assert_eq!(unwrap_ok!(unsafe { waiter.join(InstantMono::INFINITE) }), 0);
        assert_true!(observe_owner(&owq1).is_none());
        assert_true!(observe_owner(&owq2).is_none());
    }
}
