// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::futex_context_ffi::{
    cpp_thread_dispatcher_blocking_futex_id, cpp_thread_dispatcher_observe_core_thread,
    cpp_thread_dispatcher_set_blocking_futex_id,
};
use super::handle::HandleValue;
use super::process_dispatcher::ProcessDispatcher;
use super::thread_dispatcher::{
    AutoBlocked, Blocked, CoreThreadObservation, NullableThreadDispatcherGuard, ThreadDispatcher,
    ThreadDispatcherLockClass,
};
use crate::arch_rs::{CopyContext, InterruptDisableGuard, arch_copy_from_user_capture_faults};
use crate::kconcurrent::{
    ChainLockProof, ChainLockTransaction, IRQ_SAVE_OPTION, NO_IRQ_SAVE_OPTION, StateOption,
    StateSaver, clt_tag, single_chain_lock_guard_owned,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::lock_trace::{lock_trace_duration, lock_trace_flow_begin, lock_trace_flow_end};
use crate::kernel::owned_wait_queue::{IWakeRequeueHook, OwnedWaitQueue, WakeOption};
use crate::kernel::thread::{
    AnnotatedAutoEagerReschedDisabler, PreemptDisabledToken, Thread, ThreadPtr, soft_fault,
};
use crate::kernel::wait_queue::{ForceInheritance, Interruptible, ResourceOwnership};
use crate::ktrace_rs::{ArgValue, begin_scope_cond};
use crate::user_copy::{UserInPtr, UserOutPtr};
use core::mem::{MaybeUninit, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, AtomicUsize, Ordering};
use debug::ltrace_entry;
use fbl::{
    DefaultHashable, DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode,
    HashTable, HashTableKeyable, UniquePtr,
};
use ksync::{KCell, KMutex, LockToken, NoIrqSavePolicy, RawSpinlock, guarded, kcell_init};
use object_constants_rs::{kFutexContextStorageAlign, kFutexContextStorageSize};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{ZX_HANDLE_INVALID, ZX_KOID_INVALID, ZX_POL_BAD_HANDLE, zx_futex_t, zx_koid_t};

const LOCAL_TRACE: u32 = 0;

const FUTEX_BLOCK_TRACING_ENABLED: bool = cfg!(futex_block_tracing_enabled);

/// Identifier for a user-mode futex address (`uintptr_t >> 2`).
#[repr(transparent)]
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct FutexId(usize);

impl FutexId {
    /// Shift for a better ID for hashing since the low bits are zero.
    #[inline]
    pub fn new(value_ptr: UserInPtr<zx_futex_t>) -> Self {
        Self(Self::get_futex_id(value_ptr) >> 2)
    }

    /// Creates a [`FutexId`] from a pre-shifted raw identifier value.
    #[inline]
    pub const fn from_raw(id: usize) -> Self {
        Self(id)
    }

    /// Returns the raw shifted futex identifier value (hashtable support).
    #[inline]
    pub const fn get(self) -> usize {
        self.0
    }

    /// Returns the null sentinel [`FutexId`].
    #[inline]
    pub const fn null() -> Self {
        // Since user address spaces always start above 0, 0 is never a legal
        // address and we are free to use it as a sentinel value.
        zr::static_assert!(crate::arch_rs::USER_ASPACE_BASE > 0);
        Self(0)
    }

    /// Returns the hash value for `key`.
    #[inline]
    pub const fn get_hash(key: Self) -> usize {
        key.0
    }

    #[inline]
    fn get_futex_id(value_ptr: UserInPtr<zx_futex_t>) -> usize {
        let futex_id = value_ptr.as_ptr() as usize;
        // The futex ID is the pointer to the futex itself. This pointer may be tagged.
        // As per RFC 143, tagged user pointers are treated the same as their untagged
        // equivalents:
        // https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0143_userspace_top_byte_ignore
        #[cfg(target_arch = "aarch64")]
        let futex_id = crate::arch_rs::vm::detag_ptr(futex_id);
        futex_id
    }
}

// Gets a reference to the thread that the user is asserting is the new owner of
// the futex.  The thread must have the same futex context as the caller since
// futexes may not be owned by threads with different futex contexts.  In addition,
// the new potential owner thread must have been started.  Threads which have not
// started yet may not be the owner of a futex.
//
// Do this before we enter any potentially blocking locks.  Right now, this
// operation can block on BRW locks involved in protecting the global handle
// table, and the penalty for doing so can be severe due to other issues.
// Until these are resolved, we would rather pay the price to do validation
// here instead of while holding the lock.
//
// This said, we cannot bail out with an error just yet.  We need to make it
// into the futex's lock and perform futex state validation first.  See Bug
// #34382 for details.
fn validate_futex_owner(
    new_owner_handle: HandleValue,
) -> Result<Option<fbl::RefPtr<ThreadDispatcher>>, Status> {
    if new_owner_handle.raw_value() == ZX_HANDLE_INVALID {
        return Ok(None);
    }
    let up = ProcessDispatcher::get_current();
    let new_owner = up
        .handle_table()
        .get_dispatcher_with_rights_no_policy_check::<ThreadDispatcher>(new_owner_handle, 0)?;

    // Make sure that the proposed owner of the futex shares our futex context,
    // and that it has been started.
    if !core::ptr::eq(new_owner.process().futex_context(), up.futex_context())
        || !new_owner.has_started()
    {
        return Err(Status::INVALID_ARGS);
    }

    // TODO(johngro): Is this where we signal to the lower level thread that it
    // cannot exit and free itself just yet because we are considering making this
    // thread an owner.  Perhaps thread ref counting needs to come back?
    //
    // If the thread is already DEAD or DYING, don't bother attempting to assign
    // it as a new owner for the futex.
    if new_owner.is_dying_or_dead() {
        return Ok(None);
    }
    Ok(Some(new_owner))
}

#[inline]
fn validate_futex_pointer(value_ptr: UserInPtr<zx_futex_t>) -> Result<(), Status> {
    if !(value_ptr.as_ptr() as usize).is_multiple_of(size_of::<zx_futex_t>()) {
        return Err(Status::INVALID_ARGS);
    }
    if FutexId::new(value_ptr) == FutexId::null() {
        return Err(Status::INVALID_ARGS);
    }
    Ok(())
}

// These mirror C++ `ThreadDispatcher` members accessed via `friend class FutexContext`, and live
// here as private methods so that only `FutexContext` can call them.
impl ThreadDispatcher {
    /// Returns the [`ThreadDispatcher`] associated with `thread` (`thread->user_thread()`), or
    /// `None` if `thread` is a kernel-only thread.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `thread` points to a valid `Thread` and that any associated
    /// `ThreadDispatcher` remains alive for lifetime `'a`.
    #[inline]
    unsafe fn from_core_thread<'a>(thread: ThreadPtr) -> Option<&'a Self> {
        // SAFETY: `thread` is valid per caller contract; `user_thread_raw` returns a valid
        // `ThreadDispatcher*` or null.
        unsafe { thread.user_thread_raw().as_ref() }
    }

    /// Returns the ID of the futex this thread is currently waiting on (`blocking_futex_id_`), or
    /// 0 if it is not waiting on any futex at the moment.
    ///
    /// There is no good (cost free) way to statically express that this thread belongs to a
    /// specific process/futex-context, and therefore that the thread's futex-context lock can be
    /// used to guard this futex ID, so no [`LockToken`] is required here.
    ///
    /// # Safety
    ///
    /// The caller must follow `FutexContext`'s synchronization protocol for
    /// `blocking_futex_id_`: depending on context, it must hold the owning `FutexContext`'s state
    /// lock (with a pending-operation reference keeping the thread's futex state alive), or the
    /// thread's chain lock during wake/requeue, or be the thread itself after it has been removed
    /// from all futex wait queues. Merely holding one of these locks outside of that protocol is
    /// insufficient.
    #[inline]
    unsafe fn blocking_futex_id(&self) -> usize {
        // SAFETY: `self` is a valid `ThreadDispatcher`, and access to `blocking_futex_id_` is
        // synchronized per the caller contract.
        unsafe { cpp_thread_dispatcher_blocking_futex_id(self as *const _) }
    }

    /// Sets the ID of the futex this thread is currently waiting on (`blocking_futex_id_`), or 0
    /// if it is not waiting on any futex.
    ///
    /// # Safety
    ///
    /// Same as [`Self::blocking_futex_id`]: the caller must follow `FutexContext`'s
    /// synchronization protocol for `blocking_futex_id_`. Merely holding one of the locks listed
    /// there outside of that protocol is insufficient.
    #[inline]
    unsafe fn set_blocking_futex_id(&self, id: usize) {
        // SAFETY: `self` is a valid `ThreadDispatcher`, and access to `blocking_futex_id_` is
        // synchronized per the caller contract.
        unsafe { cpp_thread_dispatcher_set_blocking_futex_id(self as *const _ as *mut _, id) }
    }

    /// Acquires `core_thread_lock_` and returns a [`CoreThreadObservation`] of `core_thread_`,
    /// mirroring C++ `ThreadDispatcher::ObserveCoreThread()`.
    ///
    /// `core_thread_lock_` is a lower level lock which is needed to satisfy some odd
    /// synchronization requirements when blocking a thread in a futex and declaring a new owner
    /// in the process. It is held until the returned observation is released or dropped.
    ///
    /// Requires the caller to hold this thread's dispatcher lock, as proven by `_token`.
    ///
    /// # Safety
    ///
    /// - `_token` must have been obtained from a guard on **this particular instance's**
    ///   dispatcher lock (e.g. [`NullableThreadDispatcherGuard::token`] for a guard over `self`).
    ///   A [`LockToken`] only identifies a lock class, not a lock instance, so the type system
    ///   cannot enforce this.
    /// - Holding the lock is necessary but not sufficient: the caller must also be following
    ///   `FutexContext`'s synchronization protocol (holding the owning futex context's state lock
    ///   and pending-operation references for the threads involved, and obeying its lock
    ///   ordering for `core_thread_lock_` relative to chain locks).
    #[inline]
    unsafe fn observe_core_thread_locked(
        &self,
        _token: &LockToken<'_, ThreadDispatcherLockClass>,
    ) -> CoreThreadObservation {
        let mut obs = MaybeUninit::<CoreThreadObservation>::uninit();
        // SAFETY: `self` is a valid `ThreadDispatcher` whose dispatcher lock is held per the
        // caller contract, and `obs` points to valid uninitialized memory that
        // `cpp_thread_dispatcher_observe_core_thread` initializes.
        unsafe {
            cpp_thread_dispatcher_observe_core_thread(self as *const _ as *mut _, &raw mut obs);
            obs.assume_init()
        }
    }
}

struct WakeHook;

impl IWakeRequeueHook for WakeHook {
    fn on_wake_or_requeue(&mut self, t: &mut ChainLockProof<'_, Thread>) {
        let t = ThreadPtr::from(t.owner());
        // Why is this safe?  Because there are two places that a blocking
        // futex ID gets set.  By a thread while it is running and enters
        // FutexWait, or when a thread has been successfully dequeued from an
        // OWQ and is in the process of being woken or requeued.
        //
        // For a thread to block in any queue (FutexWait), owned or not, it must
        // hold its lock exclusively, and will drop it as it finishes the operation
        // and its CPU switches to a different thread.  By the time it is possible
        // for us to hold the thread's lock and observe it in the blocked state, any
        // mutations to the blocking futex id must have "happened before" we were
        // able to observe and wake the thread.
        //
        // Likewise, to unblock the thread during the wake operation, we need to be
        // holding thread's lock exclusively (as we are doing here).  In order for
        // the thread to run again after this, it must obtain its own lock as it
        // becomes rescheduled, meaning that any mutations we perform to the
        // blocking_futex_id must be visible to it.
        //
        // TODO(johngro): Consider simply moving the blocking futex ID down to
        // the kernel thread level of things in order to simplify all of this
        // reasoning?  That way, it could easily just be protected by the thread's
        // lock.
        // SAFETY: `t` is a valid `ThreadPtr` whose ChainLock is held by the caller.
        if let Some(user_thread) = unsafe { ThreadDispatcher::from_core_thread(t) } {
            // SAFETY: `t`'s ChainLock is held during the wake operation.
            unsafe { user_thread.set_blocking_futex_id(FutexId::null().get()) };
            // SAFETY: `t` is a valid `ThreadPtr` whose ChainLock is held.
            lock_trace_flow_end!("contend_futex", unsafe { t.lock_flow_id() });
        }
    }
}

struct RequeueHook<'a, 'b> {
    wake_futex_ref: &'a PendingOpRef<'b>,
    requeue_futex_ref: &'a PendingOpRef<'b>,
    new_id: FutexId,
}

impl IWakeRequeueHook for RequeueHook<'_, '_> {
    fn on_wake_or_requeue(&mut self, t: &mut ChainLockProof<'_, Thread>) {
        let t = ThreadPtr::from(t.owner());
        // SAFETY: `t` is a valid `ThreadPtr` whose ChainLock is held by the caller.
        if let Some(user_thread) = unsafe { ThreadDispatcher::from_core_thread(t) } {
            // SAFETY: `t`'s ChainLock is held during the requeue operation.
            unsafe { user_thread.set_blocking_futex_id(self.new_id.get()) };
        }

        // As we requeue threads, we need to transfer their pending operation
        // counts from the FutexState that they went to sleep on, over to the
        // FutexState they are being requeued to.
        //
        // Sadly, this needs to be done from within the context of the requeued
        // thread's lock.  Failure to do this means that it would be possible for us
        // to requeue a thread from futex A over to futex B, then have that thread
        // time out from the futex before we have move the pending operation
        // references from A to B.  If the thread manages wake up and attempts to
        // drop its pending operation count on futex B before we have transferred
        // the count, it would result in a bookkeeping error.
        self.requeue_futex_ref.take_refs(self.wake_futex_ref, 1);
    }
}

// Notes about FutexState lifecycle.
// aka. Why is this safe?
//
// FutexState objects are used to track the state of any futex which currently
// has waiters.  Currently, each thread in a process allocates two FutexStates
// and contributes them to its process' futex context's free pool.  When the
// thread exits, it take two FutexStates out of the free pool and lets them
// expire.
//
// There is a master spin lock for each process which protects the sets of
// active and free FutexStates.  Any time a thread needs to work with futex
// ID X, it must first obtain the process-wide pool lock and either find the
// FutexState in the active set with that ID, or activate one from the free
// list.  After this, the process wide pool lock is immediately released.
//
// In order to keep this FutexState from disappearing out from under
// the thread during its Wait/Wake/Requeue operation, a "pending operation"
// ref count is increased in the FutexState object.  FutexStates are returned
// to the free pool _only_ when the pending operation count reaches zero.
//
// FutexState objects are managed using UniquePtr.  At all times, a
// FutexState will be in one of three states.
//
// 1) A member of a FutexContext's active_futexes hashtable.  Futexes in this state are
//    currently involved in at least one futex operation.  Their futex ID will
//    be non-zero as will their pending operation count..
// 2) A member of a FutexContext's free_futexes list.  These futexes are
//    not currently in use, but are available to be allocated and used.
//    Their futex ID and pending operation count will be zero.
// 3) A member of neither.  These futexes have been created, but not added
//    to the pool yet, or removed from the free list by a thread which is
//    exiting.  Their futex ID and pending operation count will be zero.
#[guarded]
#[repr(C)]
#[derive(DoublyLinkedListContainable, fbl::Recyclable)]
#[pin_data]
struct FutexState {
    #[pin]
    #[dll_node]
    node: DoublyLinkedListNode<FutexState>,
    // `id` is atomic so that it can be read without holding the pool lock (e.g. as the
    // `active_futexes` key). It is only modified under the pool lock while the state is not in
    // `active_futexes` (see `set_id`), so the key is stable whenever the hash table observes it.
    id: AtomicUsize,
    #[pin]
    waiters: OwnedWaitQueue,

    // pending operation count is protected by the outer FutexContext pool lock.
    // Sadly, there is no good way to express this using static annotations.
    pending_operation_count: AtomicU32,

    #[pin]
    #[mutex(flags = lockdep::LOCK_FLAGS_MULTI_ACQUIRE)]
    lock: KMutex<RawSpinlock>,
}

// SAFETY: `FutexState` node links and atomic fields are only mutated while holding the owning
// `FutexContext`'s `pool_lock`, and `waiters`/`lock` are internally synchronized.
unsafe impl Send for FutexState {}
// SAFETY: Shared references to `FutexState` synchronize access via `FutexContext::pool_lock` and
// `FutexState::lock`.
unsafe impl Sync for FutexState {}

impl FutexState {
    fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            node: DoublyLinkedListNode::new(),
            id: AtomicUsize::new(FutexId::null().get()),
            waiters <- OwnedWaitQueue::init(),
            pending_operation_count: AtomicU32::new(0),
            lock <- KMutex::init(),
        })
    }

    fn try_new() -> Result<UniquePtr<Self>, Status> {
        let uninit = kalloc::Box::<Self>::try_new_uninit().map_err(|_| Status::NO_MEMORY)?;
        let raw_uninit = kalloc::Box::into_raw(uninit);
        // SAFETY: `raw_uninit` is a valid, uniquely owned pointer to uninitialized heap memory
        // for `FutexState`, and `Self::init()` is infallible.
        unsafe {
            let _ = Self::init().__pinned_init(raw_uninit.cast::<Self>());
            Ok(UniquePtr::from_raw(raw_uninit.cast::<Self>()))
        }
    }

    #[inline]
    fn id(&self) -> FutexId {
        FutexId::from_raw(self.id.load(Ordering::Relaxed))
    }

    /// Sets the futex ID of this state.
    ///
    /// The ID is the key of this state in `FutexContext::active_futexes`, so it must not change
    /// while the state is in that table: it is set (from null) just before the state is inserted,
    /// and reset (to null) only after the state has been erased.
    #[inline]
    fn set_id(&self, id: FutexId, _token: &LockToken<'_, FutexContextPoolLockClass>) {
        debug_assert!((id == FutexId::null()) != (self.id() == FutexId::null()));
        self.id.store(id.get(), Ordering::Relaxed);
    }

    #[inline]
    fn pending_operation_count(&self, _token: &LockToken<'_, FutexContextPoolLockClass>) -> u32 {
        self.pending_operation_count.load(Ordering::Relaxed)
    }

    #[inline]
    fn set_pending_operation_count(
        &self,
        count: u32,
        _token: &LockToken<'_, FutexContextPoolLockClass>,
    ) {
        self.pending_operation_count.store(count, Ordering::Relaxed);
    }
}

