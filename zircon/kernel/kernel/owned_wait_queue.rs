// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::scheduler_state::WaitQueueInheritedSchedulerState;
use crate::kernel::thread::Thread;
use crate::kernel::wait_queue::WaitQueueBase;
use core::cell::UnsafeCell;
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{DoublyLinkedListContainable, DoublyLinkedListNode};
use owned_wait_queue_bindings as bindings;
use pin_init::{PinInit, pin_data, pinned_drop};
use zr::static_assert;

/// Magic value identifying an [`OwnedWaitQueue`].
pub const OWNED_MAGIC: u32 = fbl::magic(b"ownq");
static_assert!(OWNED_MAGIC == bindings::OwnedWaitQueue_kOwnedMagic);

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
}
