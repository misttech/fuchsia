// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kconcurrent::{
    Action, ChainLockProof, ChainLockResult, ChainLockTransaction, ChainLockTransactionToken,
    ChainLocked, ClassedChainLock, DONE, OwnerGuard, PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
    TransactionResult, assert_lock_not_held, assert_no_transaction_active,
    assert_transaction_active, clt_tag,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::{OWNED_MAGIC, OwnedWaitQueue, ThreadUnblockList};
use crate::kernel::scheduler::{self, SchedDuration, WaitQueueInheritedSchedulerState};
use crate::kernel::scheduler_state::ThreadStateKind;
use crate::kernel::thread::{PreemptDisabledToken, Thread, ThreadLockClass, ThreadPtr};
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::PhantomPinned;
use core::mem::{align_of, offset_of, size_of};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;
use ksync::{KCell, LockToken};
use pin_init::{PinInit, pin_data, pinned_drop};
use wait_queue_bindings as bindings;
use zr::static_assert;
use zx_status::Status;
use zx_types::{zx_instant_mono_t, zx_status_t};

/// Add expensive code to do a full validation of the wait queue at various entry points
/// to this module.
const WAIT_QUEUE_VALIDATION: bool = cfg!(lk_debuglevel_gt2);

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

impl From<bindings::ResourceOwnership> for ResourceOwnership {
    #[inline]
    fn from(r: bindings::ResourceOwnership) -> Self {
        match r {
            bindings::ResourceOwnership::Normal => Self::Normal,
            bindings::ResourceOwnership::Reader => Self::Reader,
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

impl From<bindings::Interruptible> for Interruptible {
    #[inline]
    fn from(i: bindings::Interruptible) -> Self {
        match i {
            bindings::Interruptible::No => Self::No,
            bindings::Interruptible::Yes => Self::Yes,
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
    // dropped). If the pointed-to queue's magic is `OWNED_MAGIC`, the pointer carries provenance
    // over the enclosing `OwnedWaitQueue` (unsafe code downcasts it). Only written through the
    // `unsafe` `set_blocking_wait_queue`.
    blocking_wait_queue: KCell<Option<NonNull<WaitQueueBase>>, ThreadLockClass>,
    /// Node state for existing in `WaitQueueCollection::threads`.
    blocked_threads_tree_node: fbl::WavlTreeNode<Thread>,
    /// Primary key used for determining our position in the collection of blocked threads.
    /// Pre-computed during insert in order to save time during insert, rebalance, and search
    /// operations.
    // Safety invariant: Guarded by the enclosing `Thread`'s `ChainLock` (and read under the
    // containing `WaitQueue`'s `ChainLock` while the thread is in `WaitQueueCollection::threads`).
    // In debug builds, equals `PRIMARY_KEY_ZERO` whenever
    // `!blocked_threads_tree_node.in_container()`.
    blocked_threads_tree_sort_key: UnsafeCell<PrimaryKey>,
    /// The minimum relative deadline of this node's subtree, if any.
    // Safety invariant: Guarded by the enclosing `Thread`'s `ChainLock` and the containing
    // `WaitQueue`'s `ChainLock` while the thread is in `WaitQueueCollection::threads`, and reset
    // to `DurationMono::MAX` whenever `!blocked_threads_tree_node.in_container()`.
    subtree_min_deadline: UnsafeCell<DurationMono>,
    /// Return code if woken up abnormally from suspend, sleep, or block.
    blocked_status: KCell<zx_status_t, ThreadLockClass>,
    /// Are we allowed to be interrupted on the current thing we're blocked/sleeping on?
    interruptible: KCell<Interruptible, ThreadLockClass>,
    /// Storage used by an `OwnedWaitQueue`, but held within a thread instance, while that thread
    /// is blocked in the wait queue.
    // Safety invariant: Accessed only through raw pointers (Rust's
    // `inherited_scheduler_state_storage()`, or C++'s `&inherited_scheduler_state_storage_`).
    // While the thread is blocked in an `OwnedWaitQueue`, it is accessed only while holding that
    // queue's lock (whether or not the queue currently uses it as its storage); otherwise it is
    // accessed only while holding the enclosing `Thread`'s lock.
    inherited_scheduler_state_storage: UnsafeCell<WaitQueueInheritedSchedulerState>,
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

impl ThreadState {
    /// Returns `true` if the thread is currently in a wait queue's tree.
    #[inline]
    pub fn in_wait_queue(&self) -> bool {
        self.blocked_threads_tree_node.in_container()
    }

    /// Returns the return code if woken up abnormally from suspend, sleep, or block.
    #[inline]
    pub fn blocked_status(&self, token: &LockToken<'_, ThreadLockClass>) -> Result<(), Status> {
        // SAFETY: `token` proves the enclosing `Thread`'s lock is held.
        Status::ok(*unsafe { self.blocked_status.get(token) })
    }

    /// Sets the return code for the blocked thread.
    #[inline]
    fn set_blocked_status(
        &self,
        status: Result<(), Status>,
        token: &mut LockToken<'_, ThreadLockClass>,
    ) {
        // SAFETY: `token` proves the enclosing `Thread`'s lock is exclusively held.
        *unsafe { self.blocked_status.get_mut(token) } = Status::result_into_raw(status);
    }

    /// Returns the wait queue the thread is currently blocked on, if any.
    #[inline]
    pub fn blocking_wait_queue(
        &self,
        token: &LockToken<'_, ThreadLockClass>,
    ) -> Option<NonNull<WaitQueueBase>> {
        // SAFETY: `token` proves the enclosing `Thread`'s lock is held.
        *unsafe { self.blocking_wait_queue.get(token) }
    }

    /// Sets the wait queue the thread is currently blocked on.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `wq` is either `None` (when the thread has just been, or is
    /// about to be, removed from its wait queue's collection), or points to a live
    /// [`WaitQueueBase`] in whose collection the thread has just been (or is about to be)
    /// inserted, and which remains live while the thread is blocked in it. If that queue's magic
    /// is [`OWNED_MAGIC`], `wq` must carry provenance over the enclosing [`OwnedWaitQueue`].
    #[inline]
    pub(crate) unsafe fn set_blocking_wait_queue(
        &self,
        wq: Option<NonNull<WaitQueueBase>>,
        token: &mut LockToken<'_, ThreadLockClass>,
    ) {
        // SAFETY: `token` proves the enclosing `Thread`'s lock is exclusively held.
        *unsafe { self.blocking_wait_queue.get_mut(token) } = wq;
    }

    /// Returns whether the thread is allowed to be interrupted on the current block/sleep.
    #[inline]
    pub fn interruptible(&self, token: &LockToken<'_, ThreadLockClass>) -> Interruptible {
        // SAFETY: `token` proves the enclosing `Thread`'s lock is held.
        *unsafe { self.interruptible.get(token) }
    }

    /// Sets whether the thread is allowed to be interrupted on the current block/sleep.
    #[inline]
    fn set_interruptible(
        &self,
        interruptible: Interruptible,
        token: &mut LockToken<'_, ThreadLockClass>,
    ) {
        // SAFETY: `token` proves the enclosing `Thread`'s lock is exclusively held.
        *unsafe { self.interruptible.get_mut(token) } = interruptible;
    }

    /// Returns a pointer to the storage used by an `OwnedWaitQueue`, but held within this thread,
    /// while the thread is blocked in the wait queue.
    #[inline]
    pub fn inherited_scheduler_state_storage(&self) -> NonNull<WaitQueueInheritedSchedulerState> {
        // SAFETY: `UnsafeCell::get` never returns a null pointer.
        unsafe { NonNull::new_unchecked(self.inherited_scheduler_state_storage.get()) }
    }
}

impl Drop for ThreadState {
    fn drop(&mut self) {
        debug_assert!(self.blocking_wait_queue.get_inner_mut().is_none());
    }
}

/// The collection of threads currently blocked in a wait queue.
#[pin_data(PinnedDrop)]
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

#[pinned_drop]
impl PinnedDrop for WaitQueueCollection {
    fn drop(self: Pin<&mut Self>) {
        self.validate();
        debug_assert!(self.threads.is_empty());
    }
}

impl WaitQueueCollection {
    /// In-place pinned initializer for [`WaitQueueCollection`].
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init::pin_init!(Self {
            threads <- BlockedThreadTree::new(),
        })
    }

    /// Returns a raw pointer to the underlying C++ `WaitQueueCollection`.
    #[inline]
    fn as_raw(&self) -> *const bindings::WaitQueueCollection {
        core::ptr::from_ref(self).cast()
    }

    /// Returns a raw mutable pointer to the underlying C++ `WaitQueueCollection`.
    #[inline]
    fn as_raw_mut(&mut self) -> *mut bindings::WaitQueueCollection {
        core::ptr::from_mut(self).cast()
    }

    /// Validates the collection's internal invariants.
    #[inline]
    pub fn validate(&self) {
        // TODO(johngro): We could perform a more rigorous check of the two maintained
        // invariants of the threads collection, however we probably only want to do
        // so if SCHEDULER_EXTRA_INVARIANT_VALIDATION is true.
    }

    /// Passthru for the underlying container's size method.
    #[inline]
    pub fn count(&self) -> u32 {
        self.threads.len() as u32
    }

    /// Passthru for the underlying container's `is_empty` method.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.threads.is_empty()
    }

    /// The current minimum inheritable relative deadline of the set of blocked threads.
    #[inline]
    pub fn min_inheritable_relative_deadline(&self) -> SchedDuration {
        // SAFETY: `self.as_raw()` is valid, and callers obtain `&WaitQueueCollection` through a
        // `ChainLockProof` for the enclosing wait queue, which synchronizes access to the threads
        // in the collection.
        SchedDuration::from_raw(unsafe {
            bindings::cpp_wait_queue_collection_min_inheritable_relative_deadline(self.as_raw())
        })
    }

    /// Peeks at the first [`Thread`] in the collection.
    #[inline]
    pub fn peek_front(&self) -> Option<ThreadPtr> {
        self.threads.front().map(ThreadPtr::from)
    }

    /// Returns the inherited scheduler state storage of the last [`Thread`] in the collection, or
    /// `None` if the collection is empty.
    #[inline]
    pub fn find_inherited_scheduler_state_storage(
        &mut self,
    ) -> Option<NonNull<WaitQueueInheritedSchedulerState>> {
        // SAFETY: `self.as_raw_mut()` is valid, and callers obtain `&mut WaitQueueCollection`
        // through a `ChainLockProof` for the enclosing wait queue, which synchronizes access to
        // the threads in the collection.
        NonNull::new(
            unsafe {
                bindings::cpp_wait_queue_collection_find_inherited_scheduler_state_storage(
                    self.as_raw_mut(),
                )
            }
            .cast(),
        )
    }

    /// Returns an iterator over the [`Thread`]s in the collection.
    #[inline]
    pub fn threads(&self) -> WaitQueueCollectionThreads<'_> {
        // SAFETY: `self.as_raw()` is valid, and callers obtain `&WaitQueueCollection` through a
        // `ChainLockProof` for the enclosing wait queue. If non-null, the returned pointer points
        // to a live `Thread` in the collection.
        let next = unsafe {
            ThreadPtr::from_raw(
                bindings::cpp_wait_queue_collection_threads_begin(self.as_raw()).cast(),
            )
        };
        WaitQueueCollectionThreads { collection: self, next }
    }

    /// Adds the [`Thread`] into its sorted location in the collection.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The enclosing wait queue's lock is held, synchronizing access to the
    ///   `WaitQueueCollection` and all threads currently in `self`.
    /// - Every node currently in `self` points to a valid, live blocked [`Thread`].
    /// - `thread.owner()` is not already in any wait queue collection, with its
    ///   `blocked_threads_tree_sort_key` reset to [`PRIMARY_KEY_ZERO`] and `subtree_min_deadline`
    ///   reset to `DurationMono::MAX`.
    #[inline]
    pub unsafe fn insert(&mut self, thread: &mut ChainLockProof<'_, Thread>) {
        // SAFETY: `self.as_raw_mut()` and `thread.owner()` are valid, `thread` proves the thread's
        // lock is held, and the caller guarantees the wait queue lock is held and `thread.owner()`
        // is not already in a collection.
        unsafe {
            bindings::cpp_wait_queue_collection_insert(
                self.as_raw_mut(),
                thread.owner().as_raw().cast(),
            );
        }
    }

    /// Removes the [`Thread`] from the collection.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The enclosing wait queue's lock is held, synchronizing access to the
    ///   `WaitQueueCollection` and all threads currently in `self`.
    /// - Every node currently in `self` points to a valid, live blocked [`Thread`].
    /// - `thread.owner()` is currently a member of `self`.
    #[inline]
    pub unsafe fn remove(&mut self, thread: &mut ChainLockProof<'_, Thread>) {
        // SAFETY: `self.as_raw_mut()` and `thread.owner()` are valid, `thread` proves the thread's
        // lock is held, and the caller guarantees the wait queue lock is held and `thread.owner()`
        // is currently in `self`.
        unsafe {
            bindings::cpp_wait_queue_collection_remove(
                self.as_raw_mut(),
                thread.owner().as_raw().cast(),
            );
        }
    }
}

/// An iterator over the [`Thread`]s in a [`WaitQueueCollection`].
pub struct WaitQueueCollectionThreads<'a> {
    collection: &'a WaitQueueCollection,
    next: Option<ThreadPtr>,
}

impl Iterator for WaitQueueCollectionThreads<'_> {
    type Item = ThreadPtr;

    #[inline]
    fn next(&mut self) -> Option<ThreadPtr> {
        let current = self.next?;
        // SAFETY: `current` is a member of `self.collection`, which cannot change while it is
        // borrowed (and its wait queue's lock is held). If non-null, the returned pointer points
        // to a live `Thread` in the collection.
        self.next = unsafe {
            ThreadPtr::from_raw(
                bindings::cpp_wait_queue_collection_threads_next(
                    self.collection.as_raw(),
                    current.as_ptr().cast(),
                )
                .cast(),
            )
        };
        Some(current)
    }
}

/// `assert_in_wait_queue` asserts (as its name implies) that `t` is currently
/// blocked in `wq`, and that the wait queue is locked. This should be
/// sufficient to establish (at runtime) that it is safe to allow read-access to
/// the thread's members which are used to track the thread's state in the queue.
///
/// TODO(johngro) : Link to something documenting the reasoning behind this.
/// 1) Consider making these asserts extra-debug-only asserts, so that most
///    builds do not need to suffer the runtime overhead of using them.
/// 2) Consider ways to limit the scope of the read-only capability to just the
///    wait queue state (and other things protected by the fact that we are
///    blocked in the wait queue) and nothing else. While the wait queue state
///    cannot mutate while we exist in the wait queue (at least, not without
///    holding both the wait queue and thread's lock), other things can.
#[inline]
pub(crate) fn assert_in_wait_queue(
    t: &ChainLockProof<'_, Thread>,
    wq: &ChainLockProof<'_, WaitQueueBase>,
) {
    debug_assert!(t.wait_queue_state().in_wait_queue());
    debug_assert!(
        t.state() == ThreadStateKind::Blocked || t.state() == ThreadStateKind::BlockedReadLock
    );
    debug_assert!(
        t.wait_queue_state().blocking_wait_queue(t.token()) == Some(NonNull::from(wq.owner()))
    );
}

/// Base type for wait queue types, providing common functionality.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct WaitQueueBase {
    /// The lock guarding `collection`; `ClassedChainLock` is `repr(transparent)` over `ChainLock`,
    /// so C++ sees a plain `ChainLock` at offset 0.
    lock: ClassedChainLock<WaitQueueLockClass>,
    // Safety invariant: Initialized to `MAGIC` (for `WaitQueue`) or `OWNED_MAGIC` (for the base
    // subobject of `OwnedWaitQueue`), and zeroed during `teardown`. Wrapped in `UnsafeCell` so
    // `&WaitQueueBase` / `&WaitQueue` contains no `Freeze` bytes if a woken thread on another CPU
    // begins tearing down the wait queue while the waking CPU unwinds from `scheduler::unblock`.
    magic: UnsafeCell<u32>,
    // Safety invariant: Must be empty when dropped. Accessed only through `ChainLockProof`
    // accessors on `WaitQueueBase`, `WaitQueue`, and `OwnedWaitQueue`.
    #[pin]
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

