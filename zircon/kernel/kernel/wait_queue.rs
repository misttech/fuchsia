// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kconcurrent::{
    ChainLockProof, ChainLockTransactionToken, ChainLocked, ClassedChainLock, OwnerGuard,
    assert_lock_not_held, assert_no_transaction_active, assert_transaction_active,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::OWNED_MAGIC;
use crate::kernel::scheduler_state::WaitQueueInheritedSchedulerState;
use crate::kernel::thread::{PreemptDisabledToken, Thread, ThreadLockClass, ThreadPtr};
use crate::kernel::timer::Timer;
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::PhantomPinned;
use core::mem::{align_of, offset_of, size_of};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;
use ksync::KCell;
use pin_init::{PinInit, pin_data, pinned_drop};
use wait_queue_bindings as bindings;
use zr::static_assert;
use zx_status::Status;
use zx_types::zx_status_t;

/// Lock class for data guarded by a wait queue's `ChainLock` (`WaitQueueBase::lock`).
///
/// A marker only: tokens of this class exist solely inside a [`ChainLockProof`] for a
/// [`WaitQueueBase`], [`WaitQueue`], or `OwnedWaitQueue` owner, never via `KMutex`.
pub enum WaitQueueLockClass {}

/// Magic value identifying a standard [`WaitQueue`].
pub const MAGIC: u32 = fbl::magic(b"wait");
static_assert!(MAGIC == bindings::WaitQueue_kMagic);

/// When blocking, this enum indicates the kind of resource ownership that is being waited for
/// that is causing the block.
#[repr(i32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum ResourceOwnership {
    /// Blocking is either not for any particular resource, or it is to wait for
    /// exclusive access to a resource.
    Normal,
    /// Blocking is happening whilst waiting for shared read access to a resource.
    Reader,
}
static_assert!(ResourceOwnership::Normal as i32 == bindings::ResourceOwnership::Normal as i32);
static_assert!(ResourceOwnership::Reader as i32 == bindings::ResourceOwnership::Reader as i32);
static_assert!(size_of::<ResourceOwnership>() == size_of::<bindings::ResourceOwnership>());
static_assert!(align_of::<ResourceOwnership>() == align_of::<bindings::ResourceOwnership>());

impl From<ResourceOwnership> for bindings::ResourceOwnership {
    #[inline]
    fn from(r: ResourceOwnership) -> Self {
        match r {
            ResourceOwnership::Normal => Self::Normal,
            ResourceOwnership::Reader => Self::Reader,
        }
    }
}

/// Whether a block or a sleep can be interrupted.
#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum Interruptible {
    No,
    Yes,
}
static_assert!(Interruptible::No as u8 == bindings::Interruptible::No as u8);
static_assert!(Interruptible::Yes as u8 == bindings::Interruptible::Yes as u8);
static_assert!(size_of::<Interruptible>() == size_of::<bindings::Interruptible>());
static_assert!(align_of::<Interruptible>() == align_of::<bindings::Interruptible>());

impl Interruptible {
    pub const NO: Self = Self::No;
    pub const YES: Self = Self::Yes;

    /// Converts the `Interruptible` setting to a boolean value (`Interruptible::Yes` is `true`).
    #[inline]
    pub const fn as_bool(self) -> bool {
        matches!(self, Self::Yes)
    }
}

impl From<Interruptible> for bool {
    #[inline]
    fn from(i: Interruptible) -> bool {
        i.as_bool()
    }
}

impl From<bool> for Interruptible {
    #[inline]
    fn from(b: bool) -> Interruptible {
        if b { Self::Yes } else { Self::No }
    }
}

impl From<Interruptible> for bindings::Interruptible {
    #[inline]
    fn from(i: Interruptible) -> Self {
        match i {
            Interruptible::No => Self::No,
            Interruptible::Yes => Self::Yes,
        }
    }
}

/// Whether to force base profile inheritance.
#[repr(u8)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub enum ForceInheritance {
    No,
    Yes,
}
static_assert!(ForceInheritance::No as u8 == bindings::ForceInheritance::No as u8);
static_assert!(ForceInheritance::Yes as u8 == bindings::ForceInheritance::Yes as u8);
static_assert!(size_of::<ForceInheritance>() == size_of::<bindings::ForceInheritance>());
static_assert!(align_of::<ForceInheritance>() == align_of::<bindings::ForceInheritance>());

/// Primary key used to sort threads in a [`WaitQueueCollection`].
pub type PrimaryKey = InstantMono;

/// Zero value for [`PrimaryKey`].
pub const PRIMARY_KEY_ZERO: PrimaryKey = InstantMono::ZERO;

/// Composite key `(PrimaryKey, usize)` used to order blocked threads in a [`WaitQueueCollection`].
pub type Key = (PrimaryKey, usize);

/// Tag type for a [`Thread`]'s membership in [`WaitQueueCollection`].
pub(crate) struct BlockedThreadTreeTag;

impl fbl::WavlTreeContainable<Thread, BlockedThreadTreeTag> for Thread {
    type Rank = bool;

    fn get_node(&self) -> &fbl::WavlTreeNode<Thread, Self::Rank> {
        unimplemented!("Thread is currently opaque in Rust")
    }
}

impl fbl::WavlTreeKeyable<Key> for Thread {
    type Key<'a> = Key;

    fn get_key(&self) -> Self::Key<'_> {
        unimplemented!("Thread is currently opaque in Rust")
    }
}

/// Traits for maintaining the minimum relative deadline augmented invariant in a
/// [`WaitQueueCollection`]'s [`BlockedThreadTree`].
pub(crate) struct MinRelativeDeadlineTraits;

