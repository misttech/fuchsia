// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kconcurrent::{
    ChainLockProof, ChainLockResult, ChainLockTransactionToken, ChainLocked, ClassedChainLock,
    OwnerGuard, assert_lock_not_held, assert_no_transaction_active, assert_transaction_active,
};
use crate::kernel::deadline::Deadline;
pub use crate::kernel::scheduler_state::SchedulerStateEffectiveProfile;
use crate::kernel::scheduler_state::{SchedulerStateBaseProfile, WaitQueueInheritedSchedulerState};
use crate::kernel::thread::{PreemptDisabledToken, Thread, ThreadPtr};
use crate::kernel::wait_queue::{
    ForceInheritance, Interruptible, ResourceOwnership, WaitQueueBase, WaitQueueCollection,
    WaitQueueLockClass,
};
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{DoublyLinkedListContainable, DoublyLinkedListNode};
use ksync::KCell;
use owned_wait_queue_bindings as bindings;
use pin_init::{PinInit, pin_data, pinned_drop};
use zr::static_assert;
use zx_status::Status;

/// Magic value identifying an [`OwnedWaitQueue`].
pub const OWNED_MAGIC: u32 = fbl::magic(b"ownq");
static_assert!(OWNED_MAGIC == bindings::OwnedWaitQueue_kOwnedMagic);

static_assert!(ResourceOwnership::Normal as i32 == bindings::ResourceOwnership::Normal as i32);
static_assert!(ResourceOwnership::Reader as i32 == bindings::ResourceOwnership::Reader as i32);
static_assert!(size_of::<ResourceOwnership>() == size_of::<bindings::ResourceOwnership>());
static_assert!(align_of::<ResourceOwnership>() == align_of::<bindings::ResourceOwnership>());

static_assert!(Interruptible::No as u8 == bindings::Interruptible::No as u8);
static_assert!(Interruptible::Yes as u8 == bindings::Interruptible::Yes as u8);
static_assert!(size_of::<Interruptible>() == size_of::<bindings::Interruptible>());
static_assert!(align_of::<Interruptible>() == align_of::<bindings::Interruptible>());

static_assert!(ForceInheritance::No as u8 == bindings::ForceInheritance::No as u8);
static_assert!(ForceInheritance::Yes as u8 == bindings::ForceInheritance::Yes as u8);
static_assert!(size_of::<ForceInheritance>() == size_of::<bindings::ForceInheritance>());
static_assert!(align_of::<ForceInheritance>() == align_of::<bindings::ForceInheritance>());

static_assert!(ChainLockResult::Ok as i32 == bindings::Result::Ok as i32);
static_assert!(ChainLockResult::Retry as i32 == bindings::Result::Retry as i32);
static_assert!(ChainLockResult::Backoff as i32 == bindings::Result::Backoff as i32);
static_assert!(ChainLockResult::Cycle as i32 == bindings::Result::Cycle as i32);

static_assert!(
    size_of::<SchedulerStateEffectiveProfile>()
        == size_of::<bindings::SchedulerState_EffectiveProfile>()
);
static_assert!(
    align_of::<SchedulerStateEffectiveProfile>()
        == align_of::<bindings::SchedulerState_EffectiveProfile>()
);

impl From<ResourceOwnership> for bindings::ResourceOwnership {
    #[inline]
    fn from(r: ResourceOwnership) -> Self {
        match r {
            ResourceOwnership::Normal => Self::Normal,
            ResourceOwnership::Reader => Self::Reader,
        }
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

impl From<ForceInheritance> for bindings::ForceInheritance {
    #[inline]
    fn from(f: ForceInheritance) -> Self {
        match f {
            ForceInheritance::No => Self::No,
            ForceInheritance::Yes => Self::Yes,
        }
    }
}

#[inline]
const fn chain_lock_result_from_bindings(r: bindings::Result) -> ChainLockResult {
    match r {
        bindings::Result::Ok => ChainLockResult::Ok,
        bindings::Result::Retry => ChainLockResult::Retry,
        bindings::Result::Backoff => ChainLockResult::Backoff,
        bindings::Result::Cycle => ChainLockResult::Cycle,
    }
}

/// Hook interface for injecting into wake and requeue operations to be used during thread
/// selection and to update external bookkeeping.
///
/// Instances of it are meant to be stack allocated and should always be destroyed in the scope
/// where they were created. They are never meant to be heap allocated or have their lifecycle
/// managed using an [`IWakeRequeueHook`] pointer.
///
/// # Execution Context / Panics
///
/// Both [`IWakeRequeueHook::allow`] and [`IWakeRequeueHook::on_wake_or_requeue`] are invoked from
/// C++ with interrupts disabled inside an active
/// [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) while holding the queue's
/// `ChainLock` (and, in [`IWakeRequeueHook::on_wake_or_requeue`], the target [`Thread`]'s
/// `ChainLock`). Implementations must not block, start a nested `ChainLockTransaction`, or
/// re-enter operations that require no active transaction (`TA_EXCL(chainlock_transaction_token)`),
/// such as [`OwnedWaitQueue::wake_threads`], [`OwnedWaitQueue::wake_thread_and_assign_owner`],
/// [`OwnedWaitQueue::wake_and_requeue`], or [`OwnedWaitQueue::block_and_assign_owner`]; doing so
/// will panic.
pub trait IWakeRequeueHook {
    /// The `allow` hook is called during the locking phase of a wake or a requeue operation. The
    /// locking code is holding the queue's lock at this point, but not the thread's lock yet. The
    /// thread is still a member of the queue, meaning we have read-only access to the thread's
    /// variables.
    ///
    /// The implementation may use this hook to decide if this thread should be woken or not,
    /// returning `true` if it should be and `false` otherwise. If `true` is returned, the locking
    /// operation will attempt to lock the thread in order to commit it to waking, backing off if
    /// needed. If `false` is returned, the thread will be left in the queue and the enumeration of
    /// threads in the queue ceases.
    ///
    /// `allow` may be called multiple times for the same thread during a locking operation if the
    /// code needs to back off and attempt to lock the set of threads to be woken multiple times.
    fn allow(&mut self, _t: &Thread) -> bool {
        true
    }

    /// `on_wake_or_requeue` is called in the second phase of a wake/requeue operation. At this
    /// point, all of the threads to wake have been identified and successfully locked. This hook
    /// is called _just_ before the thread is removed from its queue, and either unblocked (wake
    /// operation) or moved to a different queue (requeue).
    fn on_wake_or_requeue(&mut self, _t: &mut ChainLockProof<'_, Thread>) {}
}

/// Default no-op [`IWakeRequeueHook`] implementation.
#[derive(Debug, Default, Clone, Copy)]
pub struct DefaultWakeHooks;

impl IWakeRequeueHook for DefaultWakeHooks {}

#[repr(C)]
struct WakeRequeueHookProxyContext<'a> {
    hook: &'a mut dyn IWakeRequeueHook,
}

/// C++ FFI callback that forwards `OwnedWaitQueue::IWakeRequeueHook::Allow` to a Rust
/// [`IWakeRequeueHook::allow`] implementation.
///
/// # Safety
///
/// - Interrupts must be disabled on the current CPU.
/// - `context` must point to a valid [`WakeRequeueHookProxyContext`] with exclusive access for the
///   duration of this call.
/// - `t` must point to a valid [`Thread`] that is currently a member of the wait queue whose lock
///   is held in an active [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction)
///   (providing shared read access to `t`).
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn rust_owned_wait_queue_wake_requeue_hook_allow(
    context: *mut c_void,
    t: *const Thread,
) -> bool {
    debug_assert!(!context.is_null());
    debug_assert!(!t.is_null());
    debug_assert!(
        crate::arch_rs::ints_disabled()
            && crate::kconcurrent::ChainLockTransaction::active().is_some()
    );
    // SAFETY: The caller guarantees `context` points to a valid `WakeRequeueHookProxyContext`
    // with exclusive access for the duration of this call.
    let ctx = unsafe { &mut *context.cast::<WakeRequeueHookProxyContext<'_>>() };
    // SAFETY: The caller guarantees `t` points to a valid `Thread` for the duration of this call,
    // and `Thread` uses `OpaqueFacade` for interior mutability.
    let thread = unsafe { &*t };
    ctx.hook.allow(thread)
}

/// C++ FFI callback that forwards `OwnedWaitQueue::IWakeRequeueHook::OnWakeOrRequeue` to a Rust
/// [`IWakeRequeueHook::on_wake_or_requeue`] implementation.
///
/// # Safety
///
/// - Interrupts must be disabled on the current CPU.
/// - `context` must point to a valid [`WakeRequeueHookProxyContext`] with exclusive access for the
///   duration of this call.
/// - `t` must point to a valid [`Thread`] whose `ChainLock` (`t.lock()`) is exclusively held in
///   the active [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) for the
///   duration of this call, with no other [`ChainLockProof<'_, Thread>`] or
///   [`OwnerGuard<'_, Thread>`] for `t` live during this call.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn rust_owned_wait_queue_wake_requeue_hook_on_wake_or_requeue(
    context: *mut c_void,
    t: *mut Thread,
) {
    debug_assert!(!context.is_null());
    debug_assert!(!t.is_null());
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: The C++ caller invokes `OnWakeOrRequeue` inside an active `ChainLockTransaction`
    // with interrupts disabled.
    let clt_token = unsafe { crate::kconcurrent::ChainLockTransaction::mark_active() };
    // SAFETY: The caller guarantees `t` points to a valid `Thread` for the duration of this call,
    // and `Thread` uses `OpaqueFacade` for interior mutability.
    let thread = unsafe { &*t };
    // SAFETY: The caller guarantees `thread.lock()` is exclusively held in the active transaction
    // for the duration of this call, and no other `ChainLockProof<'_, Thread>` or
    // `OwnerGuard<'_, Thread>` for `thread` is live.
    let mut proof = unsafe { ChainLockProof::assume_held(thread, &clt_token) };
    // SAFETY: The caller guarantees `context` points to a valid `WakeRequeueHookProxyContext`
    // with exclusive access for the duration of this call.
    let ctx = unsafe { &mut *context.cast::<WakeRequeueHookProxyContext<'_>>() };
    ctx.hook.on_wake_or_requeue(&mut proof);
}

/// A small struct which contains details about the locking used in order to prepare for a
/// [`OwnedWaitQueue::block_and_assign_owner`] operation.
#[repr(C)]
#[must_use]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BAAOLockingDetails {
    new_owner: Option<ThreadPtr>,
    stop_point: *const c_void,
}

static_assert!(
    size_of::<BAAOLockingDetails>() == size_of::<bindings::OwnedWaitQueue_BAAOLockingDetails>()
);
static_assert!(
    align_of::<BAAOLockingDetails>() == align_of::<bindings::OwnedWaitQueue_BAAOLockingDetails>()
);
static_assert!(
    offset_of!(BAAOLockingDetails, new_owner)
        == offset_of!(bindings::OwnedWaitQueue_BAAOLockingDetails, new_owner)
);
static_assert!(
    offset_of!(BAAOLockingDetails, stop_point)
        == offset_of!(bindings::OwnedWaitQueue_BAAOLockingDetails, stop_point)
);

impl BAAOLockingDetails {
    /// Creates a new `BAAOLockingDetails` with the given `initial_new_owner`.
    #[inline]
    const fn new(initial_new_owner: Option<ThreadPtr>) -> Self {
        Self { new_owner: initial_new_owner, stop_point: core::ptr::null() }
    }

    /// Returns the validated new owner recorded during the BAAO lock operation, if any.
    #[inline]
    pub const fn new_owner(&self) -> Option<ThreadPtr> {
        self.new_owner
    }
}

impl Default for BAAOLockingDetails {
    #[inline]
    fn default() -> Self {
        Self::new(None)
    }
}

/// Alias for [`BAAOLockingDetails`], matching C++ `OwnedWaitQueue::ReplaceOwnerLockingDetails`.
pub type ReplaceOwnerLockingDetails = BAAOLockingDetails;

/// A structure which holds the state of the queue as observed just before the queue lock was
/// dropped. Used by futex and kernel mutex implementations to update their bookkeeping before
/// unwinding after a wake operation.
#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct WakeThreadsResult {
    /// The number of threads woken from the wait queue.
    pub woken: u32,
    /// The number of threads still waiting in the wait queue after the wake operation.
    pub still_waiting: u32,
    /// The owner of the wait queue as observed at the end of the wake operation.
    ///
    /// # Safety Note
    ///
    /// For [`OwnedWaitQueue::wake_threads`], [`OwnedWaitQueue::wake_thread_and_assign_owner`], and
    /// [`OwnedWaitQueue::wake_and_requeue`], the queue lock and [`ChainLockTransaction`] have
    /// already been released by the time [`WakeThreadsResult`] is returned. Consequently, `owner`
    /// is only a historical snapshot: the thread may have exited and been destroyed unless the
    /// caller independently guarantees it remains alive. Do not call [`ThreadPtr::as_ref`] on
    /// `owner` without an independent liveness guarantee.
    pub owner: Option<ThreadPtr>,
}

static_assert!(
    size_of::<WakeThreadsResult>() == size_of::<bindings::OwnedWaitQueue_WakeThreadsResult>()
);
static_assert!(
    align_of::<WakeThreadsResult>() == align_of::<bindings::OwnedWaitQueue_WakeThreadsResult>()
);
static_assert!(
    offset_of!(WakeThreadsResult, woken)
        == offset_of!(bindings::OwnedWaitQueue_WakeThreadsResult, woken)
);
static_assert!(
    offset_of!(WakeThreadsResult, still_waiting)
        == offset_of!(bindings::OwnedWaitQueue_WakeThreadsResult, still_waiting)
);
static_assert!(
    offset_of!(WakeThreadsResult, owner)
        == offset_of!(bindings::OwnedWaitQueue_WakeThreadsResult, owner)
);

impl Default for WakeThreadsResult {
    #[inline]
    fn default() -> Self {
        Self { woken: 0, still_waiting: 0, owner: None }
    }
}

/// Behavior control for wake/requeue operations.
#[repr(i32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum WakeOption {
    None,
    AssignOwner,
}