    /// Wait queues come in 2 flavors (traditional and owned) which are distinguished
    /// using the magic number. When `debug_assert!` checking the magic number, check
    /// against both of the possible valid magic numbers.
    #[inline]
    pub(super) fn debug_assert_magic_check(&self) {
        let magic = self.magic();
        debug_assert!(magic == MAGIC || magic == OWNED_MAGIC, "magic {:#010x}", magic);
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
        self.collection().is_empty()
    }

    /// Returns the number of threads currently blocked in this wait queue.
    #[inline]
    pub fn count(&self) -> u32 {
        self.collection().count()
    }

    /// Peeks at the first [`Thread`] in the wait queue collection.
    #[inline]
    pub fn peek_front(&self) -> Option<ThreadPtr> {
        self.collection().peek_front()
    }

    /// Validates the internal invariants of this wait queue.
    #[inline]
    pub fn validate_queue(&self) {
        self.owner().debug_assert_magic_check();
        self.collection().validate();
    }

    /// Performs the preamble of blocking `current_thread` on this wait queue.
    ///
    /// Notes for [`ChainLockProof::block_etc_preamble`] and
    /// [`WaitQueueBase::block_etc_postamble`]:
    ///
    /// Currently, there are two variants of `WaitQueue`s in Zircon. The standard [`WaitQueue`]
    /// (used for most tasks) and the specialized [`OwnedWaitQueue`]s (used for
    /// mutexes/futexes/brwlocks, and anything else which needs to have a concept of priority
    /// inheritance).
    ///
    /// The "Block" operation for these two versions are _almost_ identical. The only real
    /// difference between the two is that the OWQ implementation needs to stop after we have
    /// decided that we are actually going to block the thread, but before the timeout timer is
    /// armed and the thread is actually blocked, in order to update its PI chain bookkeeping.
    ///
    /// Instead of duplicating the code, or exposing a code-injection mechanism into the public
    /// API, we split the code into two inline member functions instead. The first
    /// (`block_etc_preamble`) performs all of the checks and bookkeeping up-to the point of arming
    /// the timer and blocking, the second (`block_etc_postamble`) finishes the job.
    ///
    /// The traditional [`WaitQueue`] implementation of `WaitQueue::block_etc` just calls these two
    /// functions back to back, relying on the inlining to generate the original function. The
    /// [`OwnedWaitQueue`] implementation does the same, but injects its bookkeeping at the
    /// appropriate point.
    ///
    /// Nothing but these two specific pieces of code should *ever* need to call these functions.
    /// Users should *always* be using either `WaitQueue::block_etc`/`block`, or
    /// `OwnedWaitQueue::block_and_assign_owner` instead.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] in the
    ///   `THREAD_RUNNING` state and is not already queued in any wait queue.
    /// - If `self.owner().magic() == OWNED_MAGIC`, `self.owner()` is the base subobject of a live
    ///   [`OwnedWaitQueue`](crate::kernel::owned_wait_queue::OwnedWaitQueue) with provenance over
    ///   the full `OwnedWaitQueue`, and if this method returns `Ok(())`, the caller updates its
    ///   `inherited_scheduler_state_storage` (e.g. via
    ///   `OwnedWaitQueue::UpdateSchedStateStorageThreadAdded`) before releasing `self.owner()`'s
    ///   lock or observing `inherited_scheduler_state_storage`.
    /// - If this method returns `Ok(())`, the caller must complete the block operation by calling
    ///   [`WaitQueueBase::block_etc_postamble`] before dropping the wait queue or unlocking
    ///   `current_thread`.
    #[inline]
    pub unsafe fn block_etc_preamble(
        &mut self,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        signal_mask: u32,
        reason: ResourceOwnership,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        let wq_ptr = NonNull::from(self.owner());
        // SAFETY: Forwarded from the caller; `wq_ptr` points to `self.owner()`, and the caller
        // guarantees that `self.owner()` carries provenance over the enclosing `OwnedWaitQueue` if
        // its magic is `OWNED_MAGIC`.
        unsafe {
            self.block_etc_preamble_with_ptr(
                wq_ptr,
                current_thread,
                deadline,
                signal_mask,
                reason,
                interruptible,
            )
        }
    }

