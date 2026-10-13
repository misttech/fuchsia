// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kconcurrent::ChainLock;
use crate::kernel::owned_wait_queue::OWNED_MAGIC;
use crate::kernel::scheduler_state::WaitQueueInheritedSchedulerState;
use crate::kernel::thread::Thread;
use crate::platform_rs::timer::{DurationMono, InstantMono};
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::marker::PhantomPinned;
use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use pin_init::{PinInit, pin_data, pinned_drop};
use wait_queue_bindings as bindings;
use zr::static_assert;
use zx_types::zx_status_t;

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
    // Safety invariant: Guarded by the enclosing `Thread`'s `ChainLock`. Points to the active
    // `WaitQueueBase` while `blocked_threads_tree_node.in_container()` is true, and is `None`
    // otherwise (including when dropped).
    blocking_wait_queue: UnsafeCell<Option<NonNull<WaitQueueBase>>>,
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
    // Safety invariant: Guarded by the enclosing `Thread`'s `ChainLock`.
    blocked_status: UnsafeCell<zx_status_t>,
    /// Are we allowed to be interrupted on the current thing we're blocked/sleeping on?
    // Safety invariant: Guarded by the enclosing `Thread`'s `ChainLock`.
    interruptible: UnsafeCell<Interruptible>,
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
        debug_assert!(self.blocking_wait_queue.get_mut().is_none());
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
    lock: ChainLock,
    // Safety invariant: Initialized to `MAGIC` (for `WaitQueue`) or `OWNED_MAGIC` (for the base
    // subobject of `OwnedWaitQueue`), and zeroed during `PinnedDrop::drop`.
    magic: u32,
    // Safety invariant: Guarded by `lock`. Must be empty when dropped.
    collection: UnsafeCell<WaitQueueCollection>,
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
// SAFETY: All interior mutations to `WaitQueueBase::collection` through shared references are
// synchronized by `WaitQueueBase::lock` (`ChainLock`).
unsafe impl Sync for WaitQueueBase {}

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

        let count = this.collection.get_mut().count();
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