static_assert!(WakeOption::None as i32 == bindings::OwnedWaitQueue_WakeOption::None as i32);
static_assert!(
    WakeOption::AssignOwner as i32 == bindings::OwnedWaitQueue_WakeOption::AssignOwner as i32
);
static_assert!(size_of::<WakeOption>() == size_of::<bindings::OwnedWaitQueue_WakeOption>());
static_assert!(align_of::<WakeOption>() == align_of::<bindings::OwnedWaitQueue_WakeOption>());

impl From<WakeOption> for bindings::OwnedWaitQueue_WakeOption {
    #[inline]
    fn from(option: WakeOption) -> Self {
        match option {
            WakeOption::None => Self::None,
            WakeOption::AssignOwner => Self::AssignOwner,
        }
    }
}

/// Pinned opaque wrapper around C++ `Thread::UnblockList`.
///
/// C++ `Thread::UnblockList` (`fbl::DoublyLinkedListCustomTraits<Thread*, ...>`) is
/// self-referential: its end sentinel encodes the address of the list object itself
/// (`(uintptr_t)this | 1`). Moving it by value in Rust (`memcpy`) without running the C++ move
/// constructor would invalidate its sentinel. Consequently, `ThreadUnblockList` is pinned in place
/// via [`ThreadUnblockList::init`].
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct ThreadUnblockList {
    raw: zr::Opaque<bindings::Thread_UnblockList>,
}

static_assert!(size_of::<ThreadUnblockList>() == size_of::<bindings::Thread_UnblockList>());
static_assert!(align_of::<ThreadUnblockList>() == align_of::<bindings::Thread_UnblockList>());

impl ThreadUnblockList {
    /// Returns an in-place pinned initializer for a new, empty `ThreadUnblockList`.
    #[inline]
    pub fn init() -> impl PinInit<Self, Infallible> {
        // SAFETY: `bindings::cpp_thread_unblock_list_init` placement-new initializes a valid,
        // empty `Thread::UnblockList` in-place in the provided slot.
        zr::pin_init_ffi!(bindings::cpp_thread_unblock_list_init)
    }

    /// Domain-specific conversion: returns raw pointer for `ThreadUnblockList`.
    #[inline]
    pub fn as_raw(&self) -> *mut bindings::Thread_UnblockList {
        self.raw.get()
    }
}

#[pinned_drop]
impl PinnedDrop for ThreadUnblockList {
    #[inline]
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `self.as_raw()` points to a valid `Thread::UnblockList` initialized in-place by
        // `Self::init`.
        unsafe {
            bindings::cpp_thread_unblock_list_destroy(self.as_raw());
        }
    }
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
    // Safety invariant: Either `None` or points to a live `Thread` that currently owns this queue.
    // Must be `None` when dropped. Accessed only through
    // `ChainLockProof<'_, OwnedWaitQueue>::{owner_thread, owner_thread_mut}`.
    owner: KCell<Option<NonNull<Thread>>, WaitQueueLockClass>,
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
    // Safety invariant: `None` when `base`'s collection is empty, or points to the
    // `inherited_scheduler_state_storage` of a `Thread` currently blocked in `base`'s collection.
    // Accessed only through
    // `ChainLockProof<'_, OwnedWaitQueue>::inherited_scheduler_state_storage{,_mut}`.
    inherited_scheduler_state_storage:
        KCell<Option<NonNull<WaitQueueInheritedSchedulerState>>, WaitQueueLockClass>,
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
// SAFETY: All interior mutations to `OwnedWaitQueue` fields (`base.collection`, `dll_node`,
// `owner`, and `inherited_scheduler_state_storage`) through shared references occur inside
// `UnsafeCell`s (`KCell` or `DoublyLinkedListNode`) and are synchronized by the embedded
// `WaitQueueBase`'s `ChainLock` (and, for `dll_node`, the owning `Thread`'s `ChainLock`).
unsafe impl Sync for OwnedWaitQueue {}

impl Deref for OwnedWaitQueue {
    type Target = WaitQueueBase;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: `OwnedWaitQueue` is `#[repr(C)]` with `base: WaitQueueBase` at offset 0.
        // Casting from `self` rather than projecting `&self.base` preserves pointer provenance
        // over the entire `OwnedWaitQueue` allocation for `downcast_to_owq`.
        unsafe { &*core::ptr::from_ref(self).cast::<WaitQueueBase>() }
    }
}

// SAFETY: `base.lock` guards `owner` and `inherited_scheduler_state_storage` (the
// `KCell<_, WaitQueueLockClass>` fields) as well as `base.collection`, and `base` is a plain
// field so the same instance is returned every time.
unsafe impl ChainLocked for OwnedWaitQueue {
    type Class = WaitQueueLockClass;

    /// Returns the lock guarding this wait queue (the embedded `WaitQueueBase`'s).
    fn lock(&self) -> &ClassedChainLock<WaitQueueLockClass> {
        self.base.lock()
    }
}

#[pinned_drop]
impl PinnedDrop for OwnedWaitQueue {
    fn drop(self: Pin<&mut Self>) {
        // Something is very very wrong if we have been allowed to destruct while we
        // still have an owner.
        // The remainder of the C++ destructor, i.e. tearing down the base wait queue and the
        // intrusive list node, is performed by the drop glue of the individual members.
        // SAFETY: We do not move `self` or any pinned fields out of `this`.
        let this = unsafe { self.get_unchecked_mut() };
        debug_assert!(this.owner.get_inner_mut().is_none());
    }
}

