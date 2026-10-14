// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kconcurrent::{ChainLockProof, ChainLocked, ClassedChainLock};
use crate::kernel::owned_wait_queue::OWNED_MAGIC;
use crate::kernel::scheduler_state::WaitQueueInheritedSchedulerState;
use crate::kernel::thread::{Thread, ThreadLockClass};
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::PhantomPinned;
use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use ksync::KCell;
use pin_init::{PinInit, pin_data, pinned_drop};
use wait_queue_bindings as bindings;
use zr::static_assert;
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

/// Accessors to the fields of a [`WaitQueueBase`] guarded by its lock.
impl ChainLockProof<'_, WaitQueueBase> {
    /// Returns the collection of blocked threads.
    pub(crate) fn collection(&self) -> &WaitQueueCollection {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `collection`.
        unsafe { self.owner().collection.get(self.token()) }
    }

    /// Returns the collection of blocked threads mutably.
    pub(crate) fn collection_mut(&mut self) -> &mut WaitQueueCollection {
        let (owner, token) = self.split_mut();
        // SAFETY: As in `collection`; the unique borrow of `self` ensures no other reference to
        // `collection` is live.
        unsafe { owner.collection.get_mut(token) }
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

// SAFETY: `base.lock` guards `base.collection`, and `base` is a plain field so the same lock
// instance is returned every time.
unsafe impl ChainLocked for WaitQueue {
    type Class = WaitQueueLockClass;

    /// Returns the lock guarding this wait queue (the embedded `WaitQueueBase`'s).
    fn lock(&self) -> &ClassedChainLock<WaitQueueLockClass> {
        self.base.lock()
    }
}

/// Accessors to the fields of a [`WaitQueue`] guarded by its lock.
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
    pub fn as_raw(&self) -> *mut bindings::WaitQueue {
        (self as *const Self).cast_mut().cast()
    }

    /// Returns a raw mutable pointer to the underlying C++ `WaitQueue`.
    pub fn as_ptr(&self) -> *mut c_void {
        self.as_raw().cast()
    }
}

/// [`WaitQueue`] and [`WaitQueueBase`] unit tests.
#[cfg(ktest)]
#[unittest::suite(name = "wait_queue")]
mod tests {
    use unittest::{expect_eq, expect_true};

    use crate::kconcurrent::{
        ChainLockTransaction, DONE, IRQ_SAVE_OPTION, OwnerGuard, clt_tag,
        single_chain_lock_guard_owned,
    };

    /// Verifies `WaitQueue` and `WaitQueueBase` lock proofs and collection accessors.
    #[test]
    fn wait_queue_proof_and_base_retag() {
        pin_init::stack_pin_init!(let wq = WaitQueue::init());
        let wq_ref: &WaitQueue = &wq;
        expect_true!(wq_ref.lock().is_unlocked());

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
            {
                let mut base_proof = proof.as_base();
                expect_true!(core::ptr::eq(base_proof.owner(), &wq_ref.base));
                expect_eq!(base_proof.collection().count(), 0);
                expect_eq!(base_proof.collection_mut().count(), 0);
            }
        }
        expect_true!(wq_ref.lock().is_unlocked());

        let mut base_count = u32::MAX;
        ChainLockTransaction::until_done(IRQ_SAVE_OPTION, clt_tag!("wait_queue_base"), |token| {
            let mut guard = OwnerGuard::new(&wq_ref.base, token);
            ChainLockTransaction::finalize(token);
            let mut proof = guard.proof();
            base_count = proof.collection_mut().count();
            guard.release();
            DONE
        });
        expect_eq!(base_count, 0);
        expect_true!(wq_ref.lock().is_unlocked());
    }
}