    /// Exactly like [`Self::block_etc_preamble`], but records `wq_ptr` (rather than a pointer
    /// derived from `self.owner()`) as the thread's blocking wait queue. This lets an
    /// `OwnedWaitQueue` record a pointer with provenance over the whole `OwnedWaitQueue`, which a
    /// `&WaitQueueBase` does not have under Stacked Borrows.
    ///
    /// # Safety
    ///
    /// The requirements of [`Self::block_etc_preamble`] (with the provenance requirement applying
    /// to `wq_ptr` instead of `self.owner()`), and `wq_ptr` must point to `self.owner()`.
    #[inline]
    pub(crate) unsafe fn block_etc_preamble_with_ptr(
        &mut self,
        wq_ptr: NonNull<WaitQueueBase>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        signal_mask: u32,
        reason: ResourceOwnership,
        interruptible: Interruptible,
    ) -> Result<(), Status> {
        debug_assert!(core::ptr::addr_eq(wq_ptr.as_ptr(), self.owner()));
        debug_assert!(core::ptr::eq(
            current_thread.owner().as_raw(),
            // SAFETY: Multi-threading is initialized when blocking a thread.
            unsafe { ThreadPtr::current() }.as_raw()
        ));

        if deadline.when() != InstantMono::INFINITE.into()
            && deadline.when() <= current_mono_time().into()
        {
            return Err(Status::TIMED_OUT);
        }

        if interruptible == Interruptible::Yes
            && (current_thread.owner().signals() & !signal_mask) != 0
        {
            current_thread.owner().check_kill_or_suspend_signal()?;
        }

        let state = current_thread.wait_queue_state();

        state.set_interruptible(interruptible, current_thread.token_mut());

        // SAFETY: `self` proves the wait queue lock is held, and the caller guarantees
        // `current_thread.owner()` is not already in any wait queue collection and maintains the
        // `OwnedWaitQueue` `inherited_scheduler_state_storage` invariant if `self.owner()` is an
        // `OwnedWaitQueue`.
        unsafe {
            self.collection_mut().insert(current_thread);
        }
        if reason == ResourceOwnership::Normal {
            current_thread.set_blocked();
        } else {
            current_thread.set_blocked_read_lock();
        }
        // SAFETY: `current_thread.owner()` was just inserted into `self.owner()`'s collection, and
        // the caller guarantees that `wq_ptr` points to `self.owner()`, which stays live while the
        // thread is blocked in it, and that `wq_ptr` carries provenance over the enclosing
        // `OwnedWaitQueue` if its magic is `OWNED_MAGIC`.
        unsafe { state.set_blocking_wait_queue(Some(wq_ptr), current_thread.token_mut()) };
        state.set_blocked_status(Ok(()), current_thread.token_mut());

        Ok(())
    }

    /// Remove a thread from a wait queue, maintain the wait queue's internal count,
    /// and update the `WaitQueue` specific bookkeeping in the thread in the process.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `t.owner()` is currently blocked in `self.owner()`.
    /// - If `self.owner().magic() == OWNED_MAGIC`, `self.owner()` is the base subobject of a live
    ///   [`OwnedWaitQueue`](crate::kernel::owned_wait_queue::OwnedWaitQueue), and the caller
    ///   updates its `inherited_scheduler_state_storage` (e.g. via
    ///   [`ChainLockProof::update_sched_state_storage_thread_removed`]) before releasing
    ///   `self.owner()`'s lock or observing `inherited_scheduler_state_storage`.
    #[inline]
    pub unsafe fn dequeue(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
        wait_queue_error: impl Into<Result<(), Status>>,
    ) {
        assert_in_wait_queue(t, self);

        // SAFETY: `self` proves the wait queue lock is held, and the caller guarantees
        // `t.owner()` is currently blocked in `self.owner()` and maintains the `OwnedWaitQueue`
        // `inherited_scheduler_state_storage` invariant if `self.owner()` is an `OwnedWaitQueue`.
        unsafe {
            self.collection_mut().remove(t);
        }
        t.wait_queue_state().set_blocked_status(wait_queue_error.into(), t.token_mut());
        // SAFETY: `t.owner()` was just removed from `self.owner()`'s collection.
        unsafe { t.wait_queue_state().set_blocking_wait_queue(None, t.token_mut()) };
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
    #[inline]
    pub unsafe fn update_blocked_thread_effective_profile(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
    ) {
        t.owner().assert_canary();
        self.owner().debug_assert_magic_check();
        // Note, we don't do this in order to establish shared access to the thread's
        // scheduler state. We should already have the thread locked for exclusive
        // access. We just do this as an additional consistency check.
        assert_in_wait_queue(t, self);

        // SAFETY: `self` proves the wait queue lock is held, the caller guarantees `t.owner()` is
        // currently blocked in `self.owner()`, and immediately re-inserting `t` below preserves
        // the set of threads in `collection` (maintaining the `OwnedWaitQueue`
        // `inherited_scheduler_state_storage` invariant).
        unsafe {
            self.collection_mut().remove(t);
        }
        let state = t.scheduler_state_mut();
        state.recompute_effective_profile();
        // SAFETY: `self` proves the wait queue lock is held, and `t.owner()` was just removed from
        // `self.collection_mut()` (which resets its sort key and subtree min deadline), restoring
        // the set of threads in `collection` and its `OwnedWaitQueue`
        // `inherited_scheduler_state_storage` invariant.
        unsafe {
            self.collection_mut().insert(t);
        }

        if WAIT_QUEUE_VALIDATION {
            self.validate_queue();
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
        pin_init::pin_init!(Self {
            lock: ClassedChainLock::new(),
            magic: UnsafeCell::new(magic),
            collection <- KCell::pin_init(WaitQueueCollection::init()),
            _pin: PhantomPinned,
        })
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
        // SAFETY: `magic` is initialized on construction and only mutated during `teardown` after
        // all threads have been dequeued and no new operations may be initiated on this queue.
        unsafe { *self.magic.get() }
    }

    /// Tears down a wait queue.
    ///
    /// This panics if any threads were waiting on this queue, because that
    /// would indicate a race condition for most uses of wait queues. If a
    /// thread is currently waiting, it could have been scheduled later, in
    /// which case it would have called `block()` on an invalid wait
    /// queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this wait queue is being destroyed with no concurrent accesses
    /// to `magic` or `collection`.
    unsafe fn teardown(&self) {
        self.debug_assert_magic_check();

        // SAFETY: The caller guarantees exclusive teardown access to `collection`.
        let proof = unsafe { ChainLockProof::assume_held_unchecked(self) };
        let count = proof.count();
        if count != 0 {
            panic!(
                "~WaitQueue() called on non-empty WaitQueue, count={}, magic={:#010x}",
                count,
                self.magic()
            );
        }

        // SAFETY: The caller guarantees exclusive teardown access to `magic`.
        unsafe {
            self.magic.get().write(0);
        }
    }

    /// Wake a specific thread from a specific wait queue.
    ///
    /// TODO(johngro): Update this comment. `unblock_thread` does not actually remove the thread
    /// from the wait queue, it simply finishes the unblock operation, propagating any PI effects
    /// and dropping the PI lock chain starting from the wait queue in the process.
    ///
    /// This function extracts a specific thread from a wait queue, wakes it, puts it into a
    /// `Scheduler`'s run queue, and does a reschedule if necessary. Callers of this function must
    /// be sure that they are holding the locks for all of the nodes in the PI chain, starting from
    /// the thread, before calling the function. Static analysis can only ensure that the thread
    /// and its immediately downstream blocking wait queue are locked.
    ///
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
    #[inline]
    pub unsafe fn unblock_thread(
        mut guard: OwnerGuard<'_, Self>,
        mut t_guard: OwnerGuard<'_, Thread>,
        wait_queue_error: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        {
            let mut wq_proof = guard.proof();
            let mut t_proof = t_guard.proof();
            wq_proof.owner().debug_assert_magic_check();
            t_proof.owner().assert_canary();

            if WAIT_QUEUE_VALIDATION {
                wq_proof.validate_queue();
            }

            // Remove the thread from the wait queue and deal with any PI propagation
            // required if the thread was removed from an owned wait queue. Finally,
            // formally unblock the thread, allowing it to join a run queue.
            // SAFETY: The caller guarantees `t_proof.owner()` is actively blocked in
            // `wq_proof.owner()`, and if `wq_proof.owner()` is an `OwnedWaitQueue`,
            // `update_sched_state_storage_thread_removed` is called below before releasing the
            // queue lock.
            unsafe {
                wq_proof.dequeue(&mut t_proof, wait_queue_error);
            }

            // SAFETY: The caller guarantees that if `wq_proof.owner().magic() == OWNED_MAGIC`,
            // `wq_proof.owner()` is the base subobject of a live `OwnedWaitQueue` with provenance
            // over the full `OwnedWaitQueue`.
            if let Some(owq) = unsafe { OwnedWaitQueue::downcast_to_owq(wq_proof.owner()) } {
                let mut owq_proof = wq_proof.retag(owq);
                // Get the effective profile of the owned wait queue before propagation,
                // which will recompute the effective profile of the queue. This is
                // necessary for the scheduler PI logic to determine how the wait queue's
                // effective profile changed.
                let old_target_ep = owq_proof.get_effective_profile();
                // SAFETY: `t_proof.owner()` was just dequeued from `owq`, and the caller
                // guarantees all downstream locks in the PI graph are held.
                unsafe {
                    OwnedWaitQueue::begin_propagate_remove_single_edge(
                        &mut t_proof,
                        &mut owq_proof,
                        ForceInheritance::No,
                        Some(&old_target_ep),
                        clt_token,
                        preempt_disabled_token,
                    );
                    owq_proof.update_sched_state_storage_thread_removed(&mut t_proof);
                }
            }
        }

        let wait_queue = guard.into_unguarded();

        // Now that any required propagation is complete, we can release all of the PI
        // chain locks starting from this wait queue.
        // SAFETY: `wait_queue`'s lock and all downstream PI chain locks are held by the active
        // transaction and will not be unlocked again by the caller.
        unsafe {
            OwnedWaitQueue::unlock_pi_chain_common_from_wait_queue(wait_queue, clt_token);
        }

        // The PI consequences of the unblock (if any) have been applied, and all of
        // the (previously) downstream PI chain has been unlocked. Go ahead and
        // unblock the thread in the scheduler, which will drop the unblocking thread's
        // lock for us.
        // SAFETY: `t_guard.owner()` is blocked and ready to be unblocked, its lock is held and
        // consumed via `t_guard`, and the active transaction and preemption-disabled states are
        // verified.
        unsafe {
            scheduler::unblock(t_guard, clt_token);
        }
        Ok(())
    }

    /// Performs the postamble of blocking `current_thread` on `this` wait queue after the wait
    /// queue's lock has been released.
    ///
    /// Takes `this: *const Self` rather than `&self` so that no Rust reference to the wait queue
    /// lives across [`scheduler::block`], after which another CPU may wake `current_thread` and
    /// destroy the wait queue before `current_thread` returns from `scheduler::block`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] on the current CPU,
    ///   and the CPU is in a context where blocking is permitted (no spinlocks held and blocking
    ///   not disallowed).
    /// - [`ChainLockProof::block_etc_preamble`] succeeded for `current_thread.owner()` on `this`,
    ///   `(*this).lock()` has been released, and the active transaction is finalized with
    ///   `current_thread.owner().lock()` as the only held chain lock.
    /// - `this` points to a valid, initialized [`WaitQueueBase`] on entry while
    ///   `current_thread.owner().lock()` is held prior to [`scheduler::block`].
    /// - If `deadline` is not infinite, it uses the monotonic timeline.
    pub unsafe fn block_etc_postamble(
        this: *const Self,
        mut current_thread: ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        debug_assert!(!this.is_null());
        assert_transaction_active(clt_token);
        // SAFETY: While `current_thread`'s lock is held prior to `scheduler::block`,
        // `current_thread` cannot be dequeued from `this`, so `this` remains alive and valid.
        // Scope this borrow so no `&WaitQueueBase` reference lives across `scheduler::block`.
        assert_lock_not_held(unsafe { (*this).lock() });
        debug_assert!(core::ptr::eq(
            current_thread.owner().as_raw(),
            // SAFETY: Multi-threading is initialized when blocking a thread.
            unsafe { ThreadPtr::current() }.as_raw()
        ));
        pin_init::stack_pin_init!(let timer = Timer::init(ZX_CLOCK_MONOTONIC));
        let timer = timer.as_ref();

        // If the deadline is nonzero or noninfinite, set a callback to yank us out of the queue.
        if deadline.when() != InstantMono::INFINITE.into() {
            // SAFETY: `rust_wait_queue_base_timeout_handler` is a valid timer callback for
            // `current_thread`, `timer` is pinned on the stack, and if the timer is set,
            // `timer.cancel()` is called below before `timer` is dropped from the stack.
            unsafe {
                timer.set_deadline(
                    deadline,
                    rust_wait_queue_base_timeout_handler,
                    current_thread.owner().as_raw().cast::<c_void>(),
                );
            }
        }

        // SAFETY: The caller guarantees `current_thread.owner()` is the currently executing
        // thread in a blocked state with its lock held as the only lock in the finalized
        // transaction, and blocking is permitted on the current CPU.
        unsafe {
            scheduler::block(&mut current_thread, clt_token);
        }

        // We don't really know if the timer fired or not, so it's better safe to try to cancel it.
        if deadline.when() != InstantMono::INFINITE.into() {
            let _ = timer.cancel();
        }

        let wq_state = current_thread.wait_queue_state();
        wq_state.set_interruptible(Interruptible::No, current_thread.token_mut());
        wq_state.blocked_status(current_thread.token())
    }

    /// Wait queues are building blocks that other locking primitives use to handle
    /// blocking threads.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `timer` points to a valid [`Timer`] in an active timer
    /// callback context, `arg` points to the valid, live [`Thread`] associated with the wait
    /// timeout, and no `ChainLockTransaction` is currently active on the current CPU.
    pub unsafe fn timeout_handler(timer: *mut Timer, _now: InstantMono, arg: *mut c_void) {
        debug_assert!(!timer.is_null());
        debug_assert!(!arg.is_null());
        // SAFETY: The caller guarantees `arg` points to a valid, live `Thread`.
        let thread: &Thread = unsafe { &*arg.cast::<Thread>() };
        thread.assert_canary();

        // In order to wake the thread with a TIMED_OUT error, we need to hold the
        // entire PI chain starting from the thread, and ending at the target of the
        // PI graph that the thread is blocked in. It is possible, however, that the
        // thread has already woken up and is attempting to cancel the timeout
        // handler. So, use the Timer's TrylockOrCancel to lock the first node in the
        // chain (the thread), being prepared to abort the operation if we are getting
        // canceled. Then proceed to lock the rest of the chain, backing off and
        // retrying as needed.
        //
        // TODO(johngro): Optimize the case where the thread being unblocked has a
        // non-inheritable profile. We don't need to lock the entire PI chain if
        // there will be no PI consequences to deal with. It should be sufficient to
        // simply lock the thread and the wait queue following it, and remove the
        // thread from the queue.
        let try_lock_or_cancel = |lock: &crate::kconcurrent::ChainLock| -> bool {
            // SAFETY: The caller guarantees `timer` points to a valid `Timer` in an active timer
            // callback, and `Timer` uses interior mutability (`OpaqueBytes`).
            let timer_ref = unsafe { &*timer };
            // SAFETY: Called from `timer_ref`'s timer callback within an active, unfinalized
            // `ChainLockTransaction` that does not yet hold `lock`.
            unsafe { timer_ref.trylock_or_cancel(lock).is_ok() }
        };

        // If the timeout has not been cancelled, handle the following:
        // 1. The thread is still sitting in its wait queue, it has not been
        //    explicitly unblocked yet. Its state must be either BLOCKED or
        //    BLOCKED_READ_LOCK. Finish locking, and then unblock the thread.
        // 2. The thread has been explicitly unblocked, but it has not become
        //    scheduled yet. The thread will have no blocking wait queue, and its
        //    state must be READY. It will eventually become scheduled, and as it
        //    unwinds, it will attempt to cancel this timer (which is currently on
        //    its stack).
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the duration
            // of this transaction.
            let preempt_disabled = unsafe { PreemptDisabledToken::new() };
            // SAFETY: `try_lock_or_cancel` calls `Timer::trylock_or_cancel(lock)` and returns
            // `true` iff `lock` was acquired for the active transaction.
            let Some(mut thread_guard) =
                (unsafe { OwnerGuard::try_acquire_with(thread, token, try_lock_or_cancel) })
            else {
                return DONE.into();
            };

            let wq_ptr = {
                let t_proof = thread_guard.proof();
                let Some(wq_ptr) = t_proof.wait_queue_state().blocking_wait_queue(t_proof.token())
                else {
                    debug_assert!(t_proof.state() == ThreadStateKind::Ready);
                    return DONE.into();
                };
                debug_assert!(
                    t_proof.state() == ThreadStateKind::Blocked
                        || t_proof.state() == ThreadStateKind::BlockedReadLock
                );
                wq_ptr
            };

            // SAFETY: While `thread` is blocked on `wq_ptr` and `thread`'s lock is held, `wq_ptr`
            // points to a valid, live `WaitQueueBase`.
            let wq: &WaitQueueBase = unsafe { wq_ptr.as_ref() };
            {
                let mut t_proof = thread_guard.proof();
                // SAFETY: The transaction is not yet finalized and no downstream locks in
                // `thread`'s PI chain are held yet; if `Ok` is returned, `unblock_thread` below
                // unlocks all downstream locks in the PI chain.
                if unsafe { OwnedWaitQueue::lock_pi_chain_from_thread(&mut t_proof, token) }
                    == ChainLockResult::Backoff
                {
                    return Action::Backoff.into();
                }
            }
            wq.get_lock().assert_acquired(token);
            // SAFETY: `lock_pi_chain_from_thread` succeeded and acquired `wq.lock()` (verified by
            // `assert_acquired` above), and `wq_guard` takes ownership of releasing `wq.lock()`.
            let wq_guard = unsafe { OwnerGuard::adopt(wq, token) };

            ChainLockTransaction::finalize(token);
            // SAFETY: `thread` is blocked in `wq`, the transaction is finalized with `thread` and
            // its entire downstream PI chain locked, and if `wq` is an `OwnedWaitQueue`, `wq_ptr`
            // was stored from the `OwnedWaitQueue`'s base subobject.
            let _ = unsafe {
                Self::unblock_thread(
                    wq_guard,
                    thread_guard,
                    Err(Status::TIMED_OUT),
                    token,
                    &preempt_disabled,
                )
            };
            DONE.into()
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("WaitQueue::TimeoutHandler"),
            do_transaction,
        );
    }
}

#[pinned_drop]
impl PinnedDrop for WaitQueueBase {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `drop` has exclusive teardown access to `self`.
        unsafe {
            self.as_ref().get_ref().teardown();
        }
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
        // SAFETY: `WaitQueue` is `#[repr(C)]` with `base: WaitQueueBase` at offset 0.
        // Casting from `self` rather than projecting `&self.base` preserves pointer provenance
        // over the entire `WaitQueue` allocation.
        unsafe { &*core::ptr::from_ref(self).cast::<WaitQueueBase>() }
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
        let base: &WaitQueueBase = self.owner();
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
        self.collection().is_empty()
    }