impl HashTableKeyable<FutexId> for FutexState {
    type Key<'a> = FutexId;

    // hashtable support
    #[inline]
    fn get_key(&self) -> FutexId {
        self.id()
    }
}

impl DefaultHashable for FutexId {
    #[inline]
    fn hash_key(&self) -> usize {
        Self::get_hash(*self)
    }
}

// PendingOpRef is a simple RAII helper meant to help management of the
// pending operation count ref-counting in a FutexState object.  All of the
// ways to fetch a FutexState from the free/active sets will return a
// PendingOpRef to represent the borrow from the pool instead of a raw
// FutexState pointer.  By default, these object will release a pending
// operation reference when they go out of scope.  They do this under the
// protection of the outer FutexContext's pool_lock, returning the
// FutexState to the FutexContext's free pool when the pending operation
// count reaches zero.
//
// There are a few special extensions to the PendingOpRef added in order to
// support some optimizations in the futex code paths.
//
// :: CancelRef ::
// When a thread is about to block on a futex, it will have a PendingOpRef
// in scope which is holding a pending operation reference to the
// FutexState.  The thread which is about to block needs to keep that
// reference on the FutexState as it sleeps, but it should never make an
// attempt to remove the reference itself when it wakes up again.  There are
// two reasons for this but the most important one is that, because of the
// FutexRequeue operation, it may block on Futex A, but get moved over to
// Futex B while blocking.  By the time it wakes up again, Futex A's
// FutexState may no longer exist.
//
// When a thread has passed all of its checks and it is about to block, it
// has entered the thread lock, and it uses CancelRef in order to cause its
// PendingOpRef object to forget about the reference it is holding as it
// blocks.
//
// :: SetExtraRefs ::
// When a thread calls FutexWake, it will eventually enter the thread lock
// in order to manipulate the targeted FutexState's wait queue.  For every
// thread that it successfully wakes up from the wait queue, the wakeup
// thread assumes responsibility for the woken thread's pending operation
// reference.  This way, a thread which is successfully woken from a
// FutexWake operation does not need to acquire any locks on its way out.
// The thread which woke it up will release its pending operation reference
// for it.  In order to account for these extra references, the waking
// thread may call "SetExtraRefs" to account for the references that it took
// responsibility for during the wake operation.
//
// Likewise, SetExtraRefs gets used on the slow path of a thread unblocking
// from FutexWait.  In the case that a thread unblocks from FutexWait with
// an error (timeout, thread killed, etc...), it will first wake up and find
// it's FutexState using its blocking_futex_id member.  This may not be the
// same futex that it originally blocked on.  Once the thread has found the
// FutexState, it will be holding one pending operation reference as a
// result of the find operation.  It needs to add another to account for the
// pending operation reference it placed on the state when it originally
// went to sleep.  It uses SetExtraRefs to accomplish this.
//
// :: TakeRefs ::
// Finally, during a requeue operation, we are moving threads which are
// currently blocked on futex A over to futex B.  As we do this, we need to
// make sure to move their pending operation references at the same time.
// TakeRefs is the method which allows us to do this.
struct PendingOpRef<'a> {
    // A PendingOpRef is a stack-only construct which exists within the scope
    // of a single FutexContext.  There is no reason why this value ever needs
    // to change over the life of the op-ref.
    ctx: &'a FutexContext,
    state: Option<NonNull<FutexState>>,
    extra_refs: u32,
}