/// Accessors and locked operations for an [`OwnedWaitQueue`] guarded by its lock.
impl ChainLockProof<'_, OwnedWaitQueue> {
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
    ///
    /// # Safety
    ///
    /// The caller must maintain the [`OwnedWaitQueue`] safety invariants between
    /// `base.collection` and `inherited_scheduler_state_storage`.
    pub(crate) unsafe fn collection_mut(&mut self) -> &mut WaitQueueCollection {
        let (queue, token) = self.split_mut();
        // SAFETY: As in `collection`; the unique borrow of `self` ensures no other reference to
        // `base.collection` is live.
        unsafe { queue.base.collection.get_mut(token) }
    }

    /// Returns the current owner of this queue, if any.
    pub(crate) fn owner_thread(&self) -> Option<NonNull<Thread>> {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `owner`.
        *unsafe { self.owner().owner.get(self.token()) }
    }

    /// Returns the current owner of this queue mutably.
    ///
    /// # Safety
    ///
    /// The caller must maintain the [`OwnedWaitQueue`] safety invariants for `owner` and
    /// `dll_node` (any `Some(ptr)` must point to a live [`Thread`] whose `owned_wait_queues_`
    /// list contains this queue, and `owner` must be `None` when the queue is dropped).
    pub(crate) unsafe fn owner_thread_mut(&mut self) -> &mut Option<NonNull<Thread>> {
        let (queue, token) = self.split_mut();
        // SAFETY: As in `owner_thread`; the unique borrow of `self` ensures no other reference to
        // `owner` is live.
        unsafe { queue.owner.get_mut(token) }
    }

    /// Returns the location of the inherited scheduler state storage, if any.
    pub(crate) fn inherited_scheduler_state_storage(
        &self,
    ) -> Option<NonNull<WaitQueueInheritedSchedulerState>> {
        // SAFETY: See `owner_thread`.
        *unsafe { self.owner().inherited_scheduler_state_storage.get(self.token()) }
    }

    /// Returns the location of the inherited scheduler state storage mutably.
    ///
    /// # Safety
    ///
    /// The caller must maintain the [`OwnedWaitQueue`] safety invariant for
    /// `inherited_scheduler_state_storage` (`None` when `base.collection` is empty, or pointing to
    /// the `inherited_scheduler_state_storage` of a [`Thread`] currently blocked in
    /// `base.collection`).
    pub(crate) unsafe fn inherited_scheduler_state_storage_mut(
        &mut self,
    ) -> &mut Option<NonNull<WaitQueueInheritedSchedulerState>> {
        let (queue, token) = self.split_mut();
        // SAFETY: See `owner_thread_mut`.
        unsafe { queue.inherited_scheduler_state_storage.get_mut(token) }
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

    /// Updates the inherited scheduler state storage after `t` has been removed from this queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t.owner()` was just removed from `self.owner()`'s collection
    /// and that `self.owner().inherited_scheduler_state_storage` is currently `Some(...)`.
    pub(crate) unsafe fn update_sched_state_storage_thread_removed(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
    ) {
        // SAFETY: `self` and `t` prove both locks are held, and the caller guarantees `t` was
        // just removed from `self.owner()`'s collection.
        unsafe {
            bindings::cpp_owned_wait_queue_update_sched_state_storage_thread_removed(
                self.owner().as_raw(),
                t.owner().as_raw().cast(),
            );
        }
    }

    /// Gets the effective profile of this `OwnedWaitQueue`.
    ///
    /// The effective profile of an `OwnedWaitQueue` is the composition of the effective profiles
    /// of all the threads currently waiting in that queue. As such, the effective profile may
    /// change as threads are added to or removed from the queue or when the effective profile of
    /// any thread in the queue changes, either due to base profile changes or transitive
    /// propagation of upstream profile changes from owned wait queues that they own.
    pub fn get_effective_profile(&self) -> SchedulerStateEffectiveProfile {
        let mut profile = MaybeUninit::<SchedulerStateEffectiveProfile>::uninit();
        // SAFETY: `self` proves `self.owner().lock()` is held by the active transaction, and
        // `cpp_owned_wait_queue_get_effective_profile` initializes `profile`.
        unsafe {
            bindings::cpp_owned_wait_queue_get_effective_profile(
                self.owner().as_raw(),
                profile.as_mut_ptr().cast(),
            );
            profile.assume_init()
        }
    }

    /// Accessor for the queue's `owner` [`ThreadPtr`], matching C++ `OwnedWaitQueue::owner()`.
    #[inline]
    pub fn queue_owner(&self) -> Option<ThreadPtr> {
        self.owner_thread().and_then(|owner| {
            // SAFETY: By `owner`'s safety invariant, `owner` is a non-null pointer to a live
            // `Thread`.
            unsafe { ThreadPtr::from_raw(owner.as_ptr()) }
        })
    }

    /// Attempts to obtain all of the locks needed for a BAAO operation while already holding this
    /// queue's lock. On success, the current thread's lock will have been obtained in addition to
    /// any other locks in the PI chain which are needed, and the details of the locking operation
    /// will be returned along with `current_thread`'s [`OwnerGuard`]. On failure, returns `None`
    /// after dropping any locks which had been obtained (other than `self.owner().lock()`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized.
    /// - `current_thread` is the valid, currently executing [`Thread`] in `THREAD_RUNNING` state.
    /// - Neither `current_thread.lock()` nor any lock in `self.owner()`'s or `new_owner`'s
    ///   downstream PI chain is currently held.
    /// - If this call returns `Some`, the caller must finalize the transaction and subsequently
    ///   release the acquired locks via [`OwnedWaitQueue::block_and_assign_owner_locked`] or
    ///   [`OwnedWaitQueue::cancel_baao_operation_locked`] without intervening mutations to the
    ///   queue or locks.
    pub unsafe fn try_lock_for_baao_operation_locked<'a>(
        &mut self,
        current_thread: &'a Thread,
        new_owner: Option<&Thread>,
        clt_token: &'a ChainLockTransactionToken,
    ) -> Option<(BAAOLockingDetails, OwnerGuard<'a, Thread>)> {
        assert_transaction_active(clt_token);
        assert_lock_not_held(current_thread.lock());
        let mut details = BAAOLockingDetails::default();
        let new_owner_ptr = new_owner.map_or(core::ptr::null_mut(), Thread::as_raw);
        // SAFETY: `self.owner().as_raw()`, `current_thread`, and `details` are valid, `self`
        // proves `self.owner().lock()` is held in the active transaction, and the C++ side
        // synchronizes internal mutations via `Thread`'s lock and will behave correctly despite
        // the mutable pointer casts.
        let ok = unsafe {
            bindings::cpp_owned_wait_queue_try_lock_for_baao_operation_locked(
                self.owner().as_raw(),
                current_thread.as_raw().cast(),
                new_owner_ptr.cast(),
                (&raw mut details).cast(),
            )
        };
        if ok {
            // SAFETY: `cpp_owned_wait_queue_try_lock_for_baao_operation_locked` returned `true`,
            // which guarantees `current_thread.lock()` was acquired in the active transaction.
            Some((details, unsafe { OwnerGuard::adopt(current_thread, clt_token) }))
        } else {
            None
        }
    }

    /// Wakes the locked `threads` from this wait queue while already holding the queue lock.
    ///
    /// Note: The underlying C++ `WakeThreadsLocked` releases the PI-chain locks and the locks of
    /// the threads in `threads` as they are unblocked, while keeping `self.owner().lock()` held.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `threads` was populated by a preceding successful call to
    ///   [`OwnedWaitQueue::lock_for_wake_operation_or_backoff`] or
    ///   [`Self::lock_for_wake_operation_locked`] on `self.owner()` in the current transaction,
    ///   with no intervening modifications to `self.owner()`, `threads`, or the acquired locks.
    /// - No [`ChainLockProof<'_, Thread>`] or [`OwnerGuard<'_, Thread>`] for any thread in
    ///   `threads` is live across this call.
    /// - The active transaction has been finalized via
    ///   [`ChainLockTransaction::finalize`](crate::kconcurrent::ChainLockTransaction::finalize).
    /// - If `option == WakeOption::AssignOwner`, `threads` contains at most one thread (i.e.,
    ///   `max_wake <= 1` was passed to the preceding lock operation).
    pub unsafe fn wake_threads_locked(
        &mut self,
        threads: Pin<&mut ThreadUnblockList>,
        wake_hooks: &mut dyn IWakeRequeueHook,
        option: WakeOption,
        force_inheritance: ForceInheritance,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> WakeThreadsResult {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let mut wake_proxy = WakeRequeueHookProxyContext { hook: wake_hooks };
        let mut result = WakeThreadsResult::default();
        // SAFETY: `self.owner().as_raw()`, `threads.as_raw()`, `wake_proxy`, and `result` are
        // valid, `self` proves `self.owner().lock()` is held, and the caller upholds the
        // wake-threads-locked preconditions.
        unsafe {
            bindings::cpp_owned_wait_queue_wake_threads_locked(
                self.owner().as_raw(),
                threads.as_raw(),
                core::ptr::from_mut(&mut wake_proxy).cast::<c_void>(),
                option.into(),
                force_inheritance.into(),
                (&raw mut result).cast(),
            );
        }
        result
    }

    /// Attempts to obtain all of the thread/PI locks needed for a `wake_threads` operation while
    /// already holding this queue's lock.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized, and no locks in `self.owner()`'s
    ///   downstream PI chain or waiter threads are already held.
    /// - `unblock_list` is empty.
    /// - If this call returns `true`, the caller must finalize the transaction and process
    ///   `unblock_list` via [`Self::wake_threads_locked`] on `self.owner()` before ending the
    ///   transaction (and must not drop `unblock_list` while non-empty).
    #[must_use]
    pub unsafe fn lock_for_wake_operation_locked(
        &mut self,
        max_wake: u32,
        wake_hooks: &mut dyn IWakeRequeueHook,
        unblock_list: Pin<&mut ThreadUnblockList>,
        clt_token: &ChainLockTransactionToken,
    ) -> bool {
        assert_transaction_active(clt_token);
        let mut wake_proxy = WakeRequeueHookProxyContext { hook: wake_hooks };
        // SAFETY: `self.owner().as_raw()`, `wake_proxy`, and `unblock_list.as_raw()` are valid,
        // `self` proves `self.owner().lock()` is held in the active transaction, and the caller
        // upholds the preconditions above.
        unsafe {
            bindings::cpp_owned_wait_queue_lock_for_wake_operation_locked(
                self.owner().as_raw(),
                max_wake,
                core::ptr::from_mut(&mut wake_proxy).cast::<c_void>(),
                unblock_list.as_raw(),
            )
        }
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
    #[inline]
    pub fn as_raw(&self) -> *mut bindings::OwnedWaitQueue {
        (self as *const Self).cast_mut().cast()
    }

    /// Returns a raw mutable pointer to the underlying C++ `OwnedWaitQueue`.
    #[inline]
    pub fn as_ptr(&self) -> *mut c_void {
        self.as_raw().cast()
    }

    /// Returns a default no-op [`DefaultWakeHooks`] instance.
    #[inline]
    pub const fn default_wake_hooks() -> DefaultWakeHooks {
        DefaultWakeHooks
    }

    /// Releases ownership of all wait queues currently owned by `t` and updates bookkeeping as
    /// appropriate. This is meant to be called from the thread itself and therefore it is assumed
    /// that the thread in question is not blocked on any other wait queues.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t` is an exiting [`Thread`] in `THREAD_RUNNING` state with
    /// `can_own_wait_queues_` set to `false` and not blocked in any wait queue
    /// (`blocking_wait_queue_ == nullptr`).
    pub unsafe fn disown_all_queues(t: &Thread) {
        assert_no_transaction_active();
        // SAFETY: `t` is valid, no transaction is active, the caller upholds the exiting-thread
        // preconditions, and the C++ side synchronizes internal mutations via `Thread`'s lock and
        // will behave correctly despite the mutable pointer cast.
        unsafe {
            bindings::cpp_owned_wait_queue_disown_all_queues(t.as_raw().cast());
        }
    }

    /// Changes a thread's base profile and deals with profile propagation side effects.
    pub fn set_thread_base_profile_and_propagate(
        thread: &Thread,
        profile: &SchedulerStateBaseProfile,
    ) {
        assert_no_transaction_active();
        // SAFETY: `thread` and `profile` are valid, no transaction is active, and the C++ side
        // synchronizes internal mutations via `Thread`'s lock and will behave correctly despite
        // the mutable pointer cast.
        unsafe {
            bindings::cpp_owned_wait_queue_set_thread_base_profile_and_propagate(
                thread.as_raw().cast(),
                profile.get().cast(),
            );
        }
    }

    /// Attempts to lock the rest of a PI chain, starting from the (already locked) [`Thread`]
    /// node given by `start`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized, and no downstream locks in
    ///   `start.owner()`'s PI chain are already held.
    /// - If this call returns [`ChainLockResult::Ok`], any downstream locks acquired in
    ///   `start.owner()`'s PI chain are properly unlocked before ending the transaction.
    pub unsafe fn lock_pi_chain_from_thread(
        start: &mut ChainLockProof<'_, Thread>,
        clt_token: &ChainLockTransactionToken,
    ) -> ChainLockResult {
        assert_transaction_active(clt_token);
        // SAFETY: `start.owner()` is valid, both the transaction and `start.owner().lock()` are
        // held, the caller upholds the PI-chain locking preconditions, and the C++ side
        // synchronizes internal mutations via `Thread`'s lock and will behave correctly despite
        // the mutable pointer cast.
        let res = unsafe {
            bindings::cpp_owned_wait_queue_lock_pi_chain_thread(start.owner().as_raw().cast())
        };
        chain_lock_result_from_bindings(res)
    }

    /// Attempts to lock the rest of a PI chain, starting from the (already locked)
    /// [`WaitQueueBase`] node given by `start`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized, and no downstream locks in
    ///   `start.owner()`'s PI chain are already held.
    /// - If `start.owner().magic() == OWNED_MAGIC`, `start.owner()` is the `base` subobject of a
    ///   live [`OwnedWaitQueue`] and carries provenance over the enclosing [`OwnedWaitQueue`].
    /// - If this call returns [`ChainLockResult::Ok`], any downstream locks acquired in
    ///   `start.owner()`'s PI chain are properly unlocked before ending the transaction.
    pub unsafe fn lock_pi_chain_from_wait_queue(
        start: &mut ChainLockProof<'_, WaitQueueBase>,
        clt_token: &ChainLockTransactionToken,
    ) -> ChainLockResult {
        assert_transaction_active(clt_token);
        // SAFETY: `start.owner().as_raw()` is valid, both the transaction and
        // `start.owner().lock()` are held, and the caller upholds the PI-chain and subobject
        // provenance preconditions.
        let res = unsafe {
            bindings::cpp_owned_wait_queue_lock_pi_chain_queue(start.owner().as_raw().cast())
        };
        chain_lock_result_from_bindings(res)
    }

    /// Attempts to lock the rest of a PI chain, starting from the (already locked)
    /// [`OwnedWaitQueue`] node given by `start`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized, and no downstream locks in
    ///   `start.owner()`'s PI chain are already held.
    /// - If this call returns [`ChainLockResult::Ok`], any downstream locks acquired in
    ///   `start.owner()`'s PI chain are properly unlocked before ending the transaction.
    pub unsafe fn lock_pi_chain(
        start: &mut ChainLockProof<'_, Self>,
        clt_token: &ChainLockTransactionToken,
    ) -> ChainLockResult {
        assert_transaction_active(clt_token);
        // SAFETY: `start.owner().as_raw()` is valid with provenance over the full
        // `OwnedWaitQueue` (`base` is at offset 0), both the transaction and
        // `start.owner().lock()` are held, and the caller upholds the PI-chain preconditions.
        let res = unsafe {
            bindings::cpp_owned_wait_queue_lock_pi_chain_queue(start.owner().as_raw().cast())
        };
        chain_lock_result_from_bindings(res)
    }

    /// Downcast from a [`WaitQueueBase`] to an [`OwnedWaitQueue`] if possible, using the magic
    /// number to detect the underlying nature of the object. Returns `None` if the object is not
    /// an [`OwnedWaitQueue`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that if `wq.magic() == OWNED_MAGIC`, `wq` is the `base` subobject
    /// of a live [`OwnedWaitQueue`] and carries provenance over the enclosing [`OwnedWaitQueue`].
    #[inline]
    pub(crate) unsafe fn downcast_to_owq(wq: &WaitQueueBase) -> Option<&Self> {
        if wq.magic() == OWNED_MAGIC {
            // SAFETY: `base` is at offset 0 of `OwnedWaitQueue`, and the caller guarantees that
            // when `wq.magic() == OWNED_MAGIC`, `wq` is the `base` subobject of a live
            // `OwnedWaitQueue` with provenance over the enclosing `OwnedWaitQueue`.
            Some(unsafe { &*(core::ptr::from_ref(wq).cast::<Self>()) })
        } else {
            None
        }
    }

    /// Propagates the removal of a single edge between `upstream_node` and `downstream_node`
    /// along the PI chain.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `upstream_node.owner()` was just removed from `downstream_node.owner()`.
    /// - All `ChainLock`s downstream of `downstream_node.owner()` in its PI graph are held by the
    ///   active transaction.
    pub(crate) unsafe fn begin_propagate_remove_single_edge(
        upstream_node: &mut ChainLockProof<'_, Thread>,
        downstream_node: &mut ChainLockProof<'_, Self>,
        force_inheritance: ForceInheritance,
        old_target_ep: Option<&SchedulerStateEffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let old_ep_ptr = old_target_ep.map_or(core::ptr::null(), |ep| {
            core::ptr::from_ref(ep).cast::<bindings::SchedulerState_EffectiveProfile>()
        });
        // SAFETY: `upstream_node.owner()` and `downstream_node.owner()` are valid, both locks are
        // held via their proofs, the transaction and preemption-disabled tokens are verified, and
        // the caller upholds the PI-chain preconditions.
        unsafe {
            bindings::cpp_owned_wait_queue_begin_propagate_remove_single_edge(
                upstream_node.owner().as_raw().cast(),
                downstream_node.owner().as_raw(),
                force_inheritance.into(),
                old_ep_ptr,
            );
        }
    }

    /// Releases all PI chain locks starting from `start` (`include_start = true`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `start`'s lock and all `ChainLock`s downstream of `start` in its PI graph are held by
    ///   the active transaction and are not unlocked again by the caller after this call releases
    ///   them.
    /// - If `start.magic() == OWNED_MAGIC`, `start` is the `base` subobject of a live
    ///   [`OwnedWaitQueue`] with provenance over the full [`OwnedWaitQueue`].
    pub(crate) unsafe fn unlock_pi_chain_common_from_wait_queue(
        start: &WaitQueueBase,
        clt_token: &ChainLockTransactionToken,
    ) {
        assert_transaction_active(clt_token);
        // SAFETY: `start.as_raw()` is valid, the active transaction is verified, and the caller
        // upholds the PI-chain and `OwnedWaitQueue` provenance preconditions.
        unsafe {
            bindings::cpp_owned_wait_queue_unlock_pi_chain_common_wait_queue(
                start.as_raw().cast(),
                core::ptr::null(),
            );
        }
    }

    /// Debug assert wrapper which skips the thread analysis checks just to assert that a specific
    /// queue is unowned. Used by `FutexContext`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that no concurrent mutations to `self.owner` can occur for the
    /// duration of this call (for example, `self.lock()` is held or the queue is quiescent in a
    /// pool with no active operations).
    #[inline]
    pub unsafe fn assert_not_owned(&self) {
        // SAFETY: `self.as_raw()` is a valid pointer to this initialized `OwnedWaitQueue`, and the
        // caller guarantees no concurrent mutation to `self.owner`.
        unsafe {
            bindings::cpp_owned_wait_queue_assert_not_owned(self.as_raw());
        }
    }

    /// Assigns ownership of this wait queue to `new_owner`, or explicitly releases ownership if
    /// `new_owner` is `None`. No change is made to ownership if it would result in a cycle in the
    /// inheritance graph.
    ///
    /// Note, if the new owner exists, but is dead or dying, it will not be permitted to become the
    /// new owner of the wait queue. Any existing owner will be replaced with no owner in this
    /// situation.
    ///
    /// Returns `Err(Status::BAD_STATE)` if a cycle would have been produced, and `Ok(())`
    /// otherwise.
    pub fn assign_owner(&self, new_owner: Option<&Thread>) -> Result<(), Status> {
        assert_no_transaction_active();
        let new_owner_ptr = new_owner.map_or(core::ptr::null_mut(), Thread::as_raw);
        // SAFETY: `self.as_raw()` is valid, no transaction is active, and the C++ side
        // synchronizes internal mutations via `Thread`'s lock and will behave correctly despite
        // the mutable pointer cast.
        let status = unsafe {
            bindings::cpp_owned_wait_queue_assign_owner(self.as_raw(), new_owner_ptr.cast())
        };
        Status::ok(status)
    }

    /// Assigns ownership of this wait queue to an already-locked `new_owner`.
    ///
    /// Returns `Err(Status::SHOULD_WAIT)` if a back-off is required, and `Ok(())` otherwise.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized.
    /// - `new_owner.owner()` has been started (`state != Initial` / `can_own_wait_queues_` is
    ///   valid).
    /// - `new_owner.owner()`'s downstream PI chain is either empty (not blocked in any wait queue)
    ///   or already locked by the active
    ///   [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) without forming a
    ///   cycle with `self`.
    pub unsafe fn assign_owner_locked(
        &self,
        new_owner: &mut ChainLockProof<'_, Thread>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        assert_lock_not_held(self.lock());
        preempt_disabled_token.assert_held();
        // SAFETY: `self.as_raw()` and `new_owner.owner()` are valid, `self.lock()` is not held,
        // `new_owner` proves `new_owner.owner().lock()` is held, the transaction and
        // preemption-disabled tokens are verified, the caller upholds the preconditions above,
        // and the C++ side synchronizes internal mutations via `Thread`'s lock and will behave
        // correctly despite the mutable pointer cast.
        let status = unsafe {
            bindings::cpp_owned_wait_queue_assign_owner_locked(
                self.as_raw(),
                new_owner.owner().as_raw().cast(),
            )
        };
        Status::ok(status)
    }

    /// Resets the queue owner if there are no waiters currently queued.
    ///
    /// # Panics
    ///
    /// Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is already
    /// active on the current CPU.
    pub fn reset_owner_if_no_waiters(&self) {
        assert_no_transaction_active();
        // SAFETY:
        // - `self.as_raw()` returns a valid pointer to this initialized `OwnedWaitQueue`.
        // - All fields of `OwnedWaitQueue` and its embedded `WaitQueueBase` that may be mutated
        //   under the queue's `ChainLock` (`base.collection`, `owner`, and
        //   `inherited_scheduler_state_storage`) are wrapped in `KCell` (an `UnsafeCell`), and
        //   `dll_node` has its own `UnsafeCell`s.
        // - The assertion above ensures no `ChainLockTransaction` is active on the current CPU
        //   (`TA_EXCL(chainlock_transaction_token, get_lock())`).
        unsafe {
            bindings::cpp_owned_wait_queue_reset_owner_if_no_waiters(self.as_raw());
        }
    }

    /// Checks invariants before returning this wait queue to the pool.
    ///
    /// # Safety
    ///
    /// The caller must ensure that no concurrent mutations to `self` can occur for the duration of
    /// this call (for example, `self.lock()` is held or the queue is quiescent in a pool with no
    /// active operations).
    #[inline]
    pub unsafe fn assert_safe_for_pooling(&self) {
        // SAFETY: `self.as_raw()` is a valid pointer to this initialized `OwnedWaitQueue`, and the
        // caller guarantees no concurrent mutation to `self`.
        unsafe {
            bindings::cpp_owned_wait_queue_assert_safe_for_pooling(self.as_raw());
        }
    }

    /// Blocks the current thread on this wait queue, and re-assigns ownership to the specified
    /// thread (or removes ownership if `new_owner` is `None`). If a cycle would have been produced
    /// by this operation, no changes are made and `Err(Status::BAD_STATE)` will be returned.
    ///
    /// Note, if the new owner exists, but is dead or dying, it will not be permitted to become the
    /// new owner of the wait queue. Any existing owner will be replaced with no owner in this
    /// situation.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The current thread is in a valid state to block (`THREAD_RUNNING` and not holding any
    ///   spinlocks).
    /// - `self` (and `new_owner`, if `Some`) remains alive and valid at its pinned address for the
    ///   entire duration that the current thread remains blocked in `self`.
    pub unsafe fn block_and_assign_owner(
        &self,
        deadline: &Deadline,
        new_owner: Option<&Thread>,
        resource_ownership: ResourceOwnership,
        interruptible: Interruptible,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Result<(), Status> {
        assert_no_transaction_active();
        preempt_disabled_token.assert_held();
        let new_owner_ptr = new_owner.map_or(core::ptr::null_mut(), Thread::as_raw);
        // SAFETY: `self.as_raw()` and `deadline` are valid, no transaction is active, preemption
        // is disabled, the caller guarantees blocking is permitted and `self` outlives the block,
        // and the C++ side synchronizes internal mutations via `Thread`'s lock and will behave
        // correctly despite the mutable pointer cast.
        let status = unsafe {
            bindings::cpp_owned_wait_queue_block_and_assign_owner(
                self.as_raw(),
                core::ptr::from_ref(deadline).cast(),
                new_owner_ptr.cast(),
                resource_ownership.into(),
                interruptible.into(),
            )
        };
        Status::ok(status)
    }

    /// Blocks `current_thread` on this wait queue and assigns ownership according to `details`
    /// after locks have already been acquired via [`Self::lock_for_baao_operation_or_backoff`] or
    /// [`ChainLockProof::try_lock_for_baao_operation_locked`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread.owner()` is the valid, currently executing [`Thread`] in a valid state to
    ///   block (not holding any spinlocks).
    /// - `guard.owner()` remains alive and valid at its pinned address for the entire duration
    ///   that `current_thread.owner()` remains blocked in `guard.owner()`.
    /// - `details` was populated by a preceding successful BAAO lock acquisition on
    ///   `guard.owner()` for `current_thread.owner()` in the current transaction with no
    ///   intervening mutations, and the active transaction is finalized with
    ///   `guard.owner().lock()`, `current_thread.owner().lock()`, and the PI-chain locks acquired
    ///   by that operation as the only held chain locks (`guard.owner().lock()` and the PI-chain
    ///   locks are released by this call, consuming `guard`, and must not be unlocked again by the
    ///   caller).
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn block_and_assign_owner_locked(
        guard: OwnerGuard<'_, Self>,
        current_thread: &mut ChainLockProof<'_, Thread>,
        deadline: &Deadline,
        details: &BAAOLockingDetails,
        resource_ownership: ResourceOwnership,
        interruptible: Interruptible,
        force_inheritance: ForceInheritance,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        let wait_queue = guard.into_unguarded();
        // SAFETY: `wait_queue.as_raw()`, `current_thread.owner()`, `deadline`, and `details` are
        // valid, the caller upholds the BAAO locking preconditions, and the C++ side synchronizes
        // internal mutations via `Thread`'s lock and will behave correctly despite the mutable
        // pointer cast.
        let status = unsafe {
            bindings::cpp_owned_wait_queue_block_and_assign_owner_locked(
                wait_queue.as_raw(),
                current_thread.owner().as_raw().cast(),
                core::ptr::from_ref(deadline).cast(),
                core::ptr::from_ref(details).cast(),
                resource_ownership.into(),
                interruptible.into(),
                force_inheritance.into(),
            )
        };
        Status::ok(status)
    }

    /// Cancels a block-and-assign-owner operation that we have already locked for.
    /// Basically, just drops all of the locks and gets out.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - `current_thread_guard.owner()` is the valid, currently executing [`Thread`].
    /// - `details` was populated by a preceding successful BAAO lock acquisition on
    ///   `guard.owner()` for `current_thread_guard.owner()` in the current transaction with no
    ///   intervening mutations, the active transaction has been finalized via
    ///   [`ChainLockTransaction::finalize`](crate::kconcurrent::ChainLockTransaction::finalize),
    ///   and all PI-chain locks acquired by that operation are still held (`guard.owner().lock()`,
    ///   `current_thread_guard.owner().lock()`, and the PI-chain locks are released by this call,
    ///   consuming `guard` and `current_thread_guard`).
    pub unsafe fn cancel_baao_operation_locked(
        guard: OwnerGuard<'_, Self>,
        current_thread_guard: OwnerGuard<'_, Thread>,
        details: &BAAOLockingDetails,
        clt_token: &ChainLockTransactionToken,
    ) {
        assert_transaction_active(clt_token);
        let wait_queue = guard.into_unguarded();
        let current_thread = current_thread_guard.into_unguarded();
        // SAFETY: `wait_queue.as_raw()`, `current_thread`, and `details` are valid, the caller
        // upholds the BAAO locking preconditions, and the C++ side synchronizes internal mutations
        // via `Thread`'s lock and will behave correctly despite the mutable pointer cast.
        unsafe {
            bindings::cpp_owned_wait_queue_cancel_baao_operation_locked(
                wait_queue.as_raw(),
                current_thread.as_raw().cast(),
                core::ptr::from_ref(details).cast(),
            );
        }
    }

    /// Obtains all of the locks needed for a `block_and_assign_owner` operation, handling the
    /// special case of new+old owner which share a PI graph target.
    ///
    /// On success, returns `Some((details, self_guard, current_thread_guard))`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized.
    /// - `current_thread` is the valid, currently executing [`Thread`] in `THREAD_RUNNING` state.
    /// - Neither `self.lock()`, `current_thread.lock()`, nor any lock in `self`'s or `new_owner`'s
    ///   downstream PI chain is currently held.
    /// - If this call returns `Some`, the caller must finalize the transaction and subsequently
    ///   release the acquired locks via [`Self::block_and_assign_owner_locked`] or
    ///   [`Self::cancel_baao_operation_locked`] without intervening mutations to the queue or
    ///   locks.
    pub unsafe fn lock_for_baao_operation_or_backoff<'a>(
        &'a self,
        current_thread: &'a Thread,
        new_owner: Option<&Thread>,
        clt_token: &'a ChainLockTransactionToken,
    ) -> Option<(BAAOLockingDetails, OwnerGuard<'a, Self>, OwnerGuard<'a, Thread>)> {
        assert_transaction_active(clt_token);
        assert_lock_not_held(self.lock());
        assert_lock_not_held(current_thread.lock());
        let mut details = BAAOLockingDetails::default();
        let new_owner_ptr = new_owner.map_or(core::ptr::null_mut(), Thread::as_raw);
        // SAFETY: `self.as_raw()`, `current_thread`, and `details` are valid, the transaction is
        // active, and the C++ side synchronizes internal mutations via `Thread`'s lock and will
        // behave correctly despite the mutable pointer casts.
        let ok = unsafe {
            bindings::cpp_owned_wait_queue_lock_for_baao_operation_or_backoff(
                self.as_raw(),
                current_thread.as_raw().cast(),
                new_owner_ptr.cast(),
                (&raw mut details).cast(),
            )
        };
        if ok {
            // SAFETY: `cpp_owned_wait_queue_lock_for_baao_operation_or_backoff` returned `true`,
            // which guarantees `self.lock()` and `current_thread.lock()` were acquired in the
            // active transaction.
            Some((details, unsafe { OwnerGuard::adopt(self, clt_token) }, unsafe {
                OwnerGuard::adopt(current_thread, clt_token)
            }))
        } else {
            None
        }
    }

    /// Wakes up to the specified number of threads from the wait queue, removing any current queue
    /// owner in the process.
    #[inline]
    pub fn wake_threads(
        &self,
        wake_count: u32,
        wake_hooks: &mut dyn IWakeRequeueHook,
    ) -> WakeThreadsResult {
        assert_no_transaction_active();
        let mut wake_proxy = WakeRequeueHookProxyContext { hook: wake_hooks };
        let mut result = WakeThreadsResult::default();
        // SAFETY: `self.as_raw()`, `wake_proxy`, and `result` are valid and no chainlock
        // transaction is active.
        unsafe {
            bindings::cpp_owned_wait_queue_wake_threads(
                self.as_raw(),
                wake_count,
                core::ptr::from_mut(&mut wake_proxy).cast::<c_void>(),
                (&raw mut result).cast(),
            );
        }
        result
    }

    /// Attempts to wake exactly one thread, and assigns ownership of the queue to the woken thread
    /// (if any).
    #[inline]
    pub fn wake_thread_and_assign_owner(
        &self,
        wake_hooks: &mut dyn IWakeRequeueHook,
    ) -> WakeThreadsResult {
        assert_no_transaction_active();
        let mut wake_proxy = WakeRequeueHookProxyContext { hook: wake_hooks };
        let mut result = WakeThreadsResult::default();
        // SAFETY: `self.as_raw()`, `wake_proxy`, and `result` are valid and no chainlock
        // transaction is active.
        unsafe {
            bindings::cpp_owned_wait_queue_wake_thread_and_assign_owner(
                self.as_raw(),
                core::ptr::from_mut(&mut wake_proxy).cast::<c_void>(),
                (&raw mut result).cast(),
            );
        }
        result
    }

    /// Obtains all of the locks needed for a `wake_threads` operation.
    ///
    /// On success, populates `unblock_list` and returns `Some(self_guard)`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized, and neither `self.lock()` nor any lock
    ///   in `self`'s downstream PI chain or waiter threads is currently held.
    /// - `unblock_list` is empty.
    /// - If this call returns `Some`, the caller must finalize the transaction and process
    ///   `unblock_list` via [`ChainLockProof::wake_threads_locked`] on `self` and release the
    ///   returned [`OwnerGuard`] before ending the transaction (and must not drop `unblock_list`
    ///   while non-empty).
    pub unsafe fn lock_for_wake_operation_or_backoff<'a>(
        &'a self,
        max_wake: u32,
        wake_hooks: &mut dyn IWakeRequeueHook,
        unblock_list: Pin<&mut ThreadUnblockList>,
        clt_token: &'a ChainLockTransactionToken,
    ) -> Option<OwnerGuard<'a, Self>> {
        assert_transaction_active(clt_token);
        assert_lock_not_held(self.lock());
        let mut wake_proxy = WakeRequeueHookProxyContext { hook: wake_hooks };
        // SAFETY: `self.as_raw()`, `wake_proxy`, and `unblock_list.as_raw()` are valid,
        // `self.lock()` is not held, the transaction is active, and the caller upholds the
        // preconditions above.
        let ok = unsafe {
            bindings::cpp_owned_wait_queue_lock_for_wake_operation_or_backoff(
                self.as_raw(),
                max_wake,
                core::ptr::from_mut(&mut wake_proxy).cast::<c_void>(),
                unblock_list.as_raw(),
            )
        };
        if ok {
            // SAFETY: `cpp_owned_wait_queue_lock_for_wake_operation_or_backoff` returned `true`,
            // which guarantees `self.lock()` was acquired in the active transaction.
            Some(unsafe { OwnerGuard::adopt(self, clt_token) })
        } else {
            None
        }
    }

    /// Wakes up to `wake_count` threads from this queue and moves up to `requeue_count` threads to
    /// `requeue_target`, updating ownership bookkeeping according to `wake_option` and
    /// `new_requeue_owner`.
    ///
    /// Note, if `new_requeue_owner` exists, but is dead or dying, it will not be permitted to
    /// become the new owner of `requeue_target`. Any existing owner will be replaced with no owner
    /// in this situation.
    #[allow(clippy::too_many_arguments)]
    pub fn wake_and_requeue(
        &self,
        requeue_target: &OwnedWaitQueue,
        new_requeue_owner: Option<&Thread>,
        wake_count: u32,
        requeue_count: u32,
        wake_hooks: &mut dyn IWakeRequeueHook,
        requeue_hooks: &mut dyn IWakeRequeueHook,
        wake_option: WakeOption,
    ) -> WakeThreadsResult {
        assert_no_transaction_active();
        assert!(
            !core::ptr::eq(self, requeue_target),
            "wake_and_requeue: self and requeue_target must be distinct queues"
        );
        assert!(
            wake_option != WakeOption::AssignOwner || wake_count <= 1,
            "wake_and_requeue: cannot specify WakeOption::AssignOwner when wake_count > 1"
        );
        let mut wake_proxy = WakeRequeueHookProxyContext { hook: wake_hooks };
        let mut requeue_proxy = WakeRequeueHookProxyContext { hook: requeue_hooks };
        let mut result = WakeThreadsResult::default();
        let owner_ptr = new_requeue_owner.map_or(core::ptr::null_mut(), Thread::as_raw);
        // SAFETY: `self.as_raw()`, `requeue_target.as_raw()`, `wake_proxy`, `requeue_proxy`, and
        // `result` are valid, `self` and `requeue_target` are distinct queues, no transaction is
        // active, and the C++ side synchronizes internal mutations via `Thread`'s lock and will
        // behave correctly despite the mutable pointer cast.
        unsafe {
            bindings::cpp_owned_wait_queue_wake_and_requeue(
                self.as_raw(),
                requeue_target.as_raw(),
                owner_ptr.cast(),
                wake_count,
                requeue_count,
                core::ptr::from_mut(&mut wake_proxy).cast::<c_void>(),
                core::ptr::from_mut(&mut requeue_proxy).cast::<c_void>(),
                wake_option.into(),
                (&raw mut result).cast(),
            );
        }
        result
    }
}