    /// Returns the number of threads currently blocked in this wait queue.
    #[inline]
    pub fn count(&self) -> u32 {
        self.collection().count()
    }

    /// Peeks at the first [`Thread`] in the wait queue collection.
    #[inline]
    pub fn peek_front(&self) -> Option<ThreadPtr> {
        self.collection().peek_front()
    }

    /// Validates the internal invariants of this wait queue.
    #[inline]
    pub fn validate_queue(&self) {
        self.owner().base.debug_assert_magic_check();
        self.collection().validate();
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
    /// the call to [`scheduler::unblock`]).
    #[inline]
    pub fn wake_one_locked(
        &mut self,
        wait_queue_error: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<bool> {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let wait_queue_error = wait_queue_error.into();

        // Note(johngro): See the note in `wake_one`.  No one should ever be calling
        // this method on an `OwnedWaitQueue`.
        debug_assert!(self.owner().magic() == MAGIC);
        if WAIT_QUEUE_VALIDATION {
            self.validate_queue();
        }

        // Check to see if there is a thread we want to wake.
        if let Some(t_ptr) = self.peek_front() {
            // SAFETY: `t_ptr` is currently blocked in `self.owner()` and cannot exit while
            // `self.owner().lock()` is held.
            let t: &Thread = unsafe { t_ptr.as_ref() };
            // There is!  Try to lock it so we can actually wake it up.  If we can't, we
            // will need to unwind to allow the caller to release our lock before trying
            // again.
            let mut t_guard = OwnerGuard::acquire_or_backoff(t, clt_token)?;

            // We now have all of the locks we need for the wake operation to proceed.
            // Make sure we mark the active `ChainLockTransaction` as finalized.
            ChainLockTransaction::finalize(clt_token);

            // Remove the thread from the queue and unblock it.  We cannot drop the
            // queue lock yet, our caller needs us to hold onto it.  The scheduler will
            // take care of dropping the thread's lock for us after it has found a CPU
            // for the thread and added it to the proper scheduler.
            // SAFETY: `t` was obtained from `self.peek_front()`, so it is currently blocked in
            // `self.owner()`, and both locks are held.
            unsafe {
                self.as_base().dequeue(&mut t_guard.proof(), wait_queue_error);
            }
            // SAFETY: `t` was dequeued from `self.owner()` and the active transaction is
            // finalized.
            unsafe {
                scheduler::unblock(t_guard, clt_token);
            }
            return Some(true);
        }

        // No one to wake up.
        Some(false)
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
    /// the call to [`scheduler::unblock_list`]).
    #[inline]
    pub fn wake_all_locked(
        &mut self,
        wait_queue_error: impl Into<Result<(), Status>>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Option<u32> {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let wait_queue_error = wait_queue_error.into();

        // Note(johngro): See the note in `wake_one`.  No one should ever be calling
        // this method on an `OwnedWaitQueue`.
        debug_assert!(self.owner().magic() == MAGIC);
        if WAIT_QUEUE_VALIDATION {
            self.validate_queue();
        }

        // If the collection is empty, there is nothing left to do.
        if self.is_empty() {
            ChainLockTransaction::finalize(clt_token);
            return Some(0);
        }

        let count = self.count();
        pin_init::stack_pin_init!(let unblock_list = ThreadUnblockList::init());
        // SAFETY: `unblock_list` is newly initialized and empty. If `true` is returned, we
        // finalize the transaction and pass `unblock_list` to `scheduler::unblock_list`.
        let locked = unsafe {
            WaitQueueLockOps::lock_for_wake_all_in_queue(
                self,
                wait_queue_error,
                unblock_list.as_mut(),
                clt_token,
            )
        };
        if locked {
            // Now that we have all of the threads locked, we are committed to the
            // `wake_all` operation and can unblock our threads. Unlike the standard
            // `wake_all`, we need to continue to hold onto the queue lock to satisfy our
            // caller's requirements.
            ChainLockTransaction::finalize(clt_token);
            // SAFETY: All threads in `unblock_list` were locked and dequeued by
            // `lock_for_wake_all_in_queue`, and the active transaction is finalized.
            unsafe {
                scheduler::unblock_list(unblock_list, clt_token);
            }
            Some(count)
        } else {
            None
        }
    }

    /// Dequeues the specified thread `t` and sets its `blocked_status`. Does not actually schedule
    /// the thread to run.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t.owner()` is currently blocked in `self.owner()`.
    #[inline]
    pub unsafe fn dequeue_thread(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
        wait_queue_error: impl Into<Result<(), Status>>,
    ) {
        self.owner().debug_assert_magic_check();

        if WAIT_QUEUE_VALIDATION {
            self.validate_queue();
        }

        // SAFETY: `self` and `t` prove both locks are held, and the caller guarantees `t.owner()`
        // is blocked in `self.owner()`.
        unsafe {
            self.as_base().dequeue(t, wait_queue_error);
        }
    }
}

/// Operations for locking threads in a [`WaitQueue`] for waking.
pub struct WaitQueueLockOps;

impl WaitQueueLockOps {
    /// Attempts to lock one or all threads for waking.
    ///
    /// When successful, removes the threads from the wait queue, moving them to a temporary
    /// unblock list and setting their wake result in the process. Callers may then perform any
    /// internal bookkeeping before dropping the queue lock, and finally waking the threads using
    /// [`scheduler::unblock_list`].
    ///
    /// Returns `false` in the case that a backoff condition is encountered. The active transaction
    /// must not yet be finalized, and no locks of threads currently blocked in `queue.owner()` may
    /// already be held (otherwise a lock cycle assertion will trigger).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `unblock_list` is empty.
    /// - If this call returns `true`, the caller must finalize the transaction and pass
    ///   `unblock_list` to [`scheduler::unblock_list`] before ending the transaction (and must not
    ///   drop `unblock_list` while non-empty).
    #[must_use]
    #[inline]
    pub unsafe fn lock_for_wake_all_in_queue(
        queue: &mut ChainLockProof<'_, WaitQueue>,
        wait_queue_error: impl Into<Result<(), Status>>,
        unblock_list: Pin<&mut ThreadUnblockList>,
        clt_token: &ChainLockTransactionToken,
    ) -> bool {
        assert_transaction_active(clt_token);
        // SAFETY: `queue` proves `queue.owner().lock()` is exclusively held in the active
        // transaction, `unblock_list.as_raw()` points to a valid empty `Thread::UnblockList`, and
        // the caller upholds the preconditions above.
        unsafe {
            bindings::cpp_wait_queue_lock_ops_lock_for_wake_all_in_queue(
                queue.owner().as_raw(),
                Status::result_into_raw(wait_queue_error.into()),
                unblock_list.as_raw().cast(),
            )
        }
    }
}

impl WaitQueue {
    pub const MAGIC: u32 = MAGIC;

    /// In-place pinned initializer for `WaitQueue`.
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init::pin_init!(Self {
            // SAFETY: `MAGIC` is the valid magic value for a `WaitQueue`.
            base <- unsafe { WaitQueueBase::init_with_magic(MAGIC) },
        })
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

    /// There are a limited number of operations which should never be done on a
    /// [`WaitQueue`] which happens to be an [`OwnedWaitQueue`]. Specifically, blocking.
    /// Blocking on an OWQ should always go through the OWQ specific
    /// [`OwnedWaitQueue::block_and_assign_owner`].
    #[inline]
    fn debug_assert_magic_and_not_owq(&self) {
        debug_assert!(
            self.magic() != OWNED_MAGIC,
            "This operation should not be performed against the WaitQueue API, use the \
             OwnedWaitQueue API instead."
        );
        debug_assert!(self.magic() == MAGIC, "magic {:#010x}", self.magic());
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
    /// - `guard.owner()` remains valid until this call returns. (If the wait queue may be
    ///   destroyed by a waking CPU before [`scheduler::block`] returns, use
    ///   [`Self::block_etc_preamble`] in a narrower scope followed by
    ///   [`WaitQueueBase::block_etc_postamble`] so no `&WaitQueue` borrow spans the postamble.)
    #[inline(always)]
    pub unsafe fn block(
        guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        interruptible: Interruptible,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        // SAFETY: Forwarded from the caller.
        unsafe {
            Self::block_etc(
                guard,
                current_thread,
                deadline,
                0,
                ResourceOwnership::Normal,
                interruptible,
                clt_token,
            )
        }
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
    /// - `guard.owner()` remains valid until this call returns. (If the wait queue may be
    ///   destroyed by a waking CPU before [`scheduler::block`] returns, use
    ///   [`Self::block_etc_preamble`] in a narrower scope followed by
    ///   [`WaitQueueBase::block_etc_postamble`] so no `&WaitQueue` borrow spans the postamble.)
    #[inline(always)]
    pub unsafe fn block_instant(
        guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: InstantMono,
        interruptible: Interruptible,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        // SAFETY: Forwarded from the caller; `Deadline::no_slack` constructs a monotonic deadline
        // from `deadline`.
        unsafe {
            Self::block_etc(
                guard,
                current_thread,
                &Deadline::no_slack(deadline.into()),
                0,
                ResourceOwnership::Normal,
                interruptible,
                clt_token,
            )
        }
    }

    /// Performs the preamble of [`Self::block_etc`] and releases `guard` (`guard.owner().lock()`),
    /// returning a raw pointer to the embedded [`WaitQueueBase`] for
    /// [`WaitQueueBase::block_etc_postamble`].
    ///
    /// Separating the preamble from [`WaitQueueBase::block_etc_postamble`] allows callers (such as
    /// C++ FFI entry points) to create and consume the `&WaitQueue` reference and [`OwnerGuard`]
    /// entirely within a shorter lifetime before entering [`WaitQueueBase::block_etc_postamble`],
    /// so that no `&WaitQueue` borrow lives across [`scheduler::block`].
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
    /// - `guard.owner()` remains valid while `current_thread.owner()` remains blocked in it (and
    ///   at least until `guard.owner().lock()` and `current_thread.owner().lock()` are released
    ///   during the block operation).
    /// - If this call returns `Ok(base_ptr)`, the caller must immediately invoke
    ///   [`WaitQueueBase::block_etc_postamble`] with `base_ptr` before unlocking
    ///   `current_thread.owner().lock()`.
    #[inline]
    pub unsafe fn block_etc_preamble(
        mut guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        signal_mask: u32,
        reason: ResourceOwnership,
        interruptible: Interruptible,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<*const WaitQueueBase, Status> {
        assert_transaction_active(clt_token);
        debug_assert!(core::ptr::eq(
            current_thread.owner().as_raw(),
            // SAFETY: Multi-threading is initialized when blocking a thread.
            unsafe { ThreadPtr::current() }.as_raw()
        ));
        guard.owner().debug_assert_magic_and_not_owq();
        debug_assert!(current_thread.state() == ThreadStateKind::Running);

        if WAIT_QUEUE_VALIDATION {
            guard.proof().validate_queue();
        }

        // SAFETY: `guard` and `current_thread` prove both locks are held, and the caller
        // upholds the blocking preconditions.
        let res = unsafe {
            guard.proof().as_base().block_etc_preamble(
                current_thread,
                deadline,
                signal_mask,
                reason,
                interruptible,
            )
        };

        let this: *const WaitQueueBase = &raw const guard.owner().base;

        // After `block_etc_preamble` has completed, we need to drop the `WaitQueue`'s lock.
        // At this point, we are committed.  Either the thread successfully joined the
        // wait queue and needs to descend into the scheduler to actually block
        // (handled by `block_etc_postamble`), or it failed to join the queue (because of
        // a timeout, or something similar).  Either way, we should no longer be
        // holding the queue lock.
        guard.release();

        // If we failed to join the queue, propagate the error up (while still holding
        // the thread's lock).  Otherwise, return `this` so the caller can enter the
        // scheduler while holding the thread's lock.
        res.map(|()| this)
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
    /// - `guard.owner()` remains valid until this call returns. (If the wait queue may be
    ///   destroyed by a waking CPU before [`scheduler::block`] returns, use
    ///   [`Self::block_etc_preamble`] in a narrower scope followed by
    ///   [`WaitQueueBase::block_etc_postamble`] so no `&WaitQueue` borrow spans the postamble.)
    #[inline(always)]
    pub unsafe fn block_etc(
        guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        signal_mask: u32,
        reason: ResourceOwnership,
        interruptible: Interruptible,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<(), Status> {
        // SAFETY: Forwarded from the caller.
        let this = unsafe {
            Self::block_etc_preamble(
                guard,
                current_thread,
                deadline,
                signal_mask,
                reason,
                interruptible,
                clt_token,
            )?
        };

        // SAFETY: `block_etc_preamble` returned `Ok(this)`, the queue lock has been released,
        // `current_thread.owner().lock()` is the only held chain lock in the finalized active
        // transaction, and the caller upholds the blocking preconditions.
        unsafe {
            WaitQueueBase::block_etc_postamble(this, current_thread.reborrow(), deadline, clt_token)
        }
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
        let wait_queue_error = wait_queue_error.into();

        // In order to wake the next thread from the `WaitQueue`, we need to hold two
        // locks.  First we must hold the `WaitQueue`'s lock in order to select the
        // "best" thread to wake, and then we need to obtain the best thread's lock in
        // order to actually remove it from the queue and transfer it to a scheduler
        // via `scheduler::unblock`.
        //
        // Note that we explicitly disable local rescheduling using a preempt-disabler
        // before calling into the scheduler.  This is not just an optimization; we
        // can't be holding the local thread's lock if/when a local reschedule needs
        // to happen as this would set up a situation where we were holding the woken
        // thread's lock, and then needed to be holding the current thread's lock as
        // well. This is not an easy thing to do; we would have had to lock the
        // current thread as part of the lock chain before making any changes to the
        // actual state.
        //
        // Turning off local preemption before obtaining any locks avoids this issue.
        // We can now call into the scheduler holding whatever locks we want (fewer is
        // better, however), confident that there will be no local rescheduling until
        // after the preempt disabler has gone out of scope, after we have dropped all
        // of our locks.
        //
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<bool> {
            let mut guard = OwnerGuard::new(self, token);

            // Note(johngro): No one should ever be calling `wake_one` on an
            // instance of an `OwnedWaitQueue`.  `OwnedWaitQueue`s need to deal with
            // priority inheritance, and all wake operations on an `OwnedWaitQueue` should
            // be going through their interface instead.
            debug_assert!(self.magic() == MAGIC);
            if WAIT_QUEUE_VALIDATION {
                guard.proof().validate_queue();
            }

            // Now that we are holding the queue lock, attempt to lock the thread's pi
            // lock.  Be prepared to drop all locks and retry the operation if we
            // fail.
            if let Some(t_ptr) = guard.proof().peek_front() {
                // SAFETY: `t_ptr` is currently blocked in `self` and cannot exit while
                // `self.lock()` is held.
                let t: &Thread = unsafe { t_ptr.as_ref() };
                let Some(mut t_guard) = OwnerGuard::acquire_or_backoff(t, token) else {
                    return Action::Backoff.into();
                };
                ChainLockTransaction::finalize(token);

                // Remove the thread from the queue and drop the queue lock.  We no longer
                // need to hold it once the thread has been removed.
                // SAFETY: `t` was obtained from `guard.proof().peek_front()`, so it is currently
                // blocked in `self`, and both locks are held.
                unsafe {
                    guard.proof().as_base().dequeue(&mut t_guard.proof(), wait_queue_error);
                }
                guard.release();

                // Finally, call into the scheduler to finish unblocking the thread.  We
                // need to continue to hold the thread's lock while we do this, the
                // scheduler will drop the lock for us once it has found a CPU for the
                // thread and added it to the proper scheduler instance.
                // SAFETY: `t` was dequeued from `self` and the active transaction is finalized.
                unsafe {
                    scheduler::unblock(t_guard, token);
                }
                return TransactionResult::Value(true);
            }

            TransactionResult::Value(false)
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("WaitQueue::WakeOne"),
            do_transaction,
        )
    }

    /// Releases all threads from the wait queue.
    ///
    /// `wait_queue_error` is what [`WaitQueue::block`] should return for the blocking threads.
    #[inline]
    pub fn wake_all(&self, wait_queue_error: impl Into<Result<(), Status>>) {
        assert_no_transaction_active();
        assert_lock_not_held(self.lock());
        let wait_queue_error = wait_queue_error.into();

        // `wake_all` is a bit tricky.  In order to preserve the behavior as it was
        // before the global `ThreadLock` was removed, we need to obtain a set of locks
        // which includes not just the `WaitQueue`'s lock, but also all of the locks of
        // all of the threads currently waiting in the queue.  This might eventually
        // become a contention bottleneck in some situations; the only good thing which
        // can be said here is that fair nature of the `ChainLock` means that we will
        // _eventually_ succeed.
        //
        // TODO(johngro): Figure out if it is OK to relax the previous behavior of
        // wake all.  In the past, at the instant we held the `ThreadLock`, we fixed a
        // set of threads who were in our `WaitQueue`, and those were the threads that
        // we were going to wake.
        //
        // In an effort to increase concurrency, we could adopt an approach where only
        // one thread's lock was held at any given point in time, minimizing the
        // chance of conflict.  The downside to this approach is that after
        // successfully locking and waking one thread, if we need to wake a second
        // thread, we may need to back off, dropping the queue lock in the process.
        // The result of this is that our `wake_all` operation is no longer atomic.  It
        // just wakes threads (including threads who happen to join the `WaitQueue`
        // during the `wake_all` operation) until it finally observes zero threads in the
        // queue.

        // TODO(johngro): Should we suppress remote rescheduling for this operation
        // too?  Seems like it might be wise given that we could be waking a lot of
        // threads.  Then again, we drop the thread's locks as soon as we can, so it
        // might not be required.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            let mut guard = OwnerGuard::new(self, token);

            // Note(johngro): See the note in `wake_one`.  No one should ever be calling
            // this method on an `OwnedWaitQueue`.
            debug_assert!(self.magic() == MAGIC);
            if WAIT_QUEUE_VALIDATION {
                guard.proof().validate_queue();
            }

            // If the collection is empty, there is nothing left to do.
            if guard.proof().is_empty() {
                guard.release();
                return DONE.into();
            }

            pin_init::stack_pin_init!(let unblock_list = ThreadUnblockList::init());
            // SAFETY: The active transaction is not finalized, no waiter thread locks are held,
            // and `unblock_list` is newly initialized and empty. If `true` is returned, we
            // finalize the transaction and pass `unblock_list` to `scheduler::unblock_list`.
            let locked = unsafe {
                WaitQueueLockOps::lock_for_wake_all_in_queue(
                    &mut guard.proof(),
                    wait_queue_error,
                    unblock_list.as_mut(),
                    token,
                )
            };
            // Whether we locked our threads and got a list back or not, we can drop the queue
            // lock.  We are going to either unblock our threads, or loop back around to try again.
            guard.release();

            if locked {
                // Now that we have all of the threads locked, we are committed to the
                // `wake_all` operation and can unblock our threads.
                ChainLockTransaction::finalize(token);
                // SAFETY: All threads in `unblock_list` were locked and dequeued by
                // `lock_for_wake_all_in_queue`, and the active transaction is finalized.
                unsafe {
                    scheduler::unblock_list(unblock_list, token);
                }
                return DONE.into();
            }
            Action::Backoff.into()
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("WaitQueue::WakeAll"),
            do_transaction,
        );
    }

    /// Moves the specified thread `t` from the `source` wait queue to the `dest` wait queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `source.owner()` and `dest.owner()` are distinct wait queues
    /// and that `t.owner()` is currently blocked in `source.owner()`.
    #[inline]
    pub unsafe fn move_thread(
        source: &mut ChainLockProof<'_, Self>,
        dest: &mut ChainLockProof<'_, Self>,
        t: &mut ChainLockProof<'_, Thread>,
    ) {
        source.owner().debug_assert_magic_and_not_owq();
        dest.owner().debug_assert_magic_and_not_owq();

        if WAIT_QUEUE_VALIDATION {
            source.validate_queue();
            dest.validate_queue();
        }

        debug_assert!(!core::ptr::eq(source.owner(), dest.owner()));
        assert_in_wait_queue(t, &source.as_base());
        debug_assert!(source.collection().count() > 0);

        // SAFETY: `source`, `dest`, and `t` prove all three locks are held, and the caller
        // guarantees `t.owner()` is currently blocked in `source.owner()`. Removing `t` from
        // `source` resets its sort key and subtree min deadline before inserting into `dest`.
        unsafe {
            source.collection_mut().remove(t);
            dest.collection_mut().insert(t);
        }
        let dest_base: &WaitQueueBase = dest.owner();
        let dest_ptr = NonNull::from(dest_base);
        // SAFETY: `t.owner()` was just inserted into `dest.owner()`'s collection, and
        // `dest.owner()` is a plain `WaitQueue` (asserted above), which stays live while threads
        // are blocked in it.
        unsafe { t.wait_queue_state().set_blocking_wait_queue(Some(dest_ptr), t.token_mut()) };
    }
}

/// C++ FFI entry point for `WaitQueueBase::~WaitQueueBase()`.
///
/// # Safety
///
/// `wait_queue` must be a valid non-null pointer to a live [`WaitQueueBase`] being destroyed,
/// with exclusive access and no threads currently blocked in it.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_destroy(wait_queue: *mut bindings::WaitQueueBase) {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to a valid `WaitQueueBase` with exclusive
    // access to `magic` and `collection` during teardown. Using `&WaitQueueBase` avoids forming a
    // `Unique` (`&mut`) borrow over the queue if a waking CPU is still unwinding from
    // `scheduler::unblock`.
    let wq: &WaitQueueBase = unsafe { &*wait_queue.cast::<WaitQueueBase>() };
    // SAFETY: `wq` is being torn down with no remaining waiters or concurrent mutations.
    unsafe {
        wq.teardown();
    }
}

/// C++ FFI entry point for `WaitQueueBase::magic()`.
///
/// # Safety
///
/// `wait_queue` must be a valid non-null pointer to an initialized [`WaitQueueBase`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_magic(
    wait_queue: *const bindings::WaitQueueBase,
) -> u32 {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to an initialized `WaitQueueBase`.
    let wq: &WaitQueueBase = unsafe { &*wait_queue.cast::<WaitQueueBase>() };
    wq.magic()
}

/// C++ FFI entry point for `WaitQueueBase::IsEmpty()`.
///
/// # Safety
///
/// `wait_queue` must be a valid non-null pointer to an initialized [`WaitQueueBase`], and its
/// collection must be synchronized (either by holding `wait_queue`'s lock, holding the owning
/// thread's lock, or when the queue is quiescent in a pool).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_is_empty(
    wait_queue: *const bindings::WaitQueueBase,
) -> bool {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to an initialized `WaitQueueBase` with
    // synchronized access to its collection.
    let wq: &WaitQueueBase = unsafe { &*wait_queue.cast::<WaitQueueBase>() };
    // SAFETY: Synchronized access to `wq` is guaranteed by the caller (`TA_REQ_SHARED`,
    // `TA_ASSERT_SHARED`, or `TA_NO_THREAD_SAFETY_ANALYSIS` in C++).
    let proof = unsafe { ChainLockProof::assume_held_unchecked(wq) };
    proof.is_empty()
}

/// C++ FFI entry point for `WaitQueueBase::Count()`.
///
/// # Safety
///
/// `wait_queue` must be a valid non-null pointer to an initialized [`WaitQueueBase`], and its
/// collection must be synchronized (either by holding `wait_queue`'s lock, holding the owning
/// thread's lock, or when the queue is quiescent in a pool).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_count(
    wait_queue: *const bindings::WaitQueueBase,
) -> u32 {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to an initialized `WaitQueueBase` with
    // synchronized access to its collection.
    let wq: &WaitQueueBase = unsafe { &*wait_queue.cast::<WaitQueueBase>() };
    // SAFETY: Synchronized access to `wq` is guaranteed by the caller.
    let proof = unsafe { ChainLockProof::assume_held_unchecked(wq) };
    proof.count()
}

/// C++ FFI entry point for `WaitQueueBase::UnblockThread()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue` and `t` are valid non-null pointers to a live [`WaitQueueBase`] and [`Thread`].
/// - An active [`ChainLockTransaction`] is finalized on the current CPU with preemption disabled.
/// - Both `wait_queue`'s lock and `t`'s lock (as well as any downstream `ChainLock`s in `t`'s PI
///   graph) are held by the active transaction, and ownership of releasing them is transferred to
///   this function.
/// - All other preconditions of [`WaitQueueBase::unblock_thread`] are satisfied.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_unblock_thread(
    wait_queue: *mut bindings::WaitQueueBase,
    t: *mut bindings::Thread,
    wait_queue_error: zx_status_t,
) -> zx_status_t {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!t.is_null());
    // SAFETY: The caller guarantees `wait_queue` and `t` are valid pointers to a live
    // `WaitQueueBase` and `Thread`, an active `ChainLockTransaction` is finalized with preemption
    // disabled, and both `wait_queue`'s lock and `t`'s lock (plus any downstream PI chain locks)
    // are held and transferred to this function to release.
    unsafe {
        let wq: &WaitQueueBase = &*wait_queue.cast::<WaitQueueBase>();
        let thread: &Thread = &*t.cast::<Thread>();
        let token = ChainLockTransaction::mark_active();
        let preempt_disabled = PreemptDisabledToken::new();
        let wq_guard = OwnerGuard::adopt(wq, &token);
        let t_guard = OwnerGuard::adopt(thread, &token);
        Status::result_into_raw(WaitQueueBase::unblock_thread(
            wq_guard,
            t_guard,
            Status::ok(wait_queue_error),
            &token,
            &preempt_disabled,
        ))
    }
}

/// C++ FFI entry point for `WaitQueueBase::PeekFront()`.
///
/// # Safety
///
/// `wait_queue` must be a valid non-null pointer to an initialized [`WaitQueueBase`] whose lock is
/// held by the caller.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_peek_front(
    wait_queue: *const bindings::WaitQueueBase,
) -> *mut bindings::Thread {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to an initialized `WaitQueueBase` with its
    // lock held.
    let wq: &WaitQueueBase = unsafe { &*wait_queue.cast::<WaitQueueBase>() };
    // SAFETY: The caller holds `wq.lock()`.
    let proof = unsafe { ChainLockProof::assume_held_unchecked(wq) };
    proof.peek_front().map_or(core::ptr::null_mut(), |t| t.as_raw().cast())
}

/// C++ FFI entry point for `WaitQueueBase::BlockEtcPreamble()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue`, `current_thread`, and `deadline` are valid non-null pointers to a live
///   [`WaitQueueBase`], [`Thread`], and [`Deadline`].
/// - Both `wait_queue`'s lock and `current_thread`'s lock are exclusively held by the caller.
/// - All other preconditions of [`ChainLockProof::block_etc_preamble`] are satisfied.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_block_etc_preamble(
    wait_queue: *mut bindings::WaitQueueBase,
    current_thread: *mut bindings::Thread,
    deadline: *const bindings::Deadline,
    signal_mask: u32,
    reason: bindings::ResourceOwnership,
    interruptible: bindings::Interruptible,
) -> zx_status_t {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!current_thread.is_null());
    debug_assert!(!deadline.is_null());
    // SAFETY: The caller guarantees `wait_queue`, `current_thread`, and `deadline` are valid,
    // both `wait_queue`'s lock and `current_thread`'s lock are held, and the blocking preamble
    // preconditions are satisfied.
    unsafe {
        let wq: &WaitQueueBase = &*wait_queue.cast::<WaitQueueBase>();
        let thread: &Thread = &*current_thread.cast::<Thread>();
        let dl: &Deadline = &*deadline.cast::<Deadline>();
        let mut wq_proof = ChainLockProof::assume_held_unchecked(wq);
        let mut t_proof = ChainLockProof::assume_held_unchecked(thread);
        Status::result_into_raw(wq_proof.block_etc_preamble(
            &mut t_proof,
            dl,
            signal_mask,
            reason.into(),
            interruptible.into(),
        ))
    }
}