impl<'a> PendingOpRef<'a> {
    #[inline]
    const fn new(ctx: &'a FutexContext, state: Option<NonNull<FutexState>>) -> Self {
        Self { ctx, state, extra_refs: 0 }
    }

    #[inline]
    fn set_extra_refs(&mut self, extra_refs: u32) {
        debug_assert!(self.extra_refs == 0 && self.state.is_some());
        self.extra_refs = extra_refs;
    }

    fn take_refs(&self, other: &Self, count: u32) {
        ksync::lock!(let pool_lock_guard = self.ctx.lock_pool_lock());
        debug_assert!(self.state.is_some());
        debug_assert!(other.state.is_some());
        // SAFETY: Both `self.state` and `other.state` point to active `FutexState`s kept alive by
        // their pending operation reference counts.
        let (state, other_state) =
            unsafe { (self.state.unwrap().as_ref(), other.state.unwrap().as_ref()) };
        let token = pool_lock_guard.token();
        let state_count = state.pending_operation_count(token);
        let other_count = other_state.pending_operation_count(token);
        debug_assert!(state_count > 0);
        debug_assert!(other_count > count);

        state.set_pending_operation_count(state_count + count, token);
        other_state.set_pending_operation_count(other_count - count, token);
    }

    #[inline]
    fn cancel_ref(&mut self) {
        debug_assert!(self.state.is_some());
        self.state = None;
    }

    // Allow comparison against null, and dereferencing of the underlying state pointer.
    #[inline]
    const fn is_null(&self) -> bool {
        self.state.is_none()
    }

    #[inline]
    fn state(&self) -> &'a FutexState {
        // SAFETY: `self.state` points to a valid `FutexState` kept alive in `active_futexes` by our
        // non-zero `pending_operation_count`.
        unsafe { self.state.expect("PendingOpRef is null").as_ref() }
    }

    fn release(&mut self) {
        if let Some(state_ptr) = self.state.take() {
            ksync::lock!(let mut pool_lock_guard = self.ctx.lock_pool_lock());
            let release_count = 1 + self.extra_refs;

            // SAFETY: `state_ptr` is a valid active `FutexState` owned by `self.ctx`.
            let state = unsafe { state_ptr.as_ref() };
            let token = pool_lock_guard.token();
            let old_count = state.pending_operation_count(token);

            debug_assert!(state.id() != FutexId::null());
            debug_assert!(old_count >= release_count);

            let new_count = old_count - release_count;
            state.set_pending_operation_count(new_count, token);
            if new_count == 0 {
                // SAFETY: `state` has a non-zero pending operation count (until just now), so it is
                // in `self.ctx.active_futexes`, and its ID (the hash table key) has not changed
                // since it was inserted (see `FutexState::set_id`).
                let removed = unsafe {
                    pool_lock_guard
                        .as_mut()
                        .active_futexes_mut()
                        .erase_element(state)
                        .expect("active FutexState must be in the active futex table")
                };
                pool_lock_guard.as_mut().free_futexes_mut().push_front(removed);
                let token = pool_lock_guard.token();
                state.set_id(FutexId::null(), token);
                // SAFETY: `pool_lock` is held and `pending_operation_count` just reached 0, so no
                // concurrent operations are mutating `state.waiters`.
                unsafe {
                    state.waiters.assert_not_owned();
                }
            }
        }
    }
}

impl Drop for PendingOpRef<'_> {
    #[inline]
    fn drop(&mut self) {
        self.release();
    }
}

/// Owner action is an enum used to signal what to do when threads are woken
/// from a futex.  The defined behaviors are as follows.
///
/// RELEASE
/// Remove any owner regardless of how many threads are woken (including zero
/// threads)
///
/// ASSIGN_WOKEN
/// Only permitted when wake_count is exactly 1.  Assign ownership to the
/// thread who was woken if there was a thread to wake, and there are still
/// threads left in the futex after waking.  Otherwise, set the futex queue
/// owner to nothing.
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum OwnerAction {
    /// Remove any owner regardless of how many threads are woken (including zero threads).
    Release = 0,
    /// Assign ownership to the woken thread if there are still threads left in the futex after
    /// waking; otherwise clear ownership. Only permitted when `wake_count` is 1.
    AssignWoken = 1,
}

/// See [`FutexContext::init`].
#[repr(u32)]
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum ProcessType {
    /// A standard standalone process with a single address space.
    Regular = 0,
    /// A process belonging to a group of processes sharing a `ShareableProcessState`.
    Shared = 1,
}

type ActiveFutexMap =
    HashTable<FutexId, UniquePtr<FutexState>, DoublyLinkedList<UniquePtr<FutexState>>>;

zr::static_assert_size_and_align!(
    FutexContext,
    kFutexContextStorageSize,
    kFutexContextStorageAlign
);