/// Unit tests for OwnedWaitQueue Rust bindings.
#[cfg(ktest)]
#[unittest::suite(name = "owned_wait_queue_rust")]
mod tests {
    use super::{
        DefaultWakeHooks, ForceInheritance, IWakeRequeueHook, Interruptible, OWNED_MAGIC,
        OwnedWaitQueue, PreemptDisabledToken, ResourceOwnership, Thread, ThreadPtr,
        ThreadUnblockList, WakeOption, WakeThreadsResult,
    };
    use crate::kconcurrent::{
        Action, ChainLockProof, ChainLockResult, ChainLockTransaction, DONE, IRQ_SAVE_OPTION,
        OwnerGuard, PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, TransactionResult, clt_tag,
        single_chain_lock_guard_owned,
    };
    use crate::kernel::deadline::Deadline;
    use crate::kernel::thread::{self, AnnotatedAutoEagerReschedDisabler};
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::c_void;
    use core::sync::atomic::{AtomicBool, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_err, assert_true, expect_eq, expect_true, unwrap_ok};
    use zx_status::Status;

    #[derive(Default)]
    struct CountHook {
        deny: bool,
        allow_count: u32,
        wake_count: u32,
        last_thread: Option<ThreadPtr>,
    }

    impl IWakeRequeueHook for CountHook {
        fn allow(&mut self, _thread: &Thread) -> bool {
            self.allow_count += 1;
            !self.deny
        }