impl fbl::WavlTreeAugmentedInvariantObserverTraits for MinRelativeDeadlineTraits {
    type Target = Thread;
    type Value = DurationMono;

    fn get_node_value(_node: &Self::Target) -> Self::Value {
        unimplemented!("Thread is currently opaque in Rust")
    }

    fn get_subtree_value(_node: &Self::Target) -> Self::Value {
        unimplemented!("Thread is currently opaque in Rust")
    }

    fn combine_values(a: Self::Value, b: Self::Value) -> Self::Value {
        if a < b { a } else { b }
    }

    fn set_subtree_value(_node: &mut Self::Target, _val: Self::Value) {
        unimplemented!("Thread is currently opaque in Rust")
    }

    fn reset_subtree_value(_node: &mut Self::Target) {
        unimplemented!("Thread is currently opaque in Rust")
    }
}

/// The WAVL tree of blocked threads in a [`WaitQueueCollection`].
pub(crate) type BlockedThreadTree = fbl::WavlTree<
    Key,
    NonNull<Thread>,
    BlockedThreadTreeTag,
    fbl::TrackingSize,
    fbl::WavlTreeAugmentedInvariantObserver<BlockedThreadTreeTag, MinRelativeDeadlineTraits>,
>;

/// Encapsulation of all the per-thread state for the [`WaitQueueCollection`] data structure,
/// matching C++ `WaitQueueCollection::ThreadState`.
#[repr(C)]
pub struct ThreadState {
    /// If blocked, a pointer to the `WaitQueueBase` the `Thread` is on.
    // Safety invariant: Points to the active `WaitQueueBase` while
    // `blocked_threads_tree_node.in_container()` is true, and is `None` otherwise (including when
    // dropped).
    blocking_wait_queue: KCell<Option<NonNull<WaitQueueBase>>, ThreadLockClass>,
    /// Node state for existing in `WaitQueueCollection::threads`.
    blocked_threads_tree_node: fbl::WavlTreeNode<Thread>,
    /// Primary key used for determining our position in the collection of blocked threads.
    /// Pre-computed during insert in order to save time during insert, rebalance, and search
    /// operations.
    // Safety invariant: Guarded by the enclosing `Thread`'s `ChainLock` (and read under the
    // containing `WaitQueue`'s `ChainLock` while the thread is in `WaitQueueCollection::threads`).
    blocked_threads_tree_sort_key: UnsafeCell<PrimaryKey>,
    /// The minimum relative deadline of this node's subtree, if any.
    // Safety invariant: Guarded by the enclosing `Thread`'s `ChainLock` and the containing
    // `WaitQueue`'s `ChainLock` while the thread is in `WaitQueueCollection::threads`.
    subtree_min_deadline: UnsafeCell<DurationMono>,
    /// Return code if woken up abnormally from suspend, sleep, or block.
    blocked_status: KCell<zx_status_t, ThreadLockClass>,
    /// Are we allowed to be interrupted on the current thing we're blocked/sleeping on?
    interruptible: KCell<Interruptible, ThreadLockClass>,
    /// Storage used by an `OwnedWaitQueue`, but held within a thread instance, while that thread
    /// is blocked in the wait queue.
    inherited_scheduler_state_storage: WaitQueueInheritedSchedulerState,
}

/// Alias for [`ThreadState`].
pub type WaitQueueCollectionThreadState = ThreadState;

// Compile-time layout assertions against the C++ `WaitQueueCollection::ThreadState` type via
// bindgen.
static_assert!(size_of::<ThreadState>() == size_of::<bindings::WaitQueueCollection_ThreadState>());
static_assert!(
    align_of::<ThreadState>() == align_of::<bindings::WaitQueueCollection_ThreadState>()
);
static_assert!(
    offset_of!(ThreadState, blocking_wait_queue)
        == offset_of!(bindings::WaitQueueCollection_ThreadState, blocking_wait_queue_)
);
static_assert!(
    offset_of!(ThreadState, blocked_threads_tree_node)
        == offset_of!(bindings::WaitQueueCollection_ThreadState, blocked_threads_tree_node_)
);
static_assert!(
    offset_of!(ThreadState, blocked_threads_tree_sort_key)
        == offset_of!(bindings::WaitQueueCollection_ThreadState, blocked_threads_tree_sort_key_)
);
static_assert!(
    offset_of!(ThreadState, subtree_min_deadline)
        == offset_of!(bindings::WaitQueueCollection_ThreadState, subtree_min_deadline_)
);
static_assert!(
    offset_of!(ThreadState, blocked_status)
        == offset_of!(bindings::WaitQueueCollection_ThreadState, blocked_status_)
);
static_assert!(
    offset_of!(ThreadState, interruptible)
        == offset_of!(bindings::WaitQueueCollection_ThreadState, interruptible_)
);
static_assert!(
    offset_of!(ThreadState, inherited_scheduler_state_storage)
        == offset_of!(
            bindings::WaitQueueCollection_ThreadState,
            inherited_scheduler_state_storage_
        )
);

impl Drop for ThreadState {
    fn drop(&mut self) {
        debug_assert!(self.blocking_wait_queue.get_inner_mut().is_none());
    }
}

/// The collection of threads currently blocked in a wait queue.
#[pin_data]
#[repr(C)]
pub struct WaitQueueCollection {
    #[pin]
    threads: BlockedThreadTree,
}

// Compile-time layout assertions against the C++ WaitQueueCollection type via bindgen.
static_assert!(size_of::<WaitQueueCollection>() == size_of::<bindings::WaitQueueCollection>());
static_assert!(align_of::<WaitQueueCollection>() == align_of::<bindings::WaitQueueCollection>());
static_assert!(
    offset_of!(WaitQueueCollection, threads) == offset_of!(bindings::WaitQueueCollection, threads_)
);