/// FutexContext
///
/// A FutexContext is the object which manages the state of all of the active
/// futexes for a user-mode process.  Each ProcessDispatcher in the system will
/// have a single FutexContext contained within it, and the objects should exist
/// no-where else in the system.
///
/// FutexContexts manage a pool of FutexContext::FutexStates which are
/// contributed by threads created within the process.  This pool guarantees that
/// threads are guaranteed to be able to allocate a FutexState object in O(1)
/// time whenever they perform a FutexWait operation, as a Futex is only "active"
/// when it has any waiters.  See (Grow|Shrink)FutexStatePool comments as well as
/// the FutexState's notes (above) for more details.
///
/// The remaining methods in the public interface implement the 3 primary futex
/// syscall operations (Wait, Wake, and Requeue) as well as the one
/// test/diagnostic operation (GetOwner).  See the zircon syscall documentation
/// for further details.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct FutexContext {
    // Protects the free futex pool, and active futex table.  This is an
    // irq-disable spin lock because it should _never_ be held during any blocking
    // operations.  Only when putting FutexStates into and out of the free pool,
    // and when moving Futexes states to and from the active table.
    //
    // There are times where an individual futex state must be held invariant
    // while a decision to return a futex into the free pool needs to be made.  In
    // these cases, the pool lock must be acquired *after* the individual
    // FutexState lock.  Sadly, I don't know a good way to express this with
    // static analysis.
    //
    // Note that lockdep tracking is disabled on this lock because it is acquired
    // while holding the thread lock.
    #[pin]
    #[mutex(flags = lockdep::LOCK_FLAGS_TRACKING_DISABLED)]
    pool_lock: KMutex<RawSpinlock>,

    // Map containing FutexStates currently in use (eg; futexes with waiters).
    #[guarded_by(pool_lock)]
    active_futexes: ActiveFutexMap,

    // Free list for all futexes which are currently not in use.
    #[pin]
    #[guarded_by(pool_lock)]
    free_futexes: DoublyLinkedList<UniquePtr<FutexState>>,
}

// SAFETY: All access to `active_futexes` and `free_futexes` is synchronized by `pool_lock`.
unsafe impl Send for FutexContext {}
// SAFETY: All access to `active_futexes` and `free_futexes` is synchronized by `pool_lock`.
unsafe impl Sync for FutexContext {}

#[pinned_drop]
impl PinnedDrop for FutexContext {
    fn drop(self: Pin<&mut Self>) {
        ltrace_entry!();

        // All of the threads should have removed themselves from wait queues and
        // destroyed themselves by the time the process has exited.
        let this = self.project();
        debug_assert!(this.active_futexes.get_inner_mut().is_empty());
        debug_assert!(this.free_futexes.get_pinned_mut().is_empty());
    }
}

impl FutexContext {
    /// Creates an in-place initializer for a [`FutexContext`].
    pub fn new() -> impl PinInit<Self, core::convert::Infallible> {
        ltrace_entry!();
        pin_init!(Self {
            pool_lock <- KMutex::init(),
            active_futexes: KCell::new(ActiveFutexMap::new()),
            free_futexes <- kcell_init(DoublyLinkedList::new()),
        })
    }

    /// Initialize this instance for use in the specified process type.
    ///
    /// Must be called exactly once.
    pub fn init(self: Pin<&mut Self>, process_type: ProcessType) -> Result<(), Status> {
        #[allow(unreachable_patterns)]
        let num_buckets = match process_type {
            ProcessType::Regular => 37,
            // A FutexContext used by a group of shared processes tends to have *many* active
            // futexes.
            ProcessType::Shared => 1031,
            _ => panic!("unknown ProcessType {}", process_type as u32),
        };

        let active_futexes = ActiveFutexMap::try_new(num_buckets).map_err(|_| Status::NO_MEMORY)?;

        {
            ksync::lock!(let mut pool_lock_guard = self.lock_pool_lock());
            let current = pool_lock_guard.as_mut().active_futexes_mut();
            debug_assert!(!current.is_initialized());
            *current = active_futexes;
        }

        Ok(())
    }

    /// Called as ThreadDispatchers are created and destroyed in order to ensure
    /// that there are always two FutexStates for each ThreadDispatcher in a
    /// process.
    ///
    /// Why two and not one?  Because of the FutexRequeue operation.  Without
    /// requeue, we would only need one, since a futex with waiters requires one
    /// futex state, and there can be at most N futexes with waiters, where N is
    /// the number of threads in a process.
    ///
    /// During FutexRequeue, however, a thread needs to grab a hold of two futex
    /// contexts at the same time.  In addition, the thread performing this
    /// operation is no longer holding a process wide futex context lock.  Instead,
    /// it simply locks in order to activate the FutexStates and then unlocks.
    /// Another thread can attempt a requeue in parallel, or it could exit in
    /// parallel.  If only one FutexState were added for each thread, it would be
    /// possible to run out of FutexStates if these operations were happening in
    /// parallel.
    pub fn grow_futex_state_pool(&self) -> Result<(), Status> {
        let new_state1 = FutexState::try_new()?;
        let new_state2 = FutexState::try_new()?;

        ksync::lock!(let mut pool_lock_guard = self.lock_pool_lock());
        let mut free_futexes = pool_lock_guard.as_mut().free_futexes_mut();
        free_futexes.as_mut().push_front(new_state1);
        free_futexes.as_mut().push_front(new_state2);
        Ok(())
    }

    /// Removes two unused `FutexState` instances from the free pool when a thread exits.
    pub fn shrink_futex_state_pool(&self) {
        let (_state1, _state2) = {
            // Do not let the futex state become released inside of the lock.
            ksync::lock!(let mut pool_lock_guard = self.lock_pool_lock());
            debug_assert!(!pool_lock_guard.free_futexes().is_empty());
            let mut free_futexes = pool_lock_guard.as_mut().free_futexes_mut();
            let s1 = free_futexes.as_mut().pop_front();
            let s2 = free_futexes.as_mut().pop_front();
            (s1, s2)
        };
    }