        fn on_wake_or_requeue(&mut self, thread: &mut ChainLockProof<'_, Thread>) {
            self.wake_count += 1;
            self.last_thread = Some(ThreadPtr::from(thread.owner()));
        }
    }

    const EMPTY_RESULT: WakeThreadsResult =
        WakeThreadsResult { woken: 0, still_waiting: 0, owner: None };

    /// Returns the owner of `owq` as observed while holding its lock.
    fn observe_owner(owq: &OwnedWaitQueue) -> Option<ThreadPtr> {
        single_chain_lock_guard_owned!(
            scope,
            owq,
            IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_observe_owner")
        );
        scope.proof().queue_owner()
    }

    /// Blocks the current thread in `owq` using `lock_for_baao_operation_or_backoff` and
    /// `block_and_assign_owner_locked`, proposing `new_owner` as the queue's owner.
    fn block_current_thread(
        owq: &OwnedWaitQueue,
        new_owner: Option<&Thread>,
        deadline: Deadline,
    ) -> Result<(), Status> {
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current().as_ref() };
        let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        // SAFETY: Preemption is disabled by `_eager_resched_disabler`.
        let preempt_token = unsafe { PreemptDisabledToken::new() };
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_block"),
            |token| -> TransactionResult<Result<(), Status>> {
                // SAFETY: The transaction holds no locks, `current` is the current thread, and
                // `new_owner` (if any) is kept alive by the test.
                let Some((details, owq_guard, mut current_guard)) =
                    (unsafe { owq.lock_for_baao_operation_or_backoff(current, new_owner, token) })
                else {
                    return Action::Backoff.into();
                };
                ChainLockTransaction::finalize(token);
                let mut current_proof = current_guard.proof();
                // SAFETY: The transaction is finalized, preemption is disabled by
                // `_eager_resched_disabler`, `details` holds the BAAO locks for `owq` and
                // `current`, and `owq` outlives this call.
                let result = unsafe {
                    OwnedWaitQueue::block_and_assign_owner_locked(
                        owq_guard,
                        &mut current_proof,
                        &deadline,
                        &details,
                        ResourceOwnership::Normal,
                        Interruptible::No,
                        ForceInheritance::No,
                        token,
                        &preempt_token,
                    )
                };
                // `block_and_assign_owner_locked` returns holding only `current`'s lock.
                current_guard.release();
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
        // SAFETY: `new_owner` (if `Some`) is kept alive by the test while the waiter runs.
        let new_owner = unsafe { ctx.new_owner.map(|t| t.as_ref()) };
        match block_current_thread(ctx.owq, new_owner, Deadline::infinite()) {
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

    /// Verifies OwnedWaitQueue initialization and empty wake/requeue operations.
    #[test]
    fn init_and_empty_operations() {
        stack_pin_init!(let owq1 = OwnedWaitQueue::init());
        stack_pin_init!(let owq2 = OwnedWaitQueue::init());

        // SAFETY: `owq1` and `owq2` are newly initialized local queues with no concurrent access.
        unsafe {
            owq1.assert_not_owned();
            owq2.assert_not_owned();
        }
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
        let owq_ref: &OwnedWaitQueue = &owq;

        assert_err!(
            block_current_thread(owq_ref, None, Deadline::infinite_past()),
            Status::TIMED_OUT
        );

        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current().as_ref() };
        let deadline = Deadline::infinite_past();
        let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        // SAFETY: Preemption is disabled by `_eager_resched_disabler`.
        let preempt_token = unsafe { PreemptDisabledToken::new() };
        let result = ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_try_lock"),
            |token| -> TransactionResult<Result<(), Status>> {
                let mut owq_guard = OwnerGuard::new(owq_ref, token);
                // SAFETY: `current`'s lock is not held and `current` is the current thread.
                let Some((details, mut current_guard)) = (unsafe {
                    owq_guard.proof().try_lock_for_baao_operation_locked(current, None, token)
                }) else {
                    owq_guard.release();
                    return Action::Backoff.into();
                };
                ChainLockTransaction::finalize(token);
                let mut current_proof = current_guard.proof();
                // SAFETY: The transaction is finalized, preemption is disabled by
                // `_eager_resched_disabler`, `details` holds the BAAO locks for `owq` and
                // `current`, and `owq` outlives this call.
                let result = unsafe {
                    OwnedWaitQueue::block_and_assign_owner_locked(
                        owq_guard,
                        &mut current_proof,
                        &deadline,
                        &details,
                        ResourceOwnership::Normal,
                        Interruptible::No,
                        ForceInheritance::No,
                        token,
                        &preempt_token,
                    )
                };
                // `block_and_assign_owner_locked` returns holding only `current`'s lock.
                current_guard.release();
                TransactionResult::Value(result)
            },
        );
        assert_err!(result, Status::TIMED_OUT);

        assert_true!(observe_owner(owq_ref).is_none());
        let mut hook = CountHook::default();
        let res = owq_ref.wake_threads(1, &mut hook);
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
        // SAFETY: `current` is the currently executing thread.
        let current_ref = unsafe { current.as_ref() };
        let res = owq1.wake_and_requeue(
            &owq2,
            Some(current_ref),
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

    /// Verifies `cancel_baao_operation_locked` and waking waiters via locked wake operations.
    #[test]
    fn locked_wake_and_cancel_baao() {
        stack_pin_init!(let owq = OwnedWaitQueue::init());
        let owq_ref: &OwnedWaitQueue = &owq;
        // SAFETY: Called from a running kernel thread.
        let current_ptr = unsafe { ThreadPtr::current() };
        // SAFETY: `current_ptr` is the currently executing thread.
        let current = unsafe { current_ptr.as_ref() };

        // Test `lock_for_baao_operation_or_backoff` + `cancel_baao_operation_locked`.
        let mut details_owner_1 = Some(current_ptr);
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_cancel_baao_1"),
            |token| -> TransactionResult<()> {
                // SAFETY: The transaction holds no locks and `current` is the current thread.
                let Some((details, owq_guard, current_guard)) =
                    (unsafe { owq_ref.lock_for_baao_operation_or_backoff(current, None, token) })
                else {
                    return Action::Backoff.into();
                };
                details_owner_1 = details.new_owner();
                ChainLockTransaction::finalize(token);
                // SAFETY: `details`, `owq_guard`, and `current_guard` were produced by
                // `lock_for_baao_operation_or_backoff` above and the transaction is finalized.
                unsafe {
                    OwnedWaitQueue::cancel_baao_operation_locked(
                        owq_guard,
                        current_guard,
                        &details,
                        token,
                    );
                }
                DONE.into()
            },
        );
        expect_true!(details_owner_1.is_none());
        expect_true!(owq_ref.lock().is_unlocked());

        // Test `try_lock_for_baao_operation_locked` + `cancel_baao_operation_locked`.
        let mut details_owner_2 = Some(current_ptr);
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_cancel_baao_2"),
            |token| -> TransactionResult<()> {
                let mut owq_guard = OwnerGuard::new(owq_ref, token);
                // SAFETY: `current`'s lock is not held and `current` is the current thread.
                let Some((details, current_guard)) = (unsafe {
                    owq_guard.proof().try_lock_for_baao_operation_locked(current, None, token)
                }) else {
                    owq_guard.release();
                    return Action::Backoff.into();
                };
                details_owner_2 = details.new_owner();
                ChainLockTransaction::finalize(token);
                // SAFETY: `details`, `owq_guard`, and `current_guard` were produced by
                // `try_lock_for_baao_operation_locked` above and the transaction is finalized.
                unsafe {
                    OwnedWaitQueue::cancel_baao_operation_locked(
                        owq_guard,
                        current_guard,
                        &details,
                        token,
                    );
                }
                DONE.into()
            },
        );
        expect_true!(details_owner_2.is_none());
        expect_true!(owq_ref.lock().is_unlocked());

        // Spawn two waiters and wake them one by one using the locked wake APIs.
        let ctx_a = WaiterContext::new(owq_ref, Some(current_ptr));
        // SAFETY: `waiter_a` is joined below before `ctx_a` is destroyed.
        let waiter_a = unwrap_ok!(unsafe { spawn_waiter(&ctx_a) });
        let ctx_b = WaiterContext::new(owq_ref, None);
        // SAFETY: `waiter_b` is joined below before `ctx_b` is destroyed.
        let waiter_b = unwrap_ok!(unsafe { spawn_waiter(&ctx_b) });

        // Wake the first waiter using `lock_for_wake_operation_or_backoff` + `wake_threads_locked`
        // with `WakeOption::AssignOwner`.
        let mut hook_1 = CountHook::default();
        let mut res_1 = EMPTY_RESULT;
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_locked_wake_1"),
            |token| -> TransactionResult<()> {
                // SAFETY: Preemption is disabled by `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                let preempt_token = unsafe { PreemptDisabledToken::new() };
                stack_pin_init!(let unblock_list = ThreadUnblockList::init());
                // SAFETY: `owq_ref.lock()` is not held, `unblock_list` is empty, and the acquired
                // locks are processed by `wake_threads_locked` below.
                let Some(mut guard) = (unsafe {
                    owq_ref.lock_for_wake_operation_or_backoff(
                        1,
                        &mut hook_1,
                        unblock_list.as_mut(),
                        token,
                    )
                }) else {
                    return Action::Backoff.into();
                };
                ChainLockTransaction::finalize(token);
                let mut proof = guard.proof();
                // SAFETY: `unblock_list` was populated by `lock_for_wake_operation_or_backoff`
                // with `max_wake == 1`, and the transaction is finalized.
                res_1 = unsafe {
                    proof.wake_threads_locked(
                        unblock_list.as_mut(),
                        &mut hook_1,
                        WakeOption::AssignOwner,
                        ForceInheritance::No,
                        token,
                        &preempt_token,
                    )
                };
                DONE.into()
            },
        );
        assert_eq!(res_1.woken, 1);
        assert_eq!(res_1.still_waiting, 1);
        assert_true!(res_1.owner.is_some());