impl WaitQueueCollection {
    /// Returns the number of threads currently in the collection.
    pub fn count(&self) -> u32 {
        self.threads.len() as u32
    }
}

/// Base type for wait queue types, providing common functionality.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct WaitQueueBase {
    // The lock guarding `collection`; `ClassedChainLock` is `repr(transparent)` over `ChainLock`,
    // so C++ sees a plain `ChainLock` at offset 0.
    lock: ClassedChainLock<WaitQueueLockClass>,
    // Safety invariant: Initialized to `MAGIC` (for `WaitQueue`) or `OWNED_MAGIC` (for the base
    // subobject of `OwnedWaitQueue`), and zeroed during `PinnedDrop::drop`.
    magic: u32,
    // Safety invariant: Must be empty when dropped. Accessed only through `ChainLockProof`
    // accessors on `WaitQueueBase`, `WaitQueue`, and `OwnedWaitQueue`.
    pub(super) collection: KCell<WaitQueueCollection, WaitQueueLockClass>,
    #[pin]
    _pin: PhantomPinned,
}

// Compile-time layout assertions against the C++ WaitQueueBase type via bindgen.
static_assert!(size_of::<WaitQueueBase>() == size_of::<bindings::WaitQueueBase>());
static_assert!(align_of::<WaitQueueBase>() == align_of::<bindings::WaitQueueBase>());
static_assert!(offset_of!(WaitQueueBase, lock) == 0);
static_assert!(offset_of!(WaitQueueBase, magic) == offset_of!(bindings::WaitQueueBase, magic_));
static_assert!(
    offset_of!(WaitQueueBase, collection) == offset_of!(bindings::WaitQueueBase, collection_)
);

// SAFETY: `WaitQueueBase` has no thread-local ownership invariants and is `!Unpin`, so ownership
// of a pinned `WaitQueueBase` can be transferred across threads.
unsafe impl Send for WaitQueueBase {}
// SAFETY: All interior mutations to `WaitQueueBase::collection` through shared references require
// a `ChainLockProof<'_, WaitQueueBase>`, which proves `WaitQueueBase::lock` is held, and are thus
// synchronized by that `ChainLock`.
unsafe impl Sync for WaitQueueBase {}

impl WaitQueueBase {
    /// Returns a reference to this wait queue's lock (the same instance as [`ChainLocked::lock`]).
    ///
    /// Only the safe inspection methods are reachable this way; acquiring and releasing go through
    /// [`OwnerGuard`](crate::kconcurrent::OwnerGuard) or the `unsafe`
    /// [`ClassedChainLock::as_chain_lock`].
    #[inline]
    pub fn get_lock(&self) -> &ClassedChainLock<WaitQueueLockClass> {
        &self.lock
    }
}

// SAFETY: `lock` guards `collection`, the only `KCell<_, WaitQueueLockClass>` field, and is a
// plain field so the same instance is returned every time.
unsafe impl ChainLocked for WaitQueueBase {
    type Class = WaitQueueLockClass;

    /// Returns the lock guarding this wait queue.
    fn lock(&self) -> &ClassedChainLock<WaitQueueLockClass> {
        &self.lock
    }
}