/// C++ FFI entry point for `WaitQueueBase::BlockEtcPostamble()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue`, `current_thread`, and `deadline` are valid non-null pointers to a
///   [`WaitQueueBase`], [`Thread`], and [`Deadline`].
/// - An active [`ChainLockTransaction`] is finalized on the current CPU with `current_thread`'s
///   lock held as the only held chain lock and `wait_queue`'s lock not held.
/// - All other preconditions of [`WaitQueueBase::block_etc_postamble`] are satisfied.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_block_etc_postamble(
    wait_queue: *const bindings::WaitQueueBase,
    current_thread: *mut bindings::Thread,
    deadline: *const bindings::Deadline,
) -> zx_status_t {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!current_thread.is_null());
    debug_assert!(!deadline.is_null());
    // SAFETY: The caller guarantees `wait_queue`, `current_thread`, and `deadline` are valid,
    // an active transaction is finalized with `current_thread`'s lock held and `wait_queue`'s
    // lock not held, and the blocking postamble preconditions are satisfied.
    unsafe {
        let thread: &Thread = &*current_thread.cast::<Thread>();
        let dl: &Deadline = &*deadline.cast::<Deadline>();
        let token = ChainLockTransaction::mark_active();
        let t_proof = ChainLockProof::assume_held_unchecked(thread);
        Status::result_into_raw(WaitQueueBase::block_etc_postamble(
            wait_queue.cast::<WaitQueueBase>(),
            t_proof,
            dl,
            &token,
        ))
    }
}