        // Wake the second waiter using `lock_for_wake_operation_locked` + `wake_threads_locked`.
        let mut hook_2 = CountHook::default();
        let mut res_2 = EMPTY_RESULT;
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("owq_rust_test_locked_wake_2"),
            |token| -> TransactionResult<()> {
                // SAFETY: Preemption is disabled by `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                let preempt_token = unsafe { PreemptDisabledToken::new() };
                let mut guard = OwnerGuard::new(owq_ref, token);
                let mut proof = guard.proof();
                stack_pin_init!(let unblock_list = ThreadUnblockList::init());
                // SAFETY: `unblock_list` is empty and the acquired locks are processed by
                // `wake_threads_locked` below.
                if !unsafe {
                    proof.lock_for_wake_operation_locked(
                        1,
                        &mut hook_2,
                        unblock_list.as_mut(),
                        token,
                    )
                } {
                    guard.release();
                    return Action::Backoff.into();
                }
                ChainLockTransaction::finalize(token);
                // SAFETY: `unblock_list` was populated by `lock_for_wake_operation_locked` on
                // `owq_ref` in this transaction, and the transaction is finalized.
                res_2 = unsafe {
                    proof.wake_threads_locked(
                        unblock_list.as_mut(),
                        &mut hook_2,
                        WakeOption::None,
                        ForceInheritance::No,
                        token,
                        &preempt_token,
                    )
                };
                DONE.into()
            },
        );
        assert_true!(res_2 == WakeThreadsResult { woken: 1, still_waiting: 0, owner: None });

        // SAFETY: Neither waiter has been joined yet.
        assert_eq!(unwrap_ok!(unsafe { waiter_a.join(InstantMono::INFINITE) }), 0);
        // SAFETY: See above.
        assert_eq!(unwrap_ok!(unsafe { waiter_b.join(InstantMono::INFINITE) }), 0);
        assert_true!(observe_owner(owq_ref).is_none());
    }

    /// Verifies `OwnedWaitQueue` lock proofs and guarded field accessors.
    #[test]
    fn owned_wait_queue_proof_accessors() {
        stack_pin_init!(let owq = OwnedWaitQueue::init());
        let owq_ref: &OwnedWaitQueue = &owq;
        expect_true!(owq_ref.lock().is_unlocked());

        {
            single_chain_lock_guard_owned!(
                scope,
                owq_ref,
                IRQ_SAVE_OPTION,
                clt_tag!("owned_wait_queue_scope")
            );
            expect_true!(owq_ref.lock().is_held(scope.token()));
            let mut proof = scope.proof();
            expect_true!(proof.owner_thread().is_none());
            expect_true!(proof.inherited_scheduler_state_storage().is_none());
            expect_eq!(proof.collection().count(), 0);
            // SAFETY: Calling `.count()` on `collection_mut()` does not mutate the collection, and
            // setting `owner_thread_mut()` and `inherited_scheduler_state_storage_mut()` to `None`
            // on an empty, unowned queue preserves all `OwnedWaitQueue` safety invariants.
            unsafe {
                expect_eq!(proof.collection_mut().count(), 0);
                *proof.owner_thread_mut() = None;
                *proof.inherited_scheduler_state_storage_mut() = None;
            }
            {
                let mut base_proof = proof.as_base();
                expect_true!(core::ptr::eq(base_proof.owner(), &owq_ref.base));
                expect_eq!(base_proof.collection().count(), 0);
                // SAFETY: Calling `.count()` on `collection_mut()` does not mutate the collection.
                expect_eq!(unsafe { base_proof.collection_mut() }.count(), 0);
            }
        }
        expect_true!(owq_ref.lock().is_unlocked());

        owq_ref.reset_owner_if_no_waiters();
        expect_true!(owq_ref.lock().is_unlocked());
    }

    /// Verifies empty `OwnedWaitQueue` FFI operations using `ChainLockProof` and `OwnerGuard`.
    #[test]
    fn empty_owned_wait_queue_operations() {
        stack_pin_init!(let owq = OwnedWaitQueue::init());
        stack_pin_init!(let target_owq = OwnedWaitQueue::init());
        let owq_ref: &OwnedWaitQueue = &owq;
        let target_ref: &OwnedWaitQueue = &target_owq;

        expect_eq!(owq_ref.magic(), OWNED_MAGIC);
        // SAFETY: `owq_ref` is a local queue with no concurrent mutations.
        unsafe {
            owq_ref.assert_not_owned();
            owq_ref.assert_safe_for_pooling();
        }
        expect_true!(owq_ref.assign_owner(None).is_ok());

        // SAFETY: We are executing in a valid kernel thread context.
        let current_thread = unsafe { ThreadPtr::current().as_ref() };
        let mut assign_locked_ok = false;
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("empty_owq_assign_owner_locked"),
            |token| {
                // SAFETY: Preemption is disabled by `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                let preempt_token = unsafe { PreemptDisabledToken::new() };
                let mut thread_guard = OwnerGuard::new(current_thread, token);
                let mut thread_proof = thread_guard.proof();
                // SAFETY: `current_thread` is running and not blocked in any wait queue.
                assign_locked_ok = unsafe {
                    owq_ref.assign_owner_locked(&mut thread_proof, token, &preempt_token)
                }
                .is_ok();
                DONE
            },
        );
        expect_true!(assign_locked_ok);
        owq_ref.reset_owner_if_no_waiters();
        // SAFETY: `owq_ref` is a local queue with no concurrent mutations.
        unsafe {
            owq_ref.assert_not_owned();
        }

        let mut hooks = OwnedWaitQueue::default_wake_hooks();
        let wake_res = owq_ref.wake_threads(1, &mut hooks);
        expect_true!(wake_res == WakeThreadsResult::default());
        let wake_assign_res = owq_ref.wake_thread_and_assign_owner(&mut hooks);
        expect_true!(wake_assign_res == WakeThreadsResult::default());
        let mut requeue_hooks = OwnedWaitQueue::default_wake_hooks();
        let requeue_res = owq_ref.wake_and_requeue(
            target_ref,
            None,
            1,
            1,
            &mut hooks,
            &mut requeue_hooks,
            WakeOption::None,
        );
        expect_true!(requeue_res == WakeThreadsResult::default());

        let mut is_empty = false;
        let mut count = 1;
        let mut front = None;
        let mut owner = None;
        let mut pi_res = ChainLockResult::Backoff;
        let mut locked_res_1 = WakeThreadsResult { woken: 1, still_waiting: 1, owner: None };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("empty_owq_ops"),
            |token| {
                // SAFETY: Preemption is disabled by `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                let preempt_token = unsafe { PreemptDisabledToken::new() };
                {
                    let mut guard = OwnerGuard::new(owq_ref, token);
                    let mut proof = guard.proof();
                    is_empty = proof.is_empty();
                    count = proof.count();
                    front = proof.peek_front();
                    proof.validate_queue();
                    owner = proof.queue_owner();
                    let _profile = proof.get_effective_profile();
                    // SAFETY: The queue is empty and `proof` proves the lock is held.
                    pi_res = unsafe { OwnedWaitQueue::lock_pi_chain(&mut proof, token) };
                    stack_pin_init!(let unblock_list = ThreadUnblockList::init());
                    // SAFETY: `unblock_list` is empty and any locks acquired in `unblock_list` are
                    // processed by `wake_threads_locked` below.
                    let ok = unsafe {
                        proof.lock_for_wake_operation_locked(
                            1,
                            &mut hooks,
                            unblock_list.as_mut(),
                            token,
                        )
                    };
                    assert!(ok, "empty queue lock_for_wake_operation_locked should succeed");
                    ChainLockTransaction::finalize(token);
                    // SAFETY: `unblock_list` was populated by `lock_for_wake_operation_locked` on
                    // `owq_ref` in this transaction, and the transaction is finalized.
                    locked_res_1 = unsafe {
                        proof.wake_threads_locked(
                            unblock_list.as_mut(),
                            &mut hooks,
                            WakeOption::None,
                            ForceInheritance::No,
                            token,
                            &preempt_token,
                        )
                    };
                }
                DONE
            },
        );
        expect_true!(is_empty);
        expect_eq!(count, 0);
        expect_true!(front.is_none());
        expect_true!(owner.is_none());
        expect_true!(pi_res == ChainLockResult::Ok);
        expect_true!(locked_res_1 == WakeThreadsResult::default());

        let mut locked_res_2 = WakeThreadsResult { woken: 1, still_waiting: 1, owner: None };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("empty_owq_lock_for_wake_or_backoff"),
            |token| {
                // SAFETY: Preemption is disabled by `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION`.
                let preempt_token = unsafe { PreemptDisabledToken::new() };
                stack_pin_init!(let unblock_list = ThreadUnblockList::init());
                // SAFETY: `owq_ref.lock()` is not held, `unblock_list` is empty, and the returned
                // `guard` and `unblock_list` are processed and released below.
                let mut guard = unsafe {
                    owq_ref.lock_for_wake_operation_or_backoff(
                        1,
                        &mut hooks,
                        unblock_list.as_mut(),
                        token,
                    )
                }
                .expect("empty queue lock_for_wake_operation_or_backoff should succeed");
                ChainLockTransaction::finalize(token);
                let mut proof = guard.proof();
                // SAFETY: `unblock_list` was populated by `lock_for_wake_operation_or_backoff` on
                // `owq_ref` in this transaction, and the transaction is finalized.
                locked_res_2 = unsafe {
                    proof.wake_threads_locked(
                        unblock_list.as_mut(),
                        &mut hooks,
                        WakeOption::None,
                        ForceInheritance::No,
                        token,
                        &preempt_token,
                    )
                };
                DONE
            },
        );
        expect_true!(locked_res_2 == WakeThreadsResult::default());
        expect_true!(owq_ref.lock().is_unlocked());
    }
}