/// Accessors and locked operations for a [`WaitQueueBase`] guarded by its lock.
impl ChainLockProof<'_, WaitQueueBase> {
    /// Returns the collection of blocked threads.
    pub(crate) fn collection(&self) -> &WaitQueueCollection {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `collection`.
        unsafe { self.owner().collection.get(self.token()) }
    }

    /// Returns the collection of blocked threads mutably.
    ///
    /// # Safety
    ///
    /// If `self.owner()` is the base subobject of an
    /// [`OwnedWaitQueue`](crate::kernel::owned_wait_queue::OwnedWaitQueue)
    /// (`self.owner().magic() == OWNED_MAGIC`), the caller must maintain the `OwnedWaitQueue`
    /// safety invariants between `collection` and `inherited_scheduler_state_storage`.
    pub(crate) unsafe fn collection_mut(&mut self) -> &mut WaitQueueCollection {
        let (owner, token) = self.split_mut();
        // SAFETY: As in `collection`; the unique borrow of `self` ensures no other reference to
        // `collection` is live.
        unsafe { owner.collection.get_mut(token) }
    }

    /// Returns `true` if no threads are currently blocked in this wait queue.
    #[inline]
    pub fn is_empty(&self) -> bool {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction.
        unsafe { bindings::cpp_wait_queue_base_is_empty(self.owner().as_raw()) }
    }

    /// Returns the number of threads currently blocked in this wait queue.
    #[inline]
    pub fn count(&self) -> u32 {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction.
        unsafe { bindings::cpp_wait_queue_base_count(self.owner().as_raw()) }
    }

    /// Peeks at the first [`Thread`] in the wait queue collection.
    #[inline]
    pub fn peek_front(&self) -> Option<ThreadPtr> {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction. By the
        // wait queue collection invariant, every queued node belongs to a live `Thread`.
        unsafe {
            ThreadPtr::from_raw(
                bindings::cpp_wait_queue_base_peek_front(self.owner().as_raw()).cast(),
            )
        }
    }

    /// Validates the internal invariants of this wait queue.
    #[inline]
    pub fn validate_queue(&self) {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction.
        unsafe {
            bindings::cpp_wait_queue_base_validate_queue(self.owner().as_raw());
        }
    }

    /// Performs the preamble of blocking `current_thread` on this wait queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] in the
    ///   `THREAD_RUNNING` state and is not already queued in any wait queue.
    /// - If `self.owner().magic() == OWNED_MAGIC`, `self.owner()` is the base subobject of a live
    ///   [`OwnedWaitQueue`](crate::kernel::owned_wait_queue::OwnedWaitQueue) with provenance over
    ///   the full `OwnedWaitQueue`.
    /// - If this method returns `Ok(())`, the caller must complete the block operation by calling
    ///   [`WaitQueueBase::block_etc_postamble`] before dropping the wait queue or unlocking
    ///   `current_thread`.
    pub unsafe fn block_etc_preamble(
        &mut self,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        signal_mask: u32,
        reason: ResourceOwnership,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        // SAFETY: `self` and `current_thread` prove both locks are held, `deadline` is valid, and
        // the caller upholds the blocking preamble preconditions.
        let status = unsafe {
            bindings::cpp_wait_queue_base_block_etc_preamble(
                self.owner().as_raw(),
                current_thread.owner().as_raw().cast(),
                core::ptr::from_ref(deadline).cast(),
                signal_mask,
                reason.into(),
                interruptible.into(),
            )
        };
        Status::ok(status)
    }

    /// Dequeues the specified thread `t` from this wait queue and sets its blocked status to
    /// `wait_queue_error`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t.owner()` is currently blocked in `self.owner()`.
    pub unsafe fn dequeue(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
        wait_queue_error: impl Into<Result<(), Status>>,
    ) {
        // SAFETY: `self` and `t` prove both locks are held, and the caller guarantees `t.owner()`
        // is blocked in `self.owner()`.
        unsafe {
            bindings::cpp_wait_queue_base_dequeue(
                self.owner().as_raw(),
                t.owner().as_raw().cast(),
                Status::result_into_raw(wait_queue_error.into()),
            );
        }
    }

    /// Recomputes the effective profile of a thread `t` which is known to be blocked in this wait
    /// queue, reordering the thread in the queue collection as needed.
    ///
    /// This method does not deal with the consequences of profile inheritance, and should only
    /// ever be called from the scheduler's PI update code.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t.owner()` is currently blocked in `self.owner()`.
    pub unsafe fn update_blocked_thread_effective_profile(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
    ) {
        // SAFETY: `self` and `t` prove both locks are held, and the caller guarantees `t.owner()`
        // is currently blocked in `self.owner()`.
        unsafe {
            bindings::cpp_wait_queue_base_update_blocked_thread_effective_profile(
                self.owner().as_raw(),
                t.owner().as_raw().cast(),
            );
        }
    }
}

impl WaitQueueBase {
    /// In-place pinned initializer for [`WaitQueueBase`] with the specified `magic`.
    ///
    /// # Safety
    ///
    /// `magic` must either be [`MAGIC`] (for a [`WaitQueue`]), or [`OWNED_MAGIC`] when
    /// initializing the base `WaitQueueBase` subobject of a valid
    /// [`OwnedWaitQueue`](crate::kernel::owned_wait_queue::OwnedWaitQueue).
    pub unsafe fn init_with_magic(magic: u32) -> impl PinInit<Self, Infallible> {
        // SAFETY: `bindings::cpp_wait_queue_base_init` placement-new initializes a valid
        // `WaitQueueBase` in-place in the provided slot, and the caller guarantees `magic`
        // satisfies the field safety invariant.
        zr::pin_init_ffi!(bindings::cpp_wait_queue_base_init, magic)
    }

    /// Domain-specific conversion: returns raw pointer for [`WaitQueueBase`].
    #[inline]
    pub fn as_raw(&self) -> *mut bindings::WaitQueueBase {
        (self as *const Self).cast_mut().cast()
    }

    /// Returns a raw mutable pointer to the underlying C++ `WaitQueueBase`.
    #[inline]
    pub fn as_ptr(&self) -> *mut c_void {
        self.as_raw().cast()
    }

    /// Returns the magic value of this wait queue.
    #[inline]
    pub fn magic(&self) -> u32 {
        // SAFETY: `self.as_raw()` is a valid pointer to a live `WaitQueueBase`.
        unsafe { bindings::cpp_wait_queue_base_magic(self.as_raw()) }
    }

    /// Removes a specific thread out of the wait queue it's blocked on, and deals with any PI side
    /// effects. Note: when calling this function:
    ///
    /// 1) The thread `t` must be actively blocked in the `WaitQueue` instance indicated by
    ///    `guard`.
    /// 2) In addition to `t`'s lock, all of the `ChainLock`s downstream of `t` (starting from `t`
    ///    and ending at the target of `t`'s PI graph) must be held. Note that we are only able to
    ///    statically assert the first two of these locks; `t`'s lock, and the lock of the wait
    ///    queue `t` is currently blocked in.
    /// 3) During the call to `unblock_thread`, all of the locks identified in #2 (above) will be
    ///    released.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `t_guard.owner()` is actively blocked in `guard.owner()`.
    /// - The active transaction is finalized, and all `ChainLock`s downstream of
    ///   `t_guard.owner()` in its PI graph (in addition to `guard.owner().lock()` and
    ///   `t_guard.owner().lock()`, which are consumed via `guard` and `t_guard`) are held by the
    ///   active transaction and are not unlocked again by the caller after this call releases
    ///   them.
    /// - If `guard.owner().magic() == OWNED_MAGIC`, `guard.owner()` is the base subobject of a
    ///   live [`OwnedWaitQueue`](crate::kernel::owned_wait_queue::OwnedWaitQueue) with provenance
    ///   over the full `OwnedWaitQueue`.
    pub unsafe fn unblock_thread(
        guard: OwnerGuard<'_, Self>,
        t_guard: OwnerGuard<'_, Thread>,
        wait_queue_error: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let wait_queue = guard.into_unguarded();
        let t = t_guard.into_unguarded();
        // SAFETY: `wait_queue.as_raw()` and `t` are valid, their locks are held via the consumed
        // `guard` and `t_guard`, the active transaction and preemption-disabled states are
        // verified, and the caller upholds the PI-graph and `OwnedWaitQueue` preconditions.
        let status = unsafe {
            bindings::cpp_wait_queue_base_unblock_thread(
                wait_queue.as_raw(),
                t.as_raw().cast(),
                Status::result_into_raw(wait_queue_error.into()),
            )
        };
        Status::ok(status)
    }