    // Find the futex state for a given ID in the futex table, increment its
    // pending operation reference count, and return an RAII helper which helps to
    // manage the pending operation references.
    fn find_active_futex(&self, id: FutexId) -> PendingOpRef<'_> {
        ksync::lock!(let pool_lock_guard = self.lock_pool_lock());
        self.find_active_futex_locked(id, &pool_lock_guard)
    }

    fn find_active_futex_locked<'a>(
        &'a self,
        id: FutexId,
        guard: &FutexContextPoolLockGuard<'a>,
    ) -> PendingOpRef<'a> {
        debug_assert!(core::ptr::eq(self, guard.parent()));
        if let Some(state) = guard.active_futexes().find(&id) {
            let token = guard.token();
            let count = state.pending_operation_count(token);
            debug_assert!(count > 0);
            state.set_pending_operation_count(count + 1, token);
            return PendingOpRef::new(self, Some(NonNull::from(state)));
        }

        PendingOpRef::new(self, None)
    }

    // Find a futex with the specified ID, increment its pending_operation_count
    // and return it to the caller.  If the given futex ID is not currently
    // active, grab a free one and activate it.
    fn activate_futex(&self, id: FutexId) -> PendingOpRef<'_> {
        ksync::lock!(let mut pool_lock_guard = self.lock_pool_lock());
        self.activate_futex_locked(id, pool_lock_guard.as_mut())
    }

    fn activate_futex_locked<'a>(
        &'a self,
        id: FutexId,
        mut guard: Pin<&mut FutexContextPoolLockGuard<'a>>,
    ) -> PendingOpRef<'a> {
        let existing = self.find_active_futex_locked(id, &guard);
        if !existing.is_null() {
            return existing;
        }

        let new_state =
            guard.as_mut().free_futexes_mut().pop_front().expect("free_futexes must not be empty");

        // Sanity checks.
        let token = guard.token();
        debug_assert_eq!(new_state.id(), FutexId::null());
        debug_assert_eq!(new_state.pending_operation_count(token), 0);
        // SAFETY: `pool_lock` is held and `new_state` was just popped from `free_futexes` with a
        // zero `pending_operation_count`, so no concurrent operations are mutating `waiters`.
        unsafe {
            new_state.waiters.assert_not_owned();
        }

        let ptr = NonNull::from(&*new_state);
        new_state.set_id(id, token);
        new_state.set_pending_operation_count(1, token);
        guard.as_mut().active_futexes_mut().insert(new_state);

        PendingOpRef::new(self, Some(ptr))
    }

    /// FutexWait first verifies that the integer pointed to by `value_ptr` still equals
    /// `current_value`. If the test fails, FutexWait returns BAD_STATE.  Otherwise it will block
    /// the current thread until the `deadline` passes, or until the thread is woken by a FutexWake
    /// or FutexRequeue operation on the same `value_ptr` futex.
    ///
    /// Note that this method and FutexRequeue both take a user mode handle instead of having the
    /// syscall dispatch layer resolve the handle into a thread before proceeding.  This is because
    /// we need to perform the `current_value == *value_ptr` check before attempting to validate
    /// the thread handle, and this check needs to happen inside of the futex context lock.  To do
    /// otherwise leaves the potential to hit a race condition where we end up appearing to violate
    /// the "bad handle" policy when actually we didn't.  See <https://fxbug.dev/42109683> for
    /// details.
    /// May block on page requests and must be called without locks held.
    pub fn futex_wait(
        &self,
        value_ptr: UserInPtr<zx_futex_t>,
        current_value: i32,
        new_futex_owner: HandleValue,
        deadline: &Deadline,
    ) -> Result<(), Status> {
        ltrace_entry!();

        // As we may need to perform fault resolution later, ensure our caller is not holding any
        // locks.
        lockdep::assert_no_locks_held();

        // Make sure the futex pointer is following the basic rules.
        validate_futex_pointer(value_ptr)?;

        let validator_result = validate_futex_owner(new_futex_owner);
        let futex_owner_thread = validator_result.as_ref().ok().and_then(Option::as_deref);

        // SAFETY: Called from a running user thread context.
        let current_core_thread = unsafe { ThreadPtr::current() };
        let current_thread = ThreadDispatcher::get_current();
        let futex_id = FutexId::new(value_ptr);
        let result = {
            // Obtain the FutexState for the ID we are interested in, activating a free
            // futex state in the process if needed.  This operation should never fail
            // (there should always be a FutexState available to us).
            let mut futex_ref = self.activate_futex(futex_id);
            debug_assert!(!futex_ref.is_null());

            // Now that we have a hold of the FutexState, enter the futex specific lock
            // and validate the user-mode futex state.
            //
            // FutexWait() checks that the address value_ptr still contains
            // current_value, and if so it sleeps awaiting a FutexWake() on value_ptr.
            // Those two steps must together be atomic with respect to FutexWake().  If
            // a FutexWake() operation could occur between them, a user-land mutex
            // operation built on top of futexes would have a race condition that could
            // miss wakeups.
            //
            // Note that we disable involuntary preemption while we are inside of this
            // lock.  The price of blocking while holding this lock is high, and we
            // should not (in theory) _ever_ be inside of this lock for very long at
            // all. The vast majority of the time, we just need validate the state,
            // then trade this lock for the thread lock, and then block.  Even if we are
            // operating at the very end of our slice, it is best to disable preemption
            // until we manage to join the wait queue, or abort because of state
            // validation issues.
            loop {
                // The lock ordering in this loop body is a bit subtle. Here are the locks we
                // acquire in the order we acquire them, along with some rationale as to why
                // they're acquired and released in that order:
                //
                // 1. We start by acquiring the futex_owner_thread's ThreadDispatcher lock, if the
                //    futex owner is not nullptr. We need to hold this lock to verify that the
                //    thread has not died during this Wait operation. It needs to be acquired
                //    before the FutexState's spinlock, as acquiring a mutex while holding a
                //    spinlock is not allowed.
                // 2. Then, we acquire the FutexState's spinlock. We need to hold this lock to
                //    verify that the value of the futex hasn't changed during this Wait
                //    operation. Acquiring this spinlock requires disabling interrupts. We do so
                //    explicitly with an `InterruptDisableGuard` instead of using the `IrqSave`
                //    option in the `Guard` because we acquire the thread lock later in this
                //    method, and we want IRQs to be disabled for this entire duration.
                // 3. We then validate the futex's state and the futex owner's state.
                // 4. Once validation is complete, we observe the "core thread" of the proposed
                //    new owner. This is the Thread* member of the ThreadDispatcher.  If there is
                //    still a valid proposed new owner, we will obtain the dispatcher's "core
                //    thread" lock, which will ensure that the underlying Thread cannot exit even
                //    after we drop the ThreadDispatcher lock.
                // 5. Next, we can obtain the set of ChainLocks needed to block the thread an
                //    assign our (optional) new owner.
                // 6. Once we have these locks, we can drop the FutexState lock and proceed with
                //    the block operation.
                let mut futex_owner_guard = NullableThreadDispatcherGuard::new(futex_owner_thread);
                let irqd = InterruptDisableGuard::new();
                ksync::lock!(
                    let mut futex_state_guard =
                        futex_ref.state().lock_lock_policy::<NoIrqSavePolicy>()
                );

                // Sanity check, bookkeeping should not indicate that we are blocked on
                // a futex at this point in time.
                // SAFETY: `current_thread` is the running thread and cannot be blocked on a futex.
                debug_assert_eq!(
                    unsafe { current_thread.blocking_futex_id() },
                    FutexId::null().get()
                );

                let mut value: i32 = 0;
                // SAFETY: `value` is a valid stack `i32` and `value_ptr` is a user pointer.
                let copy_result = unsafe {
                    arch_copy_from_user_capture_faults(
                        (&raw mut value).cast(),
                        value_ptr.as_ptr().cast(),
                        size_of::<i32>(),
                        CopyContext::BlockingNotAllowed,
                    )
                };
                if let Err(copy_err) = copy_result {
                    // At this point we are committed to either returning from the function, or
                    // restarting the loop, so we:
                    // 1. Drop the futex state lock.
                    // 2. Re-enable interrupts.
                    // 3. Drop the futex owner lock.
                    // 4. Either return with an error or resolve the fault and continue the loop.
                    futex_state_guard.as_mut().release();
                    drop(irqd);
                    futex_owner_guard.release();
                    if let Some(fault) = copy_err.fault_info {
                        soft_fault(fault.pf_va, fault.pf_flags)?;
                        continue;
                    }
                    return Err(copy_err.status);
                }

                if value != current_value {
                    return Err(Status::BAD_STATE);
                }

                if let Err(&status) = validator_result.as_ref() {
                    if status == Status::BAD_HANDLE {
                        let _ = ProcessDispatcher::get_current()
                            .enforce_basic_policy(ZX_POL_BAD_HANDLE);
                    }
                    return Err(status);
                }

                // Now that the futex state has been validated, we can validate the state of the
                // futex owner thread (if one exists). We must do this after validating the futex
                // state to avoid the race condition outlined in https://fxbug.dev/42109683.
                // TODO(johngro): explain how the core thread observation keeps the Thread* core
                // thread valid.
                let mut new_owner_observation = CoreThreadObservation::default();
                if let Some(owner_thread) = futex_owner_thread {
                    // When attempting to wait, the new owner of the futex (if any) may not be
                    // the thread which is attempting to wait.
                    if core::ptr::eq(owner_thread, &*current_thread) {
                        return Err(Status::INVALID_ARGS);
                    }

                    // If we have a valid new owner, then verify that this thread is not already
                    // waiting on the target futex.
                    // SAFETY: `futex_state_guard` and `futex_owner_guard` are held.
                    if unsafe { owner_thread.blocking_futex_id() } == futex_id.get() {
                        return Err(Status::INVALID_ARGS);
                    }

                    // We need to resolve |futex_owner_thread|'s Thread*.  First, we must
                    // revalidate that |futex_owner_thread| is not dead or dying as it may have
                    // changed state during the |SoftFault| call above.
                    let owner_token = futex_owner_guard
                        .token()
                        .expect("futex_owner_guard must hold the futex owner's dispatcher lock");
                    if !owner_thread.is_dying_or_dead_locked(&owner_token) {
                        // SAFETY: `owner_token` was obtained from `futex_owner_guard`, which was
                        // created over `futex_owner_thread` (i.e. `owner_thread`) and holds that
                        // same thread's dispatcher lock. We also hold the futex state lock and
                        // are following the `FutexContext` protocol.
                        new_owner_observation =
                            unsafe { owner_thread.observe_core_thread_locked(&owner_token) };
                    }
                }

                // We have now passed all of our checks and are committed to becoming
                // blocked.  We are currently holding a few different locks/guards, but do
                // not yet hold the set of chain locks we will need to perform the final
                // block and assign owner operation.  We will exchange the spinlocks we
                // are holding for the ChainLocks we need, but _before_ we can do that, we
                // need to release the "futex_owner_guard", which is holding a mutex (if
                // we still have a valid proposed owner).
                //
                // We need to release this mutex before we start obtaining ChainLocks; if
                // we release the mutex and discover that we need to wake a waiter, we
                // need to make sure we are not already in the middle of a
                // ChainLockTransaction (since we will need to conduct a transaction in
                // order to unblock the next waiter)
                //
                // We also need to make sure that local rescheduling (at least) has been
                // disabled.  We are currently holding spinlocks, and even if we do end up
                // unblocking another thread, we cannot reschedule until we drop those
                // spinlocks.
                //
                // So, disable all rescheduling for now, both local and "eager
                // rescheduling" (meaning we will hold off on sending IPIs just now).  We
                // are going to block, and when we do, all of the pending reschedule
                // operations (local and remote) will be automatically flushed for us.
                let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
                futex_owner_guard.release(); // This is now OK since we have suppressed
                // local rescheduling.

                // Now we can go ahead and obtain the ChainLocks we need, and once we have
                // them, drop the spinlocks we are holding.
                // SAFETY: Called from a running thread context.
                let current_kernel_thread = unsafe { ThreadPtr::current().as_ref() };

                // StateSaver handles the relax operation during backoff. Interrupts are already
                // disabled by `irqd`.
                let mut state_saver = NO_IRQ_SAVE_OPTION.save_state();

                // Create a bare transaction to avoid tripping over all of the existing guards and
                // auto state save/restore RAII objects above.
                // TODO(eieio): Figure out how to refactor this method into a pattern that is
                // amedable to the standard chain lock transaction pattern.
                let transaction = ChainLockTransaction::new_unregistered(
                    clt_tag!("FutexContext::FutexWaitInternal"),
                    0,
                    false,
                );
                // SAFETY: `transaction` is never moved out of this stack slot, and is dropped (and
                // thereby unregistered) at the end of this loop iteration, before `state_saver`
                // and `irqd`, which keep interrupts disabled while it is registered. `token` does
                // not outlive `transaction`. Blocking below goes through the scheduler, which saves
                // and restores the active transaction. `register` asserts that no other
                // transaction is active: this method only runs on the futex syscall path, which
                // may block and so is never entered from inside a transaction, and nothing above
                // starts one except possibly the release of `futex_owner_guard`, whose wake
                // transaction has completed by the time `release` returns.
                let token = unsafe { transaction.register() };

                let new_owner = new_owner_observation.core_thread();
                let (locking_details, waiters_guard, mut current_thread_guard) = loop {
                    // SAFETY: `current_kernel_thread` is the current thread, `new_owner` is kept
                    // valid by `new_owner_observation`, and no chain locks are held.
                    if let Some(baao) = unsafe {
                        futex_ref.state().waiters.lock_for_baao_operation_or_backoff(
                            current_kernel_thread,
                            new_owner.map(|owner| owner.as_ref()),
                            &token,
                        )
                    } {
                        break baao;
                    }
                    // No other chain locks are held. Simply try again until the locks are obtained.
                    transaction.assert_num_locks_held(0);
                    state_saver.relax(&transaction);
                };

                ChainLockTransaction::finalize(&token);

                // We now have all of the locks we need in order to block our thread.
                // We can now drop the:
                //
                // 1) New owner core lock (if any).  We have the new prospective owner
                //    Thread's ChainLock held, it cannot exit out from under us at this
                //    point.
                // 2) The futex state guard.  We have the futex's OwnedWaitQueue
                //    ChainLock, so it is not possible for another thread to wake
                //    threads from the Futex's OWQ before this thread manages to become
                //    blocked.
                new_owner_observation.release();
                futex_state_guard.as_mut().release();

                // Record the futex ID of the thread we are about to block on.
                // SAFETY: `current_thread` is the running thread and its ChainLock and the wait
                // queue's ChainLock are held.
                unsafe { current_thread.set_blocking_futex_id(futex_id.get()) };

                // Mark that we are now blocked-by-futex.
                let _by = AutoBlocked::new(Blocked::FUTEX);

                // Finally, drop into the block operation.  This will take care of
                // releasing the queue's lock, as well as any of the locks involved in
                // ownership propagation.  The current thread's lock will be held right
                // up until the point where it is context switched away from, when it
                // finally will be dropped.  It will be re-obtained later on after the
                // thread has woken and become rescheduled, and we will need to drop it
                // ourselves as we unwind.
                let block_result = {
                    let blocking_tid = new_owner
                        // SAFETY: `new_owner`'s ChainLock is held.
                        .map(|owner| unsafe { owner.tid() })
                        .unwrap_or(ZX_KOID_INVALID);
                    let _block_tracer = begin_scope_cond!(
                        FUTEX_BLOCK_TRACING_ENABLED,
                        "kernel:sched",
                        "futex_block",
                        "futex_id" => futex_id.get() as u64,
                        "blocking_tid" => blocking_tid,
                    );

                    lock_trace_duration!(
                        "Futex::Wait",
                        "futex_id" => ArgValue::Pointer(futex_id.get()),
                    );

                    // SAFETY: `current_core_thread` is the current thread with its ChainLock held.
                    let flow_id = unsafe { current_core_thread.take_next_lock_flow_id() };
                    lock_trace_flow_begin!("contend_futex", flow_id);

                    // SAFETY: Preemption is disabled by `_eager_resched_disabler`.
                    let preempt_disabled_token = unsafe { PreemptDisabledToken::new() };
                    let mut current_thread_proof = current_thread_guard.proof();
                    // SAFETY: All required ChainLocks were acquired by
                    // `lock_for_baao_operation_or_backoff` (producing `locking_details`) and the
                    // transaction was finalized with rescheduling disabled by
                    // `_eager_resched_disabler`. The `FutexState` (and its `waiters` queue) stays
                    // alive until this call returns because `futex_ref` holds a pending operation
                    // reference to it.
                    unsafe {
                        OwnedWaitQueue::block_and_assign_owner_locked(
                            waiters_guard,
                            &mut current_thread_proof,
                            deadline,
                            &locking_details,
                            ResourceOwnership::Normal,
                            Interruptible::Yes,
                            ForceInheritance::No,
                            &token,
                            &preempt_disabled_token,
                        )
                    }
                };

                // Do _not_ allow the PendingOpRef helper to release our pending op
                // reference.  Having just woken up, either the thread which woke us will
                // have released our pending op reference, or we will need to revalidate
                // _which_ futex we were waiting on (because of FutexRequeue) and manage the
                // release of the reference ourselves.
                futex_ref.cancel_ref();

                // We came out of the block operation holding only our current thread's
                // chain lock.  Drop our own lock and break out of the retry loop.
                current_thread_guard.release();
                break block_result;
            }
        };

        // If we were woken by another thread, then our block result will be ZX_OK.
        // We know that the thread has handled releasing our pending op reference, and
        // has reset our blocking futex ID to zero.  No special action should be
        // needed by us at this point.
        if result.is_ok() {
            // The FutexWake operation should have already cleared our blocking
            // futex ID.
            // SAFETY: `current_thread` was woken via `FutexWake` which cleared `blocking_futex_id`
            // under its ChainLock.
            debug_assert_eq!(unsafe { current_thread.blocking_futex_id() }, FutexId::null().get());
            return Ok(());
        }

        // If the result is not ZX_OK, then additional actions may be required by
        // us.  This could be because
        //
        // 1) We hit the deadline (ZX_ERR_TIMED_OUT)
        // 2) We were killed (ZX_ERR_INTERNAL_INTR_KILLED)
        // 3) We were suspended (ZX_ERR_INTERNAL_INTR_RETRY)
        //
        // In any one of these situations, it is possible that we were the last
        // waiter in our FutexState and need to return the FutexState to the free
        // pool as a result.  To complicate things just a bit further, becuse of
        // zx_futex_requeue, the futex that we went to sleep on may not be the futex
        // we just woke up from.  We need to find the futex we were blocked by, and
        // release our pending op reference to it (potentially returning the
        // FutexState to the free pool in the process).
        // SAFETY: `current_thread` is the running thread and has already unblocked from the wait
        // queue, so no Wake/Requeue hook can race on `blocking_futex_id`.
        let blocking_id = FutexId::from_raw(unsafe { current_thread.blocking_futex_id() });
        debug_assert_ne!(blocking_id, FutexId::null());

        let mut futex_ref = self.find_active_futex(blocking_id);
        // SAFETY: `current_thread` is the running thread and is not in any wait queue.
        unsafe { current_thread.set_blocking_futex_id(FutexId::null().get()) };
        // SAFETY: `current_core_thread` is the current thread.
        lock_trace_flow_end!("contend_futex", unsafe { current_core_thread.lock_flow_id() });
        debug_assert!(!futex_ref.is_null());

        // Record the fact that we are holding an extra reference.  The first
        // reference was placed on the FutexState at the start of this method as we
        // fetched the FutexState from the pool.  This reference was not removed by a
        // waking thread because we just timed out, or were killed/suspended.
        //
        // The second reference was just added during the FindActiveFutex (above).
        futex_ref.set_extra_refs(1);

        // Deal with ownership of the futex.  It is possible that we were the last
        // thread waiting on the futex, but that the futex's wait queue still has an
        // owner assigned.  We need to obtain the locks all of the proper locks, then
        // (if the waiters OWQ has no threads blocked in it) reset the owner.
        //
        // Note: We should not need the actual FutexState lock at this point in time.
        // We know that the FutexState cannot disappear out from under us (we are
        // holding two pending operation references), and once we are inside of the
        // thread lock, we no that no new threads can join the wait queue.  If there
        // is a thread racing with us to join the queue, then it will go ahead and
        // explicitly update ownership as it joins the queue once it has made it
        // inside of the thread lock.
        //
        // No `ChainLockTransaction` is active (as `reset_owner_if_no_waiters` asserts) and
        // `waiters`' chain lock is not held: the bare transaction used to block ended with the
        // retry loop's scope above, after the only chain lock still held on wakeup (the current
        // thread's) was released.
        futex_ref.state().waiters.reset_owner_if_no_waiters();
        result
    }

    /// FutexWake will wake up to `wake_count` number of threads blocked on the `value_ptr` futex.
    ///
    /// If `owner_action` is set to `Release`, then the futex's owner will be set to `None` in the
    /// process.  If the `owner_action` is set to `AssignWoken`, then the `wake_count` *must* be 1,
    /// and the futex's owner will be set to the thread which was woken during the operation, or
    /// `None` if no thread was woken.
    pub fn futex_wake(
        &self,
        value_ptr: UserInPtr<zx_futex_t>,
        wake_count: u32,
        owner_action: OwnerAction,
    ) -> Result<(), Status> {
        ltrace_entry!();

        // Make sure the futex pointer is following the basic rules.
        validate_futex_pointer(value_ptr)?;

        // Try to find an active futex with the specified ID.  If we cannot find one,
        // then we are done.  This wake operation had no threads to wake.
        let futex_id = FutexId::new(value_ptr);
        let mut futex_ref = self.find_active_futex(futex_id);
        if futex_ref.is_null() {
            return Ok(());
        }

        // We found an "active" futex, meaning its pending operation count was
        // non-zero when we went looking for it.  Now enter the FutexState specific
        // lock and see if there are any actual waiters to wake up.
        {
            // Optimize lock contention by delaying local/remote reschedules until the mutex is
            // released.
            let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
            ksync::lock!(let _guard = futex_ref.state().lock_lock());

            // Now actually wake up the threads. OwnedWakeQueue will handle the
            // ownership bookkeeping for us.
            {
                lock_trace_duration!(
                    "Futex::Wake",
                    "futex_id" => ArgValue::Pointer(futex_id.get()),
                );

                // Attempt to wake |wake_count| threads.  Count the number of thread that
                // we have successfully woken, and assign each of their blocking futex IDs
                // to 0 as we go.  We need an accurate count in order to properly adjust
                // the pending operation ref count on our way out of this function.
                let wake_result = {
                    let mut hook = WakeHook;
                    // The futex state lock held by `_guard` is a spinlock rather than a chain
                    // lock, and `_eager_resched_disabler` only defers rescheduling, so no
                    // `ChainLockTransaction` is active, as the wake operations assert. `WakeHook`
                    // does not re-enter the wait queue.
                    if owner_action == OwnerAction::Release {
                        futex_ref.state().waiters.wake_threads(wake_count, &mut hook)
                    } else {
                        debug_assert_eq!(owner_action, OwnerAction::AssignWoken);
                        debug_assert_eq!(wake_count, 1);
                        futex_ref.state().waiters.wake_thread_and_assign_owner(&mut hook)
                    }
                };

                // Adjust the number of pending operation refs we are about to release.  In
                // addition to the ref we were holding when we started the wake operation, we
                // are also now responsible for the refs which were being held by each of the
                // threads which we have successfully woken.  Those threads are exiting along
                // the FutexWait hot-path, and they have expected us to manage their
                // blocking_futex_id and pending operation references for them.
                futex_ref.set_extra_refs(wake_result.woken);
            }
        }

        Ok(())
    }

    /// FutexRequeue first verifies that the integer pointed to by `wake_ptr` still equals
    /// `current_value`. If the test fails, FutexRequeue returns BAD_STATE.  Otherwise it will wake
    /// up to `wake_count` number of threads blocked on the `wake_ptr` futex.  If any other threads
    /// remain blocked on the `wake_ptr` futex, up to `requeue_count` of them will then be
    /// requeued to the tail of the list of threads blocked on the `requeue_ptr` futex.
    ///
    /// If `owner_action` is set to `Release`, then the futex's owner will be set to `None` in the
    /// process.  If the `owner_action` is set to `AssignWoken`, then the `wake_count` *must* be 1,
    /// and the futex's owner will be set to the thread which was woken during the operation, or
    /// `None` if no thread was woken.
    /// May block on page requests and must be called without locks held.
    #[allow(clippy::too_many_arguments)]
    pub fn futex_requeue(
        &self,
        wake_ptr: UserInPtr<zx_futex_t>,
        wake_count: u32,
        current_value: i32,
        owner_action: OwnerAction,
        requeue_ptr: UserInPtr<zx_futex_t>,
        requeue_count: u32,
        new_requeue_owner_handle: HandleValue,
    ) -> Result<(), Status> {
        ltrace_entry!();

        // As we may need to perform fault resolution later, ensure our caller is not holding any
        // locks.
        lockdep::assert_no_locks_held();

        // Make sure the futex pointers are following the basic rules.
        validate_futex_pointer(wake_ptr)?;
        validate_futex_pointer(requeue_ptr)?;

        let wake_id = FutexId::new(wake_ptr);
        let requeue_id = FutexId::new(requeue_ptr);

        if wake_id == requeue_id {
            return Err(Status::INVALID_ARGS);
        }

        // Validate the proposed new owner outside of any FutexState locks, but take
        // no action just yet.  See the comment in FutexWait for details.
        let validator_result = validate_futex_owner(new_requeue_owner_handle);
        let requeue_owner_thread = validator_result.as_ref().ok().and_then(Option::as_deref);

        // Find the FutexState for the wake and requeue futexes.
        let (mut wake_futex_ref, requeue_futex_ref) = {
            ksync::lock!(let mut ref_lookup_guard = self.lock_pool_lock());
            let wake_ref = self.activate_futex_locked(wake_id, ref_lookup_guard.as_mut());
            let requeue_ref = self.activate_futex_locked(requeue_id, ref_lookup_guard.as_mut());
            debug_assert!(!wake_ref.is_null());
            debug_assert!(!requeue_ref.is_null());
            // Manually release the ref lookup guard.  While we would typically do this
            // using scope, the PendingOpRefs need to live outside of just the locking
            // scope.  We cannot declare the PendingOpRefs outside of the scope because we
            // do not allow default construction of PendingOpRefs, nor do we allow move
            // assignment.  This is done on purpose; pending op refs should only ever be
            // constructed during lookup operations, and they really should not be moved
            // around.  We need to have a move constructor, but there is no reason for a
            // move assignment.
            (wake_ref, requeue_ref)
        };

        loop {
            // See the comment in FutexWait about the structure of lock ordering in this method.
            let mut requeue_owner_guard = NullableThreadDispatcherGuard::new(requeue_owner_thread);
            let mut eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();

            ksync::lock!(
                let mut futex_guards = FutexState::lock_lock_multiple([
                    wake_futex_ref.state(),
                    requeue_futex_ref.state()
                ])
            );

            // Validate the futex storage state.
            let mut value: i32 = 0;
            // SAFETY: `value` is a valid stack `i32` and `wake_ptr` is a user pointer.
            let copy_result = unsafe {
                arch_copy_from_user_capture_faults(
                    (&raw mut value).cast(),
                    wake_ptr.as_ptr().cast(),
                    size_of::<i32>(),
                    CopyContext::BlockingNotAllowed,
                )
            };
            if let Err(copy_err) = copy_result {
                // At this point we are committed to either returning from the function, or
                // restarting the loop, so we can drop the locks and resched disable that are local
                // to this loop iteration.
                futex_guards.as_mut().release();
                requeue_owner_guard.release();
                eager_resched_disabler.enable();
                if let Some(fault) = copy_err.fault_info {
                    soft_fault(fault.pf_va, fault.pf_flags)?;
                    continue;
                }
                return Err(copy_err.status);
            }

            if value != current_value {
                return Err(Status::BAD_STATE);
            }

            // If owner validation failed earlier, then bail out now (after we have passed the state
            // check).
            if let Err(&status) = validator_result.as_ref() {
                if status == Status::BAD_HANDLE {
                    let _ =
                        ProcessDispatcher::get_current().enforce_basic_policy(ZX_POL_BAD_HANDLE);
                }
                return Err(status);
            }

            let mut new_requeue_owner: Option<ThreadPtr> = None;
            if let Some(owner_thread) = requeue_owner_thread {
                // Verify that the thread we are attempting to make the requeue target's
                // owner (if any) is not waiting on either the wake futex or the requeue
                // futex.
                // SAFETY: Both wake and requeue `FutexState` locks and `requeue_owner_guard` are
                // held.
                let owner_blocking_id =
                    FutexId::from_raw(unsafe { owner_thread.blocking_futex_id() });
                if owner_blocking_id == wake_id || owner_blocking_id == requeue_id {
                    return Err(Status::INVALID_ARGS);
                }

                // We need to resolve |requeue_owner_thread|'s Thread*.  First, we must revalidate
                // that |requeue_owner_thread| is not dead or dying as it may have changed state
                // during the |SoftFault| call above.
                //
                // Once we've verified it's not dead or dying, we must ensure the Thread* remains
                // valid.  See related comment below just after |new_owner_guard| is released.
                let owner_token = requeue_owner_guard
                    .token()
                    .expect("requeue_owner_guard must hold the requeue owner's dispatcher lock");
                // `requeue_owner_guard` holds `owner_thread`'s own dispatcher lock, so
                // `owner_token` proves the lock the lifecycle and `core_thread_` are guarded by.
                // Checking the lifecycle first also guarantees `core_thread_` is still set: it is
                // only cleared in the same critical section that moves the thread to
                // `Lifecycle::Dead`.
                if !owner_thread.is_dying_or_dead_locked(&owner_token) {
                    new_requeue_owner = Some(owner_thread.core_thread_locked(&owner_token));
                }
            }

            // Now that all of our sanity checks are complete, it is time to do the
            // actual manipulation of the various wait queues.
            {
                debug_assert!(!wake_futex_ref.is_null());

                // TODO(johngro): we are holding the proposed new owner's
                // ThreadDispatcher; do we need to, or is it possible to drop it?

                let mut wake_hook = WakeHook;
                let mut requeue_hook = RequeueHook {
                    wake_futex_ref: &wake_futex_ref,
                    requeue_futex_ref: &requeue_futex_ref,
                    new_id: requeue_id,
                };
                let wake_option = if owner_action == OwnerAction::Release {
                    WakeOption::None
                } else {
                    WakeOption::AssignOwner
                };

                // `new_requeue_owner` is kept valid by `requeue_owner_guard`. The locks held here
                // (the two futex state spinlocks in `futex_guards` and the owner's dispatcher lock
                // in `requeue_owner_guard`) are not chain locks, so no `ChainLockTransaction` is
                // active, as `wake_and_requeue` asserts. The two queues are distinct because
                // `wake_id != requeue_id` was checked above and each active futex ID maps to its
                // own `FutexState`. Neither hook re-enters a queue.
                // SAFETY: `new_requeue_owner` (if `Some`) is kept valid by
                // `requeue_owner_guard`.
                let new_requeue_owner_ref =
                    unsafe { new_requeue_owner.map(|owner| owner.as_ref()) };
                let wake_threads_result = wake_futex_ref.state().waiters.wake_and_requeue(
                    &requeue_futex_ref.state().waiters,
                    new_requeue_owner_ref,
                    wake_count,
                    requeue_count,
                    &mut wake_hook,
                    &mut requeue_hook,
                    wake_option,
                );

                // Now, if we successfully woke any threads from the wake_futex, then we need
                // to adjust the number of references we are holding by that number of
                // threads.  They are on the hot-path out of FutexWake, and we are responsible
                // for their pending op refs.
                wake_futex_ref.set_extra_refs(wake_threads_result.woken);
            }
            // If we got to here then we have no user copy faults that need retrying, so we should
            // break out of the infinite loop.
            break;
        }

        // Now just return.  The futex states will return to the pool as needed.
        Ok(())
    }

    /// Get the KOID of the current owner of the specified futex, if any, or ZX_KOID_INVALID if
    /// there is no known owner.
    pub fn futex_get_owner(
        &self,
        value_ptr: UserInPtr<zx_futex_t>,
        koid_out: UserOutPtr<zx_koid_t>,
    ) -> Result<(), Status> {
        // Make sure the futex pointer is following the basic rules.
        validate_futex_pointer(value_ptr)?;

        // Attempt to find the futex.  If it is not in the active set, then there is no owner.
        let mut koid: zx_koid_t = ZX_KOID_INVALID;
        let futex_id = FutexId::new(value_ptr);
        let futex_ref = self.find_active_futex(futex_id);

        // We found a FutexState in the active set.  It may have an owner, but we need
        // to enter the queue's lock in order to check.
        if !futex_ref.is_null() {
            let waiters = &futex_ref.state().waiters;
            single_chain_lock_guard_owned!(
                scope,
                waiters,
                IRQ_SAVE_OPTION,
                clt_tag!("FutexContext::FutexGetOwner")
            );
            koid = match scope.proof().queue_owner() {
                None => ZX_KOID_INVALID,
                Some(owner) => {
                    // Any thread which owns a FutexState's wait queue *must* be a
                    // user mode thread.
                    // SAFETY: `owner` is a valid `ThreadPtr` kept alive as the wait queue's owner
                    // while the queue's lock is held.
                    let user_thread = unsafe { ThreadDispatcher::from_core_thread(owner) };
                    debug_assert!(user_thread.is_some());
                    user_thread.map_or(ZX_KOID_INVALID, |user_thread| user_thread.get_koid())
                }
            };
        }

        koid_out.copy_to_user(&koid)
    }
}