/// C++ FFI entry point for `WaitQueueBase::Dequeue()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue` and `t` are valid non-null pointers to a live [`WaitQueueBase`] and [`Thread`].
/// - Both `wait_queue`'s lock and `t`'s lock are exclusively held by the caller.
/// - `t` is currently blocked in `wait_queue`, and if `wait_queue` is an [`OwnedWaitQueue`], the
///   caller updates its `inherited_scheduler_state_storage` before releasing `wait_queue`'s lock.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_dequeue(
    wait_queue: *mut bindings::WaitQueueBase,
    t: *mut bindings::Thread,
    wait_queue_error: zx_status_t,
) {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!t.is_null());
    // SAFETY: The caller guarantees `wait_queue` and `t` are valid, both locks are held, `t` is
    // currently blocked in `wait_queue`, and the `OwnedWaitQueue`
    // `inherited_scheduler_state_storage` invariant is maintained by the caller.
    unsafe {
        let wq: &WaitQueueBase = &*wait_queue.cast::<WaitQueueBase>();
        let thread: &Thread = &*t.cast::<Thread>();
        let mut wq_proof = ChainLockProof::assume_held_unchecked(wq);
        let mut t_proof = ChainLockProof::assume_held_unchecked(thread);
        wq_proof.dequeue(&mut t_proof, Status::ok(wait_queue_error));
    }
}