    /// Performs the postamble of blocking `current_thread` on this wait queue after the wait
    /// queue's lock has been released.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] on the current CPU,
    ///   and the CPU is in a context where blocking is permitted (no spinlocks held and blocking
    ///   not disallowed).
    /// - [`ChainLockProof::block_etc_preamble`] succeeded for `current_thread.owner()` on `self`,
    ///   `self.lock()` has been released, and the active transaction is finalized with
    ///   `current_thread.owner().lock()` as the only held chain lock.
    /// - If `deadline` is not infinite, it uses the monotonic timeline.
    /// - `self` remains valid and unaliased by any `&mut` reference until this call returns.
    pub unsafe fn block_etc_postamble(
        &self,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        assert_lock_not_held(self.lock());
        // SAFETY: `self.as_raw()`, `current_thread.owner()`, and `deadline` are valid,
        // `current_thread` proves the thread's lock is held, `self.lock()` is verified not held,
        // and the caller upholds the blocking postamble preconditions.
        let status = unsafe {
            bindings::cpp_wait_queue_base_block_etc_postamble(
                self.as_raw(),
                current_thread.owner().as_raw().cast(),
                core::ptr::from_ref(deadline).cast(),
            )
        };
        Status::ok(status)
    }

    /// Timer callback invoked when a blocked thread's wait deadline expires.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `timer` points to a valid [`Timer`] in an active timer
    /// callback context, `arg` points to the valid, live [`Thread`] associated with the wait
    /// timeout, and no `ChainLockTransaction` is currently active on the current CPU.
    pub unsafe fn timeout_handler(timer: *mut Timer, now: InstantMono, arg: *mut c_void) {
        // SAFETY: The caller guarantees `timer` and `arg` are valid for `WaitQueueBase`'s timeout
        // handler and no transaction is active.
        unsafe {
            bindings::cpp_wait_queue_base_timeout_handler(timer.cast(), now.0, arg);
        }
    }
}

#[pinned_drop]
impl PinnedDrop for WaitQueueBase {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: We do not move `self` or any pinned fields out of `this`.
        let this = unsafe { self.get_unchecked_mut() };
        debug_assert!(
            this.magic == MAGIC || this.magic == OWNED_MAGIC,
            "magic {:#010x}",
            this.magic
        );

        let count = this.collection.get_inner_mut().count();
        if count != 0 {
            panic!(
                "~WaitQueue() called on non-empty WaitQueue, count={}, magic={:#010x}",
                count, this.magic
            );
        }

        this.magic = 0;
    }
}

/// A basic wait queue for blocking and unblocking threads without resource ownership semantics.
#[pin_data]
#[repr(C)]
pub struct WaitQueue {
    // Safety invariant: Initialized as the `WaitQueueBase` subobject of this `WaitQueue` with
    // `magic == MAGIC`.
    #[pin]
    base: WaitQueueBase,
}

// Compile-time layout assertions against the C++ WaitQueue type via bindgen.
static_assert!(size_of::<WaitQueue>() == size_of::<bindings::WaitQueue>());
static_assert!(align_of::<WaitQueue>() == align_of::<bindings::WaitQueue>());
static_assert!(offset_of!(WaitQueue, base) == offset_of!(bindings::WaitQueue, _base));

// SAFETY: `WaitQueue` has no thread-local ownership invariants and is `!Unpin`, so ownership of a
// pinned `WaitQueue` can be transferred across threads.
unsafe impl Send for WaitQueue {}
// SAFETY: All interior mutations to `WaitQueue` through shared references are synchronized by the
// embedded `WaitQueueBase`'s `ChainLock`.
unsafe impl Sync for WaitQueue {}

impl Deref for WaitQueue {
    type Target = WaitQueueBase;

    #[inline]
    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

// SAFETY: `base.lock` guards `base.collection`, and `base` is a plain field so the same lock
// instance is returned every time.
unsafe impl ChainLocked for WaitQueue {
    type Class = WaitQueueLockClass;