/// Unit tests for `FutexContext`.
#[cfg(ktest)]
#[unittest::suite(name = "futex_context")]
mod tests {
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_err, assert_false, assert_ne, assert_ok, assert_true};

    /// Tests growing/shrinking the `FutexState` pool and `PendingOpRef` reference counting.
    #[test]
    fn pool_grow_shrink_and_pending_op_ref() {
        stack_pin_init!(let ctx = FutexContext::new());
        assert_ok!(ctx.as_mut().init(ProcessType::Regular));
        assert_ok!(ctx.grow_futex_state_pool());

        let id1 = FutexId::from_raw(0x1000);
        let id2 = FutexId::from_raw(0x2000);

        // Initially no active futexes.
        assert_true!(ctx.find_active_futex(id1).is_null());

        {
            let ref1 = ctx.activate_futex(id1);
            assert_false!(ref1.is_null());
            assert_eq!(ref1.state().id().get(), id1.get());

            let ref1_again = ctx.find_active_futex(id1);
            assert_false!(ref1_again.is_null());
            assert_true!(core::ptr::eq(ref1.state(), ref1_again.state()));

            let mut ref2 = ctx.activate_futex(id2);
            assert_false!(ref2.is_null());
            assert_eq!(ref2.state().id().get(), id2.get());

            // Transfer a ref from ref1_again to ref2 and cancel ref1_again.
            ref2.take_refs(&ref1_again, 1);
            let mut ref1_again = ref1_again;
            ref1_again.cancel_ref();
            ref2.set_extra_refs(1);
        }

        // After all PendingOpRefs drop, both states must have returned to the free pool.
        assert_true!(ctx.find_active_futex(id1).is_null());
        assert_true!(ctx.find_active_futex(id2).is_null());

        ctx.shrink_futex_state_pool();
    }

    /// Tests activating and releasing futexes whose IDs hash to the same bucket.
    #[test]
    fn colliding_futex_ids() {
        stack_pin_init!(let ctx = FutexContext::new());
        assert_ok!(ctx.as_mut().init(ProcessType::Regular));
        assert_ok!(ctx.grow_futex_state_pool());
        assert_ok!(ctx.grow_futex_state_pool());

        // A regular process has 37 buckets, so these IDs all hash to the same bucket.
        let ids = [
            FutexId::from_raw(0x1000),
            FutexId::from_raw(0x1000 + 37),
            FutexId::from_raw(0x1000 + 2 * 37),
        ];

        {
            let ref0 = ctx.activate_futex(ids[0]);
            let ref1 = ctx.activate_futex(ids[1]);
            let ref2 = ctx.activate_futex(ids[2]);
            assert_eq!(ref0.state().id().get(), ids[0].get());
            assert_eq!(ref1.state().id().get(), ids[1].get());
            assert_eq!(ref2.state().id().get(), ids[2].get());

            // Release the futex which was inserted in the middle of the bucket first.
            drop(ref1);
            assert_true!(ctx.find_active_futex(ids[1]).is_null());
            let found0 = ctx.find_active_futex(ids[0]);
            let found2 = ctx.find_active_futex(ids[2]);
            assert_true!(core::ptr::eq(found0.state(), ref0.state()));
            assert_true!(core::ptr::eq(found2.state(), ref2.state()));
        }

        for id in ids {
            assert_true!(ctx.find_active_futex(id).is_null());
        }

        ctx.shrink_futex_state_pool();
        ctx.shrink_futex_state_pool();
    }

    /// Tests `validate_futex_pointer` alignment and null checks.
    #[test]
    fn validate_futex_pointer_checks() {
        assert_err!(
            validate_futex_pointer(UserInPtr::new(core::ptr::null())),
            Status::INVALID_ARGS
        );
        for offset in 1..size_of::<zx_futex_t>() {
            let unaligned = core::ptr::with_exposed_provenance::<zx_futex_t>(0x1000usize + offset);
            assert_err!(validate_futex_pointer(UserInPtr::new(unaligned)), Status::INVALID_ARGS);
        }
        #[cfg(target_arch = "aarch64")]
        {
            let tagged_null =
                core::ptr::with_exposed_provenance::<zx_futex_t>((0xffu64 << 56) as usize);
            assert_err!(validate_futex_pointer(UserInPtr::new(tagged_null)), Status::INVALID_ARGS);
        }
        assert_ok!(validate_futex_pointer(UserInPtr::new(core::ptr::with_exposed_provenance::<
            zx_futex_t,
        >(0x1000usize))));
        assert_ok!(validate_futex_pointer(UserInPtr::new(core::ptr::with_exposed_provenance::<
            zx_futex_t,
        >(0x1004usize))));
    }

    /// Tests `FutexId::new` right-shift and `aarch64` TBI detagging.
    #[test]
    fn futex_id_shift_and_detag() {
        assert_eq!(FutexId::null().get(), 0);
        let addr: usize = 0x1234_5678;
        let id =
            FutexId::new(UserInPtr::new(core::ptr::with_exposed_provenance::<zx_futex_t>(addr)));
        assert_eq!(id.get(), addr >> 2);
        assert_eq!(FutexId::get_hash(id), addr >> 2);
        assert_ne!(id.get(), FutexId::null().get());

        #[cfg(target_arch = "aarch64")]
        {
            let tagged_addr: usize = ((0xabu64 << 56) as usize) | addr;
            let tagged_id = FutexId::new(UserInPtr::new(core::ptr::with_exposed_provenance::<
                zx_futex_t,
            >(tagged_addr)));
            assert_eq!(tagged_id.get(), id.get());
            assert_eq!(tagged_id.get(), addr >> 2);
        }
    }
}