/// C++ FFI entry point for `WaitQueueBase::ValidateQueue()`.
///
/// # Safety
///
/// `wait_queue` must be a valid non-null pointer to an initialized [`WaitQueueBase`] whose lock is
/// held by the caller.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_validate_queue(
    wait_queue: *const bindings::WaitQueueBase,
) {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to an initialized `WaitQueueBase` with its
    // lock held.
    let wq: &WaitQueueBase = unsafe { &*wait_queue.cast::<WaitQueueBase>() };
    // SAFETY: The caller holds `wq.lock()`.
    let proof = unsafe { ChainLockProof::assume_held_unchecked(wq) };
    proof.validate_queue();
}

/// C++ FFI entry point for `WaitQueueBase::TimeoutHandler()`.
///
/// # Safety
///
/// The caller must uphold all preconditions of [`WaitQueueBase::timeout_handler`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_timeout_handler(
    timer: *mut Timer,
    now: zx_instant_mono_t,
    arg: *mut c_void,
) {
    // SAFETY: The caller guarantees `timer` and `arg` are valid for
    // `WaitQueueBase::timeout_handler` and no transaction is active.
    unsafe {
        WaitQueueBase::timeout_handler(timer, InstantMono(now), arg);
    }
}

/// C++ FFI entry point for `WaitQueueBase::UpdateBlockedThreadEffectiveProfile()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue` and `t` are valid non-null pointers to a live [`WaitQueueBase`] and [`Thread`].
/// - Both `wait_queue`'s lock and `t`'s lock are exclusively held by the caller.
/// - `t` is currently blocked in `wait_queue`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_base_update_blocked_thread_effective_profile(
    wait_queue: *mut bindings::WaitQueueBase,
    t: *mut bindings::Thread,
) {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!t.is_null());
    // SAFETY: The caller guarantees `wait_queue` and `t` are valid, both locks are held, and `t`
    // is currently blocked in `wait_queue`.
    unsafe {
        let wq: &WaitQueueBase = &*wait_queue.cast::<WaitQueueBase>();
        let thread: &Thread = &*t.cast::<Thread>();
        let mut wq_proof = ChainLockProof::assume_held_unchecked(wq);
        let mut t_proof = ChainLockProof::assume_held_unchecked(thread);
        wq_proof.update_blocked_thread_effective_profile(&mut t_proof);
    }
}