    /// Returns the lock guarding this wait queue (the embedded `WaitQueueBase`'s).
    fn lock(&self) -> &ClassedChainLock<WaitQueueLockClass> {
        self.base.lock()
    }
}

/// Accessors and locked operations for a [`WaitQueue`] guarded by its lock.
impl ChainLockProof<'_, WaitQueue> {
    /// Re-expresses this proof as one for the embedded [`WaitQueueBase`] (same lock instance).
    /// `self` is unusable while the returned proof lives.
    pub(crate) fn as_base(&mut self) -> ChainLockProof<'_, WaitQueueBase> {
        let base = &self.owner().base;
        self.retag(base)
    }

    /// Returns the collection of blocked threads.
    pub(crate) fn collection(&self) -> &WaitQueueCollection {
        // SAFETY: `self` proves `owner().lock()` (which is `owner().base.lock()`) is held, and
        // that lock guards `base.collection`.
        unsafe { self.owner().base.collection.get(self.token()) }
    }

    /// Returns the collection of blocked threads mutably.
    pub(crate) fn collection_mut(&mut self) -> &mut WaitQueueCollection {
        let (queue, token) = self.split_mut();
        // SAFETY: As in `collection`; the unique borrow of `self` ensures no other reference to
        // `base.collection` is live.
        unsafe { queue.base.collection.get_mut(token) }
    }

    /// Returns `true` if no threads are currently blocked in this wait queue.
    #[inline]
    pub fn is_empty(&self) -> bool {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction.
        unsafe { bindings::cpp_wait_queue_base_is_empty(self.owner().base.as_raw()) }
    }

    /// Returns the number of threads currently blocked in this wait queue.
    #[inline]
    pub fn count(&self) -> u32 {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction.
        unsafe { bindings::cpp_wait_queue_base_count(self.owner().base.as_raw()) }
    }

    /// Peeks at the first [`Thread`] in the wait queue collection.
    #[inline]
    pub fn peek_front(&self) -> Option<ThreadPtr> {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction. By the
        // wait queue collection invariant, every queued node belongs to a live `Thread`.
        unsafe {
            ThreadPtr::from_raw(
                bindings::cpp_wait_queue_base_peek_front(self.owner().base.as_raw()).cast(),
            )
        }
    }

    /// Validates the internal invariants of this wait queue.
    #[inline]
    pub fn validate_queue(&self) {
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction.
        unsafe {
            bindings::cpp_wait_queue_base_validate_queue(self.owner().base.as_raw());
        }
    }

    /// Locked version of [`WaitQueue::wake_one`].
    ///
    /// These calls are going to need to obtain locks for each of the threads woken, which could
    /// result in needing to back off and start the operation again. Each routine returns an
    /// `Option` (`bool` or `u32`).
    ///
    /// ++ If the `Option` holds `Some(woken)`, then `woken` indicates whether a thread was woken,
    ///    and the operation succeeded.
    /// ++ Otherwise (`None`), the operation failed with a Backoff error and the queue's lock needs
    ///    to be dropped before trying again.
    ///
    /// The active [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) must not
    /// already be finalized on entry; when a thread is woken (`Some(true)`), this routine
    /// finalizes the active transaction so the caller cannot acquire additional `ChainLock`s in
    /// the same transaction afterward.
    ///
    /// It is assumed that forming a lock cycle should be impossible. If such a cycle is detected
    /// (like, if the caller was holding the lock of one of the threads blocked in this queue at
    /// the time of the call) it will trigger a `DEBUG_ASSERT`.
    ///
    /// Either way, unlike the non-locked versions of these routines, the wait queue's lock is held
    /// for the duration of the operation, instead of being released as soon as possible (during
    /// the call to `Scheduler::Unlock`).
    #[inline]
    pub fn wake_one_locked(
        &mut self,
        wait_queue_error: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<bool> {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let mut woken = false;
        // SAFETY: `self` proves `self.owner().lock()` is exclusively held by the active
        // transaction, and the transaction and preemption-disabled tokens are verified.
        let ok = unsafe {
            bindings::cpp_wait_queue_wake_one_locked(
                self.owner().as_raw(),
                Status::result_into_raw(wait_queue_error.into()),
                &mut woken,
            )
        };
        ok.then_some(woken)
    }

    /// Locked version of [`WaitQueue::wake_all`].
    ///
    /// These calls are going to need to obtain locks for each of the threads woken, which could
    /// result in needing to back off and start the operation again. Each routine returns an
    /// `Option` (`bool` or `u32`).
    ///
    /// ++ If the `Option` holds `Some(count)`, then `count` is the number of threads which were
    ///    woken, and the operation succeeded.
    /// ++ Otherwise (`None`), the operation failed with a Backoff error and the queue's lock needs
    ///    to be dropped before trying again.
    ///
    /// The active [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) must not
    /// already be finalized on entry; when this routine succeeds (`Some(_)`), it finalizes the
    /// active transaction so the caller cannot acquire additional `ChainLock`s in the same
    /// transaction afterward.
    ///
    /// It is assumed that forming a lock cycle should be impossible. If such a cycle is detected
    /// (like, if the caller was holding the lock of one of the threads blocked in this queue at
    /// the time of the call) it will trigger a `DEBUG_ASSERT`.
    ///
    /// Either way, unlike the non-locked versions of these routines, the wait queue's lock is held
    /// for the duration of the operation, instead of being released as soon as possible (during
    /// the call to `Scheduler::Unlock`).
    #[inline]
    pub fn wake_all_locked(
        &mut self,
        wait_queue_error: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<u32> {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let mut woken = 0u32;
        // SAFETY: `self` proves `self.owner().lock()` is exclusively held by the active
        // transaction, and the transaction and preemption-disabled tokens are verified.
        let ok = unsafe {
            bindings::cpp_wait_queue_wake_all_locked(
                self.owner().as_raw(),
                Status::result_into_raw(wait_queue_error.into()),
                &mut woken,
            )
        };
        ok.then_some(woken)
    }

    /// Dequeues the specified thread `t` and sets its `blocked_status`. Does not actually schedule
    /// the thread to run.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t.owner()` is currently blocked in `self.owner()`.
    pub unsafe fn dequeue_thread(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
        wait_queue_error: impl Into<Result<(), Status>>,
    ) {
        // SAFETY: `self` and `t` prove both locks are held, and the caller guarantees `t.owner()`
        // is blocked in `self.owner()`.
        unsafe {
            bindings::cpp_wait_queue_dequeue_thread(
                self.owner().as_raw(),
                t.owner().as_raw().cast(),
                Status::result_into_raw(wait_queue_error.into()),
            );
        }
    }
}

impl WaitQueue {
    pub const MAGIC: u32 = MAGIC;

    /// In-place pinned initializer for `WaitQueue`.
    pub fn init() -> impl PinInit<Self, Infallible> {
        // SAFETY: `bindings::cpp_wait_queue_init` placement-new initializes a valid `WaitQueue`
        // in-place in the provided slot.
        zr::pin_init_ffi!(bindings::cpp_wait_queue_init)
    }

    /// Domain-specific conversion: returns raw pointer for `WaitQueue`.
    #[inline]
    pub fn as_raw(&self) -> *mut bindings::WaitQueue {
        (self as *const Self).cast_mut().cast()
    }

    /// Returns a raw mutable pointer to the underlying C++ `WaitQueue`.
    #[inline]
    pub fn as_ptr(&self) -> *mut c_void {
        self.as_raw().cast()
    }

    /// Blocks `current_thread` on this wait queue.
    ///
    /// The returned status is whatever the caller of `WaitQueue::wake_*()` specifies.
    /// A deadline other than `Deadline::infinite()` will abort at the specified time
    /// and return `Status::TIMED_OUT`. A deadline in the past will immediately return.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] on the current CPU
    ///   in the `THREAD_RUNNING` state, is not already queued in any wait queue, and the CPU is in
    ///   a context where blocking is permitted (no spinlocks held and blocking not disallowed).
    /// - The active transaction is finalized with `guard.owner().lock()` and
    ///   `current_thread.owner().lock()` as the only held chain locks (`guard.owner().lock()` is
    ///   released by this call, consuming `guard`).
    /// - If `deadline` is not infinite, it uses the monotonic timeline.
    /// - `guard.owner()` remains valid and unaliased by any `&mut` reference until this call
    ///   returns.
    pub unsafe fn block(
        guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        interruptible: Interruptible,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        let wait_queue = guard.into_unguarded();
        // SAFETY: `wait_queue.as_raw()`, `current_thread.owner()`, and `deadline` are valid, the
        // queue lock is held via the consumed `guard` and the thread lock via `current_thread`,
        // and the caller upholds the blocking preconditions.
        let status = unsafe {
            bindings::cpp_wait_queue_block(
                wait_queue.as_raw(),
                current_thread.owner().as_raw().cast(),
                core::ptr::from_ref(deadline).cast(),
                interruptible.into(),
            )
        };
        Status::ok(status)
    }

    /// Blocks `current_thread` on this wait queue with an [`InstantMono`]-typed deadline.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] on the current CPU
    ///   in the `THREAD_RUNNING` state, is not already queued in any wait queue, and the CPU is in
    ///   a context where blocking is permitted (no spinlocks held and blocking not disallowed).
    /// - The active transaction is finalized with `guard.owner().lock()` and
    ///   `current_thread.owner().lock()` as the only held chain locks (`guard.owner().lock()` is
    ///   released by this call, consuming `guard`).
    /// - `guard.owner()` remains valid and unaliased by any `&mut` reference until this call
    ///   returns.
    pub unsafe fn block_instant(
        guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: InstantMono,
        interruptible: Interruptible,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        let wait_queue = guard.into_unguarded();
        // SAFETY: `wait_queue.as_raw()` and `current_thread.owner()` are valid, the queue lock is
        // held via the consumed `guard` and the thread lock via `current_thread`, and the caller
        // upholds the blocking preconditions.
        let status = unsafe {
            bindings::cpp_wait_queue_block_instant(
                wait_queue.as_raw(),
                current_thread.owner().as_raw().cast(),
                deadline.0,
                interruptible.into(),
            )
        };
        Status::ok(status)
    }

    /// Blocks `current_thread` on this wait queue, ignoring existing signals in `signal_mask`.
    ///
    /// The returned status is whatever the caller of `WaitQueue::wake_*()` specifies, or
    /// `Status::TIMED_OUT` if the deadline has elapsed or is in the past.
    /// This will never timeout when called with a deadline of `Deadline::infinite()`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] on the current CPU
    ///   in the `THREAD_RUNNING` state, is not already queued in any wait queue, and the CPU is in
    ///   a context where blocking is permitted (no spinlocks held and blocking not disallowed).
    /// - The active transaction is finalized with `guard.owner().lock()` and
    ///   `current_thread.owner().lock()` as the only held chain locks (`guard.owner().lock()` is
    ///   released by this call, consuming `guard`).
    /// - If `deadline` is not infinite, it uses the monotonic timeline.
    /// - `guard.owner()` remains valid and unaliased by any `&mut` reference until this call
    ///   returns.
    pub unsafe fn block_etc(
        guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        signal_mask: u32,
        reason: ResourceOwnership,
        interruptible: Interruptible,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        let wait_queue = guard.into_unguarded();
        // SAFETY: `wait_queue.as_raw()`, `current_thread.owner()`, and `deadline` are valid, the
        // queue lock is held via the consumed `guard` and the thread lock via `current_thread`,
        // and the caller upholds the blocking preconditions.
        let status = unsafe {
            bindings::cpp_wait_queue_block_etc(
                wait_queue.as_raw(),
                current_thread.owner().as_raw().cast(),
                core::ptr::from_ref(deadline).cast(),
                signal_mask,
                reason.into(),
                interruptible.into(),
            )
        };
        Status::ok(status)
    }

    /// Releases one thread from the wait queue.
    ///
    /// `wait_queue_error` is what [`WaitQueue::block`] should return for the blocking thread.
    ///
    /// Returns `true` if a thread was woken, and `false` otherwise.
    #[inline]
    pub fn wake_one(&self, wait_queue_error: impl Into<Result<(), Status>>) -> bool {
        assert_no_transaction_active();
        assert_lock_not_held(self.lock());
        // SAFETY: `self.as_raw()` is valid and no chainlock transaction or queue lock is held.
        unsafe {
            bindings::cpp_wait_queue_wake_one(
                self.as_raw(),
                Status::result_into_raw(wait_queue_error.into()),
            )
        }
    }

    /// Releases all threads from the wait queue.
    ///
    /// `wait_queue_error` is what [`WaitQueue::block`] should return for the blocking threads.
    #[inline]
    pub fn wake_all(&self, wait_queue_error: impl Into<Result<(), Status>>) {
        assert_no_transaction_active();
        assert_lock_not_held(self.lock());
        // SAFETY: `self.as_raw()` is valid and no chainlock transaction or queue lock is held.
        unsafe {
            bindings::cpp_wait_queue_wake_all(
                self.as_raw(),
                Status::result_into_raw(wait_queue_error.into()),
            );
        }
    }

    /// Moves the specified thread `t` from the `source` wait queue to the `dest` wait queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `source.owner()` and `dest.owner()` are distinct wait queues
    /// and that `t.owner()` is currently blocked in `source.owner()`.
    pub unsafe fn move_thread(
        source: &mut ChainLockProof<'_, Self>,
        dest: &mut ChainLockProof<'_, Self>,
        t: &mut ChainLockProof<'_, Thread>,
    ) {
        debug_assert!(!core::ptr::eq(source.owner(), dest.owner()));
        // SAFETY: `source`, `dest`, and `t` prove all three locks are held, and the caller
        // guarantees `source` and `dest` are distinct and `t.owner()` is blocked in
        // `source.owner()`.
        unsafe {
            bindings::cpp_wait_queue_move_thread(
                source.owner().as_raw(),
                dest.owner().as_raw(),
                t.owner().as_raw().cast(),
            );
        }
    }
}

/// [`WaitQueue`] and [`WaitQueueBase`] unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "wait_queue")]
mod tests {
    use unittest::{expect_eq, expect_false, expect_true};

    use crate::kconcurrent::{
        ChainLockTransaction, DONE, IRQ_SAVE_OPTION, OwnerGuard,
        PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned,
    };

    /// Verifies `WaitQueue` and `WaitQueueBase` lock proofs, accessors, and empty ops.
    #[test]
    fn wait_queue_proof_and_base_retag() {
        pin_init::stack_pin_init!(let wq = WaitQueue::init());
        let wq_ref: &WaitQueue = &wq;
        expect_true!(wq_ref.lock().is_unlocked());
        expect_eq!(wq_ref.magic(), MAGIC);

        expect_false!(wq_ref.wake_one(Ok(())));
        wq_ref.wake_all(Ok(()));

        {
            single_chain_lock_guard_owned!(
                scope,
                wq_ref,
                IRQ_SAVE_OPTION,
                clt_tag!("wait_queue_scope")
            );
            expect_true!(wq_ref.lock().is_held(scope.token()));
            let mut proof = scope.proof();
            expect_eq!(proof.collection().count(), 0);
            expect_eq!(proof.collection_mut().count(), 0);
            expect_true!(proof.is_empty());
            expect_eq!(proof.count(), 0);
            expect_true!(proof.peek_front().is_none());
            proof.validate_queue();
            {
                let mut base_proof = proof.as_base();
                expect_true!(core::ptr::eq(base_proof.owner(), &wq_ref.base));
                expect_eq!(base_proof.collection().count(), 0);
                // SAFETY: `wq_ref.base` is the base of a standalone `WaitQueue` (not an
                // `OwnedWaitQueue`), and `.count()` does not mutate the collection.
                expect_eq!(unsafe { base_proof.collection_mut() }.count(), 0);
                expect_true!(base_proof.is_empty());
                expect_eq!(base_proof.count(), 0);
                expect_true!(base_proof.peek_front().is_none());
                base_proof.validate_queue();
            }
        }
        expect_true!(wq_ref.lock().is_unlocked());

        let mut base_count = u32::MAX;
        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("wait_queue_base"), |token| {
            let mut guard = OwnerGuard::new(&wq_ref.base, token);
            ChainLockTransaction::finalize(token);
            let mut proof = guard.proof();
            // SAFETY: `wq_ref.base` is the base of a standalone `WaitQueue` (not an
            // `OwnedWaitQueue`), and `.count()` does not mutate the collection.
            base_count = unsafe { proof.collection_mut() }.count();
            guard.release();
            DONE
        });
        expect_eq!(base_count, 0);
        expect_true!(wq_ref.lock().is_unlocked());

        let mut wake_one_res = None;
        let mut wake_all_res = None;
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("wait_queue_wake_locked"),
            |token| {
                // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the
                // duration of this transaction.
                let preempt_disabled = unsafe { PreemptDisabledToken::new() };
                let mut guard = OwnerGuard::new(wq_ref, token);
                let mut proof = guard.proof();
                wake_one_res = proof.wake_one_locked(Ok(()), token, &preempt_disabled);
                wake_all_res = proof.wake_all_locked(Ok(()), token, &preempt_disabled);
                guard.release();
                DONE
            },
        );
        expect_true!(wake_one_res == Some(false));
        expect_true!(wake_all_res == Some(0));
        expect_true!(wq_ref.lock().is_unlocked());
    }
}