/// C++ FFI entry point for `WaitQueue::BlockEtc()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue`, `current_thread`, and `deadline` are valid non-null pointers to a live
///   [`WaitQueue`] (valid while `current_thread` remains blocked in it), [`Thread`], and
///   [`Deadline`].
/// - An active [`ChainLockTransaction`] is finalized on the current CPU with `wait_queue`'s lock
///   and `current_thread`'s lock as the only held chain locks (`wait_queue`'s lock is released by
///   this call).
/// - All other preconditions of [`WaitQueue::block_etc_preamble`] and
///   [`WaitQueueBase::block_etc_postamble`] are satisfied.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_block_etc(
    wait_queue: *mut bindings::WaitQueue,
    current_thread: *mut bindings::Thread,
    deadline: *const bindings::Deadline,
    signal_mask: u32,
    reason: bindings::ResourceOwnership,
    interruptible: bindings::Interruptible,
) -> zx_status_t {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!current_thread.is_null());
    debug_assert!(!deadline.is_null());
    // SAFETY:
    // - `wait_queue`, `current_thread`, and `deadline` are valid pointers per caller contract.
    //   `wq` and `wq_guard` are created and consumed entirely inside the preamble block with a
    //   separate `preamble_token` so that no `&WaitQueue` borrow lives across
    //   `WaitQueueBase::block_etc_postamble`.
    // - An active transaction is finalized on the current CPU (`mark_active`) with `wait_queue`'s
    //   lock (`OwnerGuard::adopt`) and `current_thread`'s lock (`assume_held_unchecked`) held.
    // - All blocking preconditions of `WaitQueue::block_etc_preamble` and
    //   `WaitQueueBase::block_etc_postamble` are upheld by the caller.
    let res = unsafe {
        let thread: &Thread = &*current_thread.cast::<Thread>();
        let dl: &Deadline = &*deadline.cast::<Deadline>();
        let mut t_proof = ChainLockProof::assume_held_unchecked(thread);
        let this = {
            let wq: &WaitQueue = &*wait_queue.cast::<WaitQueue>();
            let preamble_token = ChainLockTransaction::mark_active();
            let wq_guard = OwnerGuard::adopt(wq, &preamble_token);
            WaitQueue::block_etc_preamble(
                wq_guard,
                &mut t_proof,
                dl,
                signal_mask,
                reason.into(),
                interruptible.into(),
                &preamble_token,
            )
        };
        let postamble_token = ChainLockTransaction::mark_active();
        this.and_then(|this| {
            WaitQueueBase::block_etc_postamble(this, t_proof, dl, &postamble_token)
        })
    };
    Status::result_into_raw(res)
}

/// C++ FFI entry point for `WaitQueue::WakeOne()`.
///
/// # Safety
///
/// The caller must ensure that `wait_queue` is a valid non-null pointer to an initialized
/// [`WaitQueue`], no [`ChainLockTransaction`] is active on the current CPU, and `wait_queue`'s
/// lock is not held by the current CPU.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_wake_one(
    wait_queue: *mut bindings::WaitQueue,
    wait_queue_error: zx_status_t,
) -> bool {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to an initialized `WaitQueue`.
    let wq: &WaitQueue = unsafe { &*wait_queue.cast::<WaitQueue>() };
    wq.wake_one(Status::ok(wait_queue_error))
}

/// C++ FFI entry point for `WaitQueue::WakeAll()`.
///
/// # Safety
///
/// The caller must ensure that `wait_queue` is a valid non-null pointer to an initialized
/// [`WaitQueue`], no [`ChainLockTransaction`] is active on the current CPU, and `wait_queue`'s
/// lock is not held by the current CPU.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_wake_all(
    wait_queue: *mut bindings::WaitQueue,
    wait_queue_error: zx_status_t,
) {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: The caller guarantees `wait_queue` points to an initialized `WaitQueue`.
    let wq: &WaitQueue = unsafe { &*wait_queue.cast::<WaitQueue>() };
    wq.wake_all(Status::ok(wait_queue_error));
}

/// C++ FFI entry point for `WaitQueue::WakeOneLocked()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue` is a valid non-null pointer to a live [`WaitQueue`].
/// - `out_woken` is a valid non-null, properly aligned, unaliased pointer writable for a `bool`.
/// - An active, unfinalized [`ChainLockTransaction`] is in progress on the current CPU with
///   preemption disabled and `wait_queue`'s lock exclusively held (and no waiter thread's lock
///   held).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_wake_one_locked(
    wait_queue: *mut bindings::WaitQueue,
    wait_queue_error: zx_status_t,
    out_woken: *mut bool,
) -> bool {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!out_woken.is_null());
    // SAFETY: The caller guarantees `wait_queue` is valid, an active unfinalized transaction is in
    // progress with preemption disabled, and `wait_queue`'s lock is exclusively held.
    let mut proof = unsafe {
        let wq: &WaitQueue = &*wait_queue.cast::<WaitQueue>();
        ChainLockProof::assume_held_unchecked(wq)
    };
    // SAFETY: The caller guarantees an active transaction is in progress with preemption disabled.
    let (token, preempt_disabled) =
        unsafe { (ChainLockTransaction::mark_active(), PreemptDisabledToken::new()) };
    let Some(woken) =
        proof.wake_one_locked(Status::ok(wait_queue_error), &token, &preempt_disabled)
    else {
        return false;
    };
    // SAFETY: The caller guarantees `out_woken` is valid for writes, properly aligned, and
    // unaliased.
    unsafe {
        *out_woken = woken;
    }
    true
}

/// C++ FFI entry point for `WaitQueue::WakeAllLocked()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue` is a valid non-null pointer to a live [`WaitQueue`].
/// - `out_woken` is a valid non-null, properly aligned, unaliased pointer writable for a `u32`.
/// - An active, unfinalized [`ChainLockTransaction`] is in progress on the current CPU with
///   preemption disabled and `wait_queue`'s lock exclusively held (and no waiter thread's lock
///   held).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_wake_all_locked(
    wait_queue: *mut bindings::WaitQueue,
    wait_queue_error: zx_status_t,
    out_woken: *mut u32,
) -> bool {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!out_woken.is_null());
    // SAFETY: The caller guarantees `wait_queue` is valid, an active unfinalized transaction is in
    // progress with preemption disabled, and `wait_queue`'s lock is exclusively held.
    let mut proof = unsafe {
        let wq: &WaitQueue = &*wait_queue.cast::<WaitQueue>();
        ChainLockProof::assume_held_unchecked(wq)
    };
    // SAFETY: The caller guarantees an active transaction is in progress with preemption disabled.
    let (token, preempt_disabled) =
        unsafe { (ChainLockTransaction::mark_active(), PreemptDisabledToken::new()) };
    let Some(woken) =
        proof.wake_all_locked(Status::ok(wait_queue_error), &token, &preempt_disabled)
    else {
        return false;
    };
    // SAFETY: The caller guarantees `out_woken` is valid for writes, properly aligned, and
    // unaliased.
    unsafe {
        *out_woken = woken;
    }
    true
}

/// C++ FFI entry point for `WaitQueue::DequeueThread()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `wait_queue` and `t` are valid non-null pointers to a live [`WaitQueue`] and [`Thread`].
/// - Both `wait_queue`'s lock and `t`'s lock are exclusively held by the caller.
/// - `t` is currently blocked in `wait_queue`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_dequeue_thread(
    wait_queue: *mut bindings::WaitQueue,
    t: *mut bindings::Thread,
    wait_queue_error: zx_status_t,
) {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!t.is_null());
    // SAFETY:
    // - `wait_queue` and `t` are valid pointers to a live `WaitQueue` and `Thread` per caller
    //   contract.
    // - Both locks are exclusively held by the caller (`assume_held_unchecked`).
    // - `t` is currently blocked in `wait_queue` (`dequeue_thread`).
    unsafe {
        let wq: &WaitQueue = &*wait_queue.cast::<WaitQueue>();
        let thread: &Thread = &*t.cast::<Thread>();
        let mut wq_proof = ChainLockProof::assume_held_unchecked(wq);
        let mut t_proof = ChainLockProof::assume_held_unchecked(thread);
        wq_proof.dequeue_thread(&mut t_proof, Status::ok(wait_queue_error));
    }
}

/// C++ FFI entry point for `WaitQueue::MoveThread()`.
///
/// # Safety
///
/// The caller must ensure that:
/// - `source`, `dest`, and `t` are valid non-null pointers to two distinct live [`WaitQueue`]s
///   and a live [`Thread`].
/// - `source`'s lock, `dest`'s lock, and `t`'s lock are all exclusively held by the caller.
/// - `t` is currently blocked in `source`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/wait.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_wait_queue_move_thread(
    source: *mut bindings::WaitQueue,
    dest: *mut bindings::WaitQueue,
    t: *mut bindings::Thread,
) {
    debug_assert!(!source.is_null());
    debug_assert!(!dest.is_null());
    debug_assert!(!t.is_null());
    // SAFETY:
    // - `source`, `dest`, and `t` are valid pointers to two distinct live `WaitQueue`s and a live
    //   `Thread` per caller contract.
    // - All three locks are exclusively held by the caller (`assume_held_unchecked`).
    // - `t` is currently blocked in `source` (`move_thread`).
    unsafe {
        let src_wq: &WaitQueue = &*source.cast::<WaitQueue>();
        let dst_wq: &WaitQueue = &*dest.cast::<WaitQueue>();
        let thread: &Thread = &*t.cast::<Thread>();
        let mut src_proof = ChainLockProof::assume_held_unchecked(src_wq);
        let mut dst_proof = ChainLockProof::assume_held_unchecked(dst_wq);
        let mut t_proof = ChainLockProof::assume_held_unchecked(thread);
        WaitQueue::move_thread(&mut src_proof, &mut dst_proof, &mut t_proof);
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
        // SAFETY: `MAGIC` is a valid magic value for a standalone `WaitQueueBase`.
        pin_init::stack_pin_init!(
            let base_only = unsafe { WaitQueueBase::init_with_magic(MAGIC) }
        );
        expect_true!(base_only.lock().is_unlocked());
        expect_eq!(base_only.magic(), MAGIC);

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
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("wait_queue_wake_one_locked"),
            |token| {
                // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the
                // duration of this transaction.
                let preempt_disabled = unsafe { PreemptDisabledToken::new() };
                let mut guard = OwnerGuard::new(wq_ref, token);
                let mut proof = guard.proof();
                wake_one_res = proof.wake_one_locked(Ok(()), token, &preempt_disabled);
                ChainLockTransaction::finalize(token);
                guard.release();
                DONE
            },
        );
        expect_true!(wake_one_res == Some(false));
        expect_true!(wq_ref.lock().is_unlocked());

        let mut wake_all_res = None;
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("wait_queue_wake_all_locked"),
            |token| {
                // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the
                // duration of this transaction.
                let preempt_disabled = unsafe { PreemptDisabledToken::new() };
                let mut guard = OwnerGuard::new(wq_ref, token);
                let mut proof = guard.proof();
                wake_all_res = proof.wake_all_locked(Ok(()), token, &preempt_disabled);
                guard.release();
                DONE
            },
        );
        expect_true!(wake_all_res == Some(0));
        expect_true!(wq_ref.lock().is_unlocked());
    }
}
