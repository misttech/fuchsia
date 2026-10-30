// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::kconcurrent::{
    Action, ChainLock, ChainLockProof, ChainLockResult, ChainLockTransaction,
    ChainLockTransactionToken, ChainLocked, ClassedChainLock, DONE,
    EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION, IRQ_SAVE_OPTION, OwnerGuard,
    PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, TransactionResult, acquire_both_or_backoff,
    assert_no_transaction_active, assert_transaction_active, clt_tag, debug_assert_lock_not_held,
    debug_assert_no_transaction_active,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::scheduler::{
    self, BaseProfile, EffectiveProfile, InheritedProfileValues, PiNode, PiNodeRef,
    SchedDiscipline, SchedDuration, SchedUtilization, SchedWeight,
    WaitQueueInheritedSchedulerState,
};
use crate::kernel::scheduler_state::ThreadStateKind;
use crate::kernel::thread::{PreemptDisabledToken, Thread, ThreadPtr, UnblockList};
use crate::kernel::wait_queue::{
    ForceInheritance, Interruptible, ResourceOwnership, WaitQueueBase, WaitQueueCollection,
    WaitQueueLockClass, assert_in_wait_queue,
};
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};
use core::ops::Deref;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode};
use ksync::KCell;
use owned_wait_queue_bindings as bindings;
use pin_init::{PinInit, pin_data, pinned_drop};
use zr::static_assert;
use zx_status::Status;
use zx_types::{ZX_ERR_INTERNAL_INTR_KILLED, zx_status_t};

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

// `ChainLockResult` crosses the FFI boundary as the `int32_t` value of the C++
// `ChainLock::Result` enumerator, which is checked by matching `static_assert`s in
// `owned_wait_queue.h`.
static_assert!(ChainLockResult::Ok as i32 == 0);
static_assert!(ChainLockResult::Retry as i32 == 1);
static_assert!(ChainLockResult::Backoff as i32 == 2);
static_assert!(ChainLockResult::Cycle as i32 == 3);

static_assert!(
    size_of::<EffectiveProfile>() == size_of::<bindings::SchedulerState_EffectiveProfile>()
);
static_assert!(
    align_of::<EffectiveProfile>() == align_of::<bindings::SchedulerState_EffectiveProfile>()
);

// `Thread::owned_wait_queues_` is a `fbl::DoublyLinkedList<OwnedWaitQueue*>`, which consists of a
// single head pointer.
static_assert!(size_of::<DoublyLinkedList<*mut OwnedWaitQueue>>() == size_of::<*mut c_void>());
static_assert!(align_of::<DoublyLinkedList<*mut OwnedWaitQueue>>() == align_of::<*mut c_void>());

// Notes on the defined kernel counters.
//
// Adjustments (aka promotions and demotions)
// The number of times that a thread either gained or lost inherited profile
// pressure as a result of a PI event.
//
// Note that the number of promotions does not have to equal the number of
// demotions in the system.  For example, a thread could slowly gain weight as
// fair scheduled threads join a wait queue it owns, then suddenly drop
// back down to its base profile when the thread releases ownership of the
// queue.
//
// In addition to simple promotions and demotions, the number of threads whose
// effective profile changed as a result of another thread's base profile
// changing is also tracked, although whether these changes amount to a
// promotion or a demotion is not computed.
//
// Max chain traversal.
// The maximum traversed length of a PI chain during execution of the propagation
// algorithm.
//
// The length of a propagation chain is defined as the number of nodes in an
// inheritance graph which are affected by a propagation event.  For example, if
// a thread (T1) blocks in an owned wait queue (OWQ1), adding an edge between
// them, and the wait queue has no owner, then the propagation event's chain
// length is 1. This is regardless of whether or not the blocking thread
// currently owns one or more wait queues upstream from it. The OWQ1's IPVs were
// updated, but no other nodes in the graph were affected.  If the OWQ1 had been
// owned by a running/runnable thread (T2), then the chain length of the
// operation would have been two instead, since both OWQ1 and T2 needed to be
// visited and updated.
define_kcounter!(PI_PROMOTIONS, "kernel.pi.adj.promotions", Sum);
define_kcounter!(PI_DEMOTIONS, "kernel.pi.adj.demotions", Sum);
define_kcounter!(PI_BP_CHANGED, "kernel.pi.adj.bp_changed", Sum);
define_kcounter!(MAX_PI_CHAIN_TRAVERSE, "kernel.pi.max_chain_traverse", Max);

/// Option bits for a [`ChainLengthTracker`].
struct ChainLengthTrackerOpt;

impl ChainLengthTrackerOpt {
    const NONE: u32 = 0;
    const RECORD_MAX_LENGTH: u32 = 1;
    const ENFORCE_LENGTH_GUARD: u32 = 2;
}

// We enforce the length guard in everything but release builds. Note that the max length counter
// is only maintained when the length guard is enforced, since `ChainLengthTracker`'s `Drop` gates
// it on `ENFORCE_LENGTH_GUARD` rather than on `RECORD_MAX_LENGTH`.
const ENABLE_PI_CHAIN_GUARDS: u32 = if !cfg!(lk_debuglevel = "0") {
    ChainLengthTrackerOpt::ENFORCE_LENGTH_GUARD
} else {
    ChainLengthTrackerOpt::NONE
};

const DEFAULT_CHAIN_LENGTH_TRACKER_OPT: u32 =
    ChainLengthTrackerOpt::RECORD_MAX_LENGTH | ENABLE_PI_CHAIN_GUARDS;

/// Tracks the number of nodes visited during a PI propagation operation, maintaining the
/// `kernel.pi.max_chain_traverse` counter and (optionally) enforcing a maximum chain length.
struct ChainLengthTracker<const OPTIONS: u32 = DEFAULT_CHAIN_LENGTH_TRACKER_OPT> {
    nodes_visited: u32,
}

#[cold]
#[inline(never)]
fn panic_chain_length_exceeded(visited: u32) -> ! {
    panic!("visited {}", visited);
}

impl<const OPTIONS: u32> ChainLengthTracker<OPTIONS> {
    #[inline(always)]
    fn new() -> Self {
        Self { nodes_visited: 0 }
    }

    #[inline(always)]
    fn node_visited(&mut self) {
        if OPTIONS != ChainLengthTrackerOpt::NONE {
            self.nodes_visited = self.nodes_visited.wrapping_add(1);
        }

        if (OPTIONS & ChainLengthTrackerOpt::ENFORCE_LENGTH_GUARD) != ChainLengthTrackerOpt::NONE {
            const MAX_CHAIN_LEN: u32 = 2048;
            if self.nodes_visited > MAX_CHAIN_LEN {
                panic_chain_length_exceeded(self.nodes_visited);
            }
        }
    }
}

impl<const OPTIONS: u32> Drop for ChainLengthTracker<OPTIONS> {
    #[inline(always)]
    fn drop(&mut self) {
        if (OPTIONS & ChainLengthTrackerOpt::ENFORCE_LENGTH_GUARD) != ChainLengthTrackerOpt::NONE {
            // Note, the only real reason that this is an accurate max at all is
            // because the counter is effectively protected by the thread lock
            // (although there is no real good way to annotate that fact).  When we
            // finally remove the thread lock, we are going to need to do better than
            // this.
            MAX_PI_CHAIN_TRAVERSE.max(i64::from(self.nodes_visited));
        }
    }
}

/// A enum which determines the specific behavior of the propagate method.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PropagateOp {
    /// Add a single edge from the upstream node to the downstream node, and
    /// propagate the effects. This happens either when a thread blocks in an
    /// OWQ, or when an owner is assigned to an OWQ.
    AddSingleEdge,

    /// Remove a single edge from the upstream node to the downstream node. This
    /// happens either when a thread unblocks from an OWQ, or when an OWQ becomes
    /// unowned.
    RemoveSingleEdge,

    /// The base profile of an upstream node has changed, causing a change to the
    /// profile pressure without an explicit join or split operation.
    BaseProfileChanged,
}

/// A compile-time tag selecting a [`PropagateOp`], and the matching `propagate` operation.
pub(crate) trait PropagateOpTag: Copy {
    /// The operation selected by this tag.
    const OP: PropagateOp;

    /// Informs the scheduler of the PI change between `upstream` and `downstream`, and updates
    /// the PI kcounters.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the locks of `upstream`, `downstream`, and every node in the
    /// PI chain downstream of `downstream` are held by the active transaction.
    unsafe fn propagate<U: PiNode, D: PiNode>(
        upstream: &U,
        downstream: &D,
        force_inheritance: ForceInheritance,
        old_target_ep: Option<&EffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    );
}

/// Tag for [`PropagateOp::AddSingleEdge`].
#[derive(Debug, Clone, Copy)]
struct AddSingleEdgeTag;

/// Tag for [`PropagateOp::RemoveSingleEdge`].
#[derive(Debug, Clone, Copy)]
pub(crate) struct RemoveSingleEdgeTag;

/// Tag for [`PropagateOp::BaseProfileChanged`].
#[derive(Debug, Clone, Copy)]
struct BaseProfileChangedTag;

const ADD_SINGLE_EDGE_OP: AddSingleEdgeTag = AddSingleEdgeTag;
pub(crate) const REMOVE_SINGLE_EDGE_OP: RemoveSingleEdgeTag = RemoveSingleEdgeTag;
const BASE_PROFILE_CHANGED_OP: BaseProfileChangedTag = BaseProfileChangedTag;

impl PropagateOpTag for AddSingleEdgeTag {
    const OP: PropagateOp = PropagateOp::AddSingleEdge;

    /// # Safety
    ///
    /// See [`PropagateOpTag::propagate`].
    #[inline]
    unsafe fn propagate<U: PiNode, D: PiNode>(
        upstream: &U,
        downstream: &D,
        force_inheritance: ForceInheritance,
        _old_target_ep: Option<&EffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        preempt_disabled_token.assert_held();
        // SAFETY: Forwarded from the caller.
        unsafe {
            scheduler::join_node_to_pi_graph(upstream, downstream, force_inheritance, clt_token);
        }
        if D::IS_THREAD {
            PI_PROMOTIONS.add(1);
        }
    }
}

impl PropagateOpTag for RemoveSingleEdgeTag {
    const OP: PropagateOp = PropagateOp::RemoveSingleEdge;

    /// # Safety
    ///
    /// See [`PropagateOpTag::propagate`].
    #[inline]
    unsafe fn propagate<U: PiNode, D: PiNode>(
        upstream: &U,
        downstream: &D,
        _force_inheritance: ForceInheritance,
        old_target_ep: Option<&EffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        preempt_disabled_token.assert_held();
        // SAFETY: Forwarded from the caller.
        unsafe {
            scheduler::split_node_from_pi_graph(upstream, downstream, old_target_ep, clt_token);
        }
        if D::IS_THREAD {
            PI_DEMOTIONS.add(1);
        }
    }
}

impl PropagateOpTag for BaseProfileChangedTag {
    const OP: PropagateOp = PropagateOp::BaseProfileChanged;

    /// # Safety
    ///
    /// See [`PropagateOpTag::propagate`].
    #[inline]
    unsafe fn propagate<U: PiNode, D: PiNode>(
        upstream: &U,
        downstream: &D,
        _force_inheritance: ForceInheritance,
        _old_target_ep: Option<&EffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        const {
            assert!(
                U::IS_THREAD,
                "Base profile propagation changes may only start from Threads, not OwnedWaitQueues"
            )
        };
        preempt_disabled_token.assert_held();
        let PiNodeRef::Thread(upstream) = upstream.pi_node_ref() else {
            unreachable!();
        };
        // SAFETY: Forwarded from the caller.
        unsafe {
            scheduler::upstream_thread_base_profile_changed(upstream, downstream, clt_token);
        }
        if D::IS_THREAD {
            PI_BP_CHANGED.add(1);
        }
    }
}

#[inline]
fn ipvs_are_consequential(ipvs: Option<&InheritedProfileValues>) -> bool {
    ipvs.is_some_and(InheritedProfileValues::is_consequential)
}

/// Behavior control for locking pi paths.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LockingBehavior {
    RefuseCycle,
    StopAtCycle,
}

/// A compile-time tag selecting a [`LockingBehavior`].
trait LockingBehaviorTag {
    const BEHAVIOR: LockingBehavior;
}

/// Tag for [`LockingBehavior::RefuseCycle`].
struct RefuseCycle;

impl LockingBehaviorTag for RefuseCycle {
    const BEHAVIOR: LockingBehavior = LockingBehavior::RefuseCycle;
}

/// Tag for [`LockingBehavior::StopAtCycle`].
struct StopAtCycle;

impl LockingBehaviorTag for StopAtCycle {
    const BEHAVIOR: LockingBehavior = LockingBehavior::StopAtCycle;
}

/// A reference to the node at which a PI chain locking or unlocking operation starts.
#[derive(Clone, Copy)]
pub(crate) enum PiChainNodeRef<'a> {
    /// A [`Thread`] node.
    Thread(&'a Thread),
    /// A wait queue node. A raw pointer is used so that, for an [`OwnedWaitQueue`], it can carry
    /// provenance over the enclosing object (see [`OwnedWaitQueue::downcast_ptr`]).
    WaitQueue(NonNull<WaitQueueBase>),
}

impl PiChainNodeRef<'_> {
    /// Returns the address of the node, as an opaque PI chain stop point.
    #[inline]
    fn as_ptr(self) -> *const c_void {
        match self {
            Self::Thread(t) => core::ptr::from_ref(t).cast(),
            Self::WaitQueue(wq) => wq.as_ptr().cast_const().cast(),
        }
    }

    /// Returns `true` if the node's lock is held by the transaction `token` belongs to.
    ///
    /// # Safety
    ///
    /// The node must be live.
    #[inline]
    unsafe fn is_held(self, token: &ChainLockTransactionToken) -> bool {
        match self {
            Self::Thread(t) => t.lock().is_held(token),
            // SAFETY: The caller guarantees the node is live.
            Self::WaitQueue(wq) => unsafe { wq.as_ref() }.lock().is_held(token),
        }
    }
}

/// Implemented by the types at which a PI chain locking or unlocking operation may start.
///
/// # Safety
///
/// [`PiChainNode::pi_chain_node_ref`] must return a reference to `self`, whose lock is the one
/// returned by `self.lock()`; unsafe code relies on this to operate on the nodes whose locks its
/// callers hold.
pub(crate) unsafe trait PiChainNode: ChainLocked {
    /// Returns `self` as a [`PiChainNodeRef`].
    fn pi_chain_node_ref(&self) -> PiChainNodeRef<'_>;
}

// SAFETY: `pi_chain_node_ref` returns `self` as a `PiChainNodeRef::Thread`.
unsafe impl PiChainNode for Thread {
    #[inline]
    fn pi_chain_node_ref(&self) -> PiChainNodeRef<'_> {
        PiChainNodeRef::Thread(self)
    }
}

// SAFETY: `pi_chain_node_ref` returns a pointer to `self` as a `PiChainNodeRef::WaitQueue`.
unsafe impl PiChainNode for WaitQueueBase {
    #[inline]
    fn pi_chain_node_ref(&self) -> PiChainNodeRef<'_> {
        PiChainNodeRef::WaitQueue(NonNull::from(self))
    }
}

/// Returns the raw [`ChainLock`] guarding `node`, for locking operations which cannot be expressed
/// with [`OwnerGuard`]s (such as walking and unwinding PI chains).
///
/// # Safety
///
/// The caller must not use the returned lock to release `node`'s lock while an [`OwnerGuard`] or
/// [`ChainLockProof`] for `node` is live (see [`ClassedChainLock::as_chain_lock`]).
#[inline]
unsafe fn raw_lock<O: ChainLocked + ?Sized>(node: &O) -> &ChainLock {
    let lock: &ClassedChainLock<O::Class> = node.lock();
    // SAFETY: Forwarded from the caller.
    unsafe { lock.as_chain_lock() }
}

/// Mints a proof that `node`'s lock is held, for a lock which is held by the current execution
/// context but is not tracked by an [`OwnerGuard`] (such as the locks of nodes in a PI chain).
///
/// # Safety
///
/// `node`'s lock must be held by the active transaction for the whole of `'a`, and while the
/// returned proof is live, no other proof or guard for `node` may be used to access the state
/// protected by `node`'s lock (see [`ChainLockProof::assume_held_unchecked`]).
#[inline]
unsafe fn assume_locked<'a, O: ChainLocked>(node: &'a O) -> ChainLockProof<'a, O> {
    // SAFETY: Forwarded from the caller.
    unsafe { ChainLockProof::assume_held_unchecked(node) }
}

/// Mints a read-only proof for `node` whose lock is *not* necessarily held, for read access to
/// fields which another held lock (or quiescence) keeps from changing.
///
/// This is used to read state which is guarded by `node`'s lock, but which cannot change because
/// of some other synchronization (for example, a blocked thread's wait queue state cannot change
/// while the thread remains in a wait queue whose lock is held).
///
/// # Safety
///
/// See [`ChainLockProof::assume_synchronized_read_only`].
#[inline]
unsafe fn assume_synchronized_read_only<'a, O: ChainLocked>(node: &'a O) -> ChainLockProof<'a, O> {
    // SAFETY: Forwarded from the caller.
    unsafe { ChainLockProof::assume_synchronized_read_only(node) }
}

/// Returns `true` if `a` and `b` refer to the same [`Thread`] (or are both `None`).
#[inline]
fn same_thread(a: Option<ThreadPtr>, b: Option<&Thread>) -> bool {
    a == b.map(ThreadPtr::from)
}

/// Returns the address of `t` (or null) as an opaque PI chain stop point.
#[inline]
fn thread_stop_point(t: Option<ThreadPtr>) -> *const c_void {
    t.map_or(core::ptr::null(), |t| t.as_ptr().cast())
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
/// Both [`IWakeRequeueHook::allow`] and [`IWakeRequeueHook::on_wake_or_requeue`] are invoked with
/// interrupts disabled inside an active
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
    #[inline]
    fn allow(&mut self, _t: &Thread) -> bool {
        true
    }

    /// `on_wake_or_requeue` is called in the second phase of a wake/requeue operation. At this
    /// point, all of the threads to wake have been identified and successfully locked. This hook
    /// is called _just_ before the thread is removed from its queue, and either unblocked (wake
    /// operation) or moved to a different queue (requeue).
    #[inline]
    fn on_wake_or_requeue(&mut self, _t: &mut ChainLockProof<'_, Thread>) {}
}

/// Default no-op [`IWakeRequeueHook`] implementation.
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct DefaultWakeHooks;

impl IWakeRequeueHook for DefaultWakeHooks {}

/// Adapts a C++ `OwnedWaitQueue::IWakeRequeueHook` for use as an [`IWakeRequeueHook`].
struct CppWakeRequeueHook {
    // Safety invariant: Points to a live C++ `OwnedWaitQueue::IWakeRequeueHook` for the lifetime
    // of `self`.
    hook: NonNull<bindings::OwnedWaitQueue_IWakeRequeueHook>,
}

impl CppWakeRequeueHook {
    /// Wraps the C++ `OwnedWaitQueue::IWakeRequeueHook` pointed to by `hook`.
    ///
    /// # Safety
    ///
    /// `hook` must point to a live C++ `OwnedWaitQueue::IWakeRequeueHook` which outlives the
    /// returned value.
    #[inline]
    unsafe fn new(hook: *mut bindings::OwnedWaitQueue_IWakeRequeueHook) -> Self {
        debug_assert!(!hook.is_null());
        // SAFETY: The caller guarantees `hook` points to a live hook, so it is non-null.
        Self { hook: unsafe { NonNull::new_unchecked(hook) } }
    }
}

impl IWakeRequeueHook for CppWakeRequeueHook {
    #[inline]
    fn allow(&mut self, t: &Thread) -> bool {
        // SAFETY: `self.hook` points to a live C++ hook (type invariant), and `allow` is only
        // called while `t` is a member of a wait queue whose lock is held, providing read-only
        // access to `t`.
        unsafe {
            bindings::cpp_owned_wait_queue_wake_requeue_hook_allow(
                self.hook.as_ptr(),
                t.as_raw().cast(),
            )
        }
    }

    #[inline]
    fn on_wake_or_requeue(&mut self, t: &mut ChainLockProof<'_, Thread>) {
        // SAFETY: `self.hook` points to a live C++ hook (type invariant), and `t` proves that
        // `t.owner().lock()` is held.
        unsafe {
            bindings::cpp_owned_wait_queue_wake_requeue_hook_on_wake_or_requeue(
                self.hook.as_ptr(),
                t.owner().as_raw().cast(),
            );
        }
    }
}

/// A small struct which contains details about the locking used in order to prepare for a
/// [`OwnedWaitQueue::block_and_assign_owner`] operation. See
/// [`OwnedWaitQueue::lock_for_baao_operation_or_backoff`] for details.
#[repr(C)]
#[must_use]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct BAAOLockingDetails {
    /// The validated new owner (if any) of the queue.
    new_owner: Option<ThreadPtr>,
    /// Opaque identity of the PI-chain node at which unlocking stops; compared by address only
    /// and never dereferenced.
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

/// Alias for [`BAAOLockingDetails`], used when locking for an owner replacement.
type ReplaceOwnerLockingDetails = BAAOLockingDetails;

/// The lists of locked threads selected for waking and requeueing during a
/// [`OwnedWaitQueue::wake_and_requeue`] operation.
#[pin_data]
struct WakeRequeueThreadDetails {
    #[pin]
    wake_threads: ThreadUnblockList,
    #[pin]
    requeue_threads: ThreadUnblockList,
}

impl WakeRequeueThreadDetails {
    #[inline]
    fn init() -> impl PinInit<Self, Infallible> {
        pin_init::pin_init!(Self {
            wake_threads <- ThreadUnblockList::init(),
            requeue_threads <- ThreadUnblockList::init(),
        })
    }
}

/// Details of the locking performed in order to prepare for a
/// [`OwnedWaitQueue::wake_and_requeue`] operation.
#[pin_data]
struct RequeueLockingDetails {
    /// The validated new owner (if any) of the requeue target.
    new_requeue_target_owner: Option<ThreadPtr>,
    /// Opaque identity of the node at which unlocking the new requeue target owner's PI chain
    /// stops; compared by address only and never dereferenced.
    new_requeue_target_owner_stop_point: *const c_void,
    /// Opaque identity of the node at which unlocking the wake queue's old owner's PI chain
    /// stops; compared by address only and never dereferenced.
    old_wake_queue_owner_stop_point: *const c_void,
    /// Opaque identity of the node at which unlocking the requeue target's old owner's PI chain
    /// stops; compared by address only and never dereferenced.
    old_requeue_target_owner_stop_point: *const c_void,
    #[pin]
    threads: WakeRequeueThreadDetails,
}

impl RequeueLockingDetails {
    #[inline]
    fn init() -> impl PinInit<Self, Infallible> {
        pin_init::pin_init!(Self {
            new_requeue_target_owner: None,
            new_requeue_target_owner_stop_point: core::ptr::null(),
            old_wake_queue_owner_stop_point: core::ptr::null(),
            old_requeue_target_owner_stop_point: core::ptr::null(),
            threads <- WakeRequeueThreadDetails::init(),
        })
    }
}

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

/// Pinned opaque wrapper around C++ `Thread::UnblockList`.
///
/// C++ `Thread::UnblockList` (`fbl::DoublyLinkedListCustomTraits<Thread*, ...>`) is
/// self-referential: its end sentinel encodes the address of the list object itself
/// (`(uintptr_t)this | 1`). Moving it by value in Rust (`memcpy`) without running the C++ move
/// constructor would invalidate its sentinel. Consequently, `ThreadUnblockList` is pinned in place
/// via [`ThreadUnblockList::init`].
///
/// `raw` is structurally pinned so that `ThreadUnblockList` is `!Unpin`. Otherwise
/// `&mut ThreadUnblockList` would be `noalias`, and LLVM could assume that the C++ list's head is
/// not modified through pointers recovered from its (self-referential) sentinel, caching stale
/// list state across FFI calls that inline the C++ list operations.
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct ThreadUnblockList {
    #[pin]
    raw: zr::Opaque<bindings::Thread_UnblockList>,
}

static_assert!(size_of::<ThreadUnblockList>() == size_of::<bindings::Thread_UnblockList>());
static_assert!(align_of::<ThreadUnblockList>() == align_of::<bindings::Thread_UnblockList>());
// `Thread::UnblockList` and the Rust `UnblockList` are two views of one list: both are a single
// sentinel-encoded head pointer over the same `unblock_list_node` of `Thread` (whose offset is
// asserted in `thread.rs`), which is what lets `as_list` reinterpret the storage.
static_assert!(size_of::<UnblockList>() == size_of::<bindings::Thread_UnblockList>());
static_assert!(align_of::<UnblockList>() == align_of::<bindings::Thread_UnblockList>());

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

    /// Views the list as the Rust [`UnblockList`] over the same storage.
    #[inline]
    pub fn as_list(self: Pin<&mut Self>) -> Pin<&mut UnblockList> {
        // SAFETY: `Self::init` constructed a valid `Thread::UnblockList` in `raw`, which shares
        // its layout and sentinel encoding with `UnblockList` (asserted above); the projection
        // keeps the list pinned and the `&mut` borrow of `self` makes it unique.
        unsafe { self.map_unchecked_mut(|list| &mut *list.raw.get().cast::<UnblockList>()) }
    }

    /// Views the list as the Rust [`UnblockList`] over the same storage, shared.
    #[inline]
    pub fn as_list_ref(&self) -> &UnblockList {
        // SAFETY: As in `as_list`; only shared access is handed out.
        unsafe { &*self.raw.get().cast::<UnblockList>() }
    }

    /// Returns whether the list is empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.as_list_ref().is_empty()
    }

    /// Adds `thread` to the back of the list.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `thread` is not currently a member of any unblock list, and
    /// that it remains alive while it is a member of this list.
    #[inline]
    pub unsafe fn push_back(self: Pin<&mut Self>, thread: &Thread) {
        // SAFETY: Forwarded from the caller.
        unsafe { self.as_list().push_back_raw(NonNull::from(thread)) }
    }

    /// Removes and returns the [`Thread`] at the front of the list, if any.
    #[inline]
    pub fn pop_front(self: Pin<&mut Self>) -> Option<ThreadPtr> {
        // SAFETY: Every member of the list is a live `Thread`.
        self.as_list().pop_front().and_then(|p| unsafe { ThreadPtr::from_raw(p.as_ptr()) })
    }

    /// Returns an iterator over the [`Thread`]s in the list.
    #[inline]
    pub fn iter(&self) -> impl Iterator<Item = ThreadPtr> + '_ {
        self.as_list_ref().iter().map(ThreadPtr::from)
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
///
/// Every byte of an `OwnedWaitQueue` is interior-mutable (`UnsafeCell`, `KCell`, or the
/// `!Freeze` `WaitQueueBase`), which `&OwnedWaitQueue` references held across the release of the
/// queue's own lock rely on (as with `WaitQueueBase::magic`). Do not add a `Freeze` field.
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
    owner: KCell<Option<ThreadPtr>, WaitQueueLockClass>,
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
    ///
    /// TODO(johngro): An alternative approach would be to copy the token-ante
    /// system used by Theads and FutexContext.
    ///
    /// Pros: Easier to reason about code and reduced risk of accidental
    ///       use-after-free.  Also, no need to copy bookkeeping from one thread to
    ///       another when the currently active thread leaves the OwnedWaitQueue.
    /// Cons: Storage of the free tokens would require some central pool system
    ///       which would act as a central choke point, which might become a
    ///       scalability issue on systems where the processor count is high.
    // Safety invariant: `None` when `base`'s collection is empty, or points to the
    // `inherited_scheduler_state_storage` of a `Thread` currently blocked in `base`'s collection,
    // whenever `base`'s lock is not held. While the lock is held, it may transiently point to the
    // storage of a thread which was just removed from the collection: between that thread's
    // removal and the matching `update_sched_state_storage_thread_removed` call, and while wake
    // and requeue locking operations have temporarily removed locked waiters from the collection
    // (they return the waiters before releasing the lock). Accessed only through
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

// SAFETY: `OwnedWaitQueue` has no thread-local ownership invariants: its `ThreadPtr` and `NonNull`
// fields refer to kernel-global objects which are not tied to the creating CPU or thread, so
// ownership of a pinned `OwnedWaitQueue` can be transferred across threads.
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
        // Note: under Stacked Borrows, the returned reference only has provenance over the
        // `WaitQueueBase` bytes, however it is derived, so downcasting it back to an
        // `OwnedWaitQueue` relies on Tree Borrows-style `container_of` semantics. Code which needs
        // to downcast should keep a pointer derived from the `OwnedWaitQueue` (see
        // `OwnedWaitQueue::downcast_ptr`).
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
        // SAFETY: `drop` has exclusive teardown access to `self`.
        unsafe {
            self.as_ref().get_ref().teardown();
        }
    }
}

/// Accessors and locked operations for an [`OwnedWaitQueue`] guarded by its lock.
impl ChainLockProof<'_, OwnedWaitQueue> {
    /// Re-expresses this proof as one for the embedded [`WaitQueueBase`] (same lock instance).
    /// `self` is unusable while the returned proof lives.
    #[inline(always)]
    pub(crate) fn as_base(&mut self) -> ChainLockProof<'_, WaitQueueBase> {
        let base: &WaitQueueBase = self.owner();
        // SAFETY: `self` proves `self.owner().lock()` (which is `base.lock()`) is held, and the
        // unique borrow of `self` prevents concurrent use of `self` while the returned proof lives.
        unsafe { ChainLockProof::assume_held_unchecked(base) }
    }

    /// Returns the collection of blocked threads.
    #[inline(always)]
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
    /// `base.collection` and `inherited_scheduler_state_storage` (including restoring them
    /// before this queue's lock is released, after the transient states which that invariant
    /// permits).
    #[inline(always)]
    pub(crate) unsafe fn collection_mut(&mut self) -> &mut WaitQueueCollection {
        let (queue, token) = self.split_mut();
        // SAFETY: As in `collection`; the unique borrow of `self` ensures no other reference to
        // `base.collection` is live.
        unsafe { queue.base.collection.get_mut(token) }
    }

    /// Returns the current owner of this queue, if any.
    #[inline(always)]
    pub(crate) fn owner_thread(&self) -> Option<ThreadPtr> {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards `owner`.
        *unsafe { self.owner().owner.get(self.token()) }
    }

    /// Returns the current owner of this queue mutably.
    ///
    /// # Safety
    ///
    /// The caller must maintain the [`OwnedWaitQueue`] safety invariants for `owner` and
    /// `dll_node`:
    /// - Any owner stored must point to a live [`Thread`] whose `owned_wait_queues_` list
    ///   contains this queue.
    /// - `owner` must be `None` when the queue is dropped.
    #[inline(always)]
    pub(crate) unsafe fn owner_thread_mut(&mut self) -> &mut Option<ThreadPtr> {
        let (queue, token) = self.split_mut();
        // SAFETY: As in `owner_thread`; the unique borrow of `self` ensures no other reference to
        // `owner` is live.
        unsafe { queue.owner.get_mut(token) }
    }

    /// Returns the location of the inherited scheduler state storage, if any.
    #[inline(always)]
    pub(crate) fn inherited_scheduler_state_storage(
        &self,
    ) -> Option<NonNull<WaitQueueInheritedSchedulerState>> {
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `inherited_scheduler_state_storage`.
        *unsafe { self.owner().inherited_scheduler_state_storage.get(self.token()) }
    }

    /// Returns the location of the inherited scheduler state storage mutably.
    ///
    /// # Safety
    ///
    /// The caller must maintain the [`OwnedWaitQueue`] safety invariant for
    /// `inherited_scheduler_state_storage` (`None` when `base.collection` is empty, or pointing to
    /// the `inherited_scheduler_state_storage` of a [`Thread`] currently blocked in
    /// `base.collection`, apart from the transient states which that invariant permits while this
    /// queue's lock is held).
    #[inline(always)]
    pub(crate) unsafe fn inherited_scheduler_state_storage_mut(
        &mut self,
    ) -> &mut Option<NonNull<WaitQueueInheritedSchedulerState>> {
        let (queue, token) = self.split_mut();
        // SAFETY: `self` proves `owner().lock()` is held, and that lock guards
        // `inherited_scheduler_state_storage`; the unique borrow of `self` ensures no other
        // reference to it is live.
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

    /// Accessor for the queue's `owner` [`ThreadPtr`].
    #[inline]
    pub fn queue_owner(&self) -> Option<ThreadPtr> {
        self.owner_thread()
    }

    /// Returns `true` if this wait queue currently has inherited deadline utilization pressure.
    #[inline]
    pub(crate) fn has_deadline_pressure(&self) -> bool {
        self.inherited_scheduler_state_storage().is_some_and(|iss_ptr| {
            // SAFETY: By the `inherited_scheduler_state_storage` safety invariant, `iss_ptr`
            // points to the storage of a thread blocked in this queue, which is guarded by this
            // queue's lock (held per `self`) and is not mutated while `self` is borrowed.
            unsafe { (*iss_ptr.as_ptr()).ipvs.uncapped_utilization > SchedUtilization::from_raw(0) }
        })
    }

    /// Gets the effective profile of this `OwnedWaitQueue`.
    ///
    /// The effective profile of an `OwnedWaitQueue` is the composition of the effective profiles
    /// of all the threads currently waiting in that queue. As such, the effective profile may
    /// change as threads are added to or removed from the queue or when the effective profile of
    /// any thread in the queue changes, either due to base profile changes or transitive
    /// propagation of upstream profile changes from owned wait queues that they own.
    #[inline]
    pub fn get_effective_profile(&self) -> EffectiveProfile {
        match self.inherited_scheduler_state_storage() {
            Some(iss_ptr) => {
                // SAFETY: By the `inherited_scheduler_state_storage` safety invariant, `iss_ptr`
                // points to the storage of a thread blocked in this queue, which is guarded by
                // this queue's lock (held per `self`) and is not mutated while `self` is
                // borrowed.
                unsafe { iss_ptr.as_ref() }.get_effective_profile()
            }
            None => EffectiveProfile::default(),
        }
    }

    /// Validates (unconditionally) that the inherited scheduler state storage location refers to
    /// the storage of a thread which is currently blocked in this queue.
    fn validate_sched_state_storage_unconditional(&mut self) {
        if let Some(iss) = self.inherited_scheduler_state_storage() {
            let mut found = false;
            let base = self.as_base();
            for t in base.collection().threads() {
                // SAFETY: `t`'s lock is not held. `t` is blocked in this queue, and this queue's
                // lock (proven held by `self`) is held for the whole loop. The wait queue state
                // of a thread blocked in a queue (including its blocking queue and inherited
                // scheduler state storage) cannot change without also holding that queue's lock
                // (see `assert_in_wait_queue`). `t_proof` is only used for those reads.
                let t_proof = unsafe { assume_synchronized_read_only(t.as_ref()) };
                assert_in_wait_queue(&t_proof, &base);
                if t_proof.wait_queue_state().inherited_scheduler_state_storage() == iss {
                    found = true;
                    break;
                }
            }
            debug_assert!(found);
        } else {
            debug_assert!(self.is_empty());
        }
    }

    /// Validates the inherited scheduler state storage location when extra scheduler invariant
    /// validation is enabled.
    #[inline]
    fn validate_sched_state_storage(&mut self) {
        if cfg!(scheduler_extra_invariant_validation) {
            self.validate_sched_state_storage_unconditional();
        }
    }

    /// Updates the inherited scheduler state storage after `t` has been removed from this queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t.owner()` was just removed from `self.owner()`'s collection.
    #[inline]
    pub(crate) unsafe fn update_sched_state_storage_thread_removed(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
    ) {
        debug_assert!(self.inherited_scheduler_state_storage().is_some());

        let old_iss = t.wait_queue_state().inherited_scheduler_state_storage();
        if Some(old_iss) == self.inherited_scheduler_state_storage() {
            // SAFETY: The new storage location is either `None` (the collection is empty) or the
            // storage of a thread currently blocked in the collection.
            let new_iss = unsafe {
                let new_iss = self.collection_mut().find_inherited_scheduler_state_storage();
                *self.inherited_scheduler_state_storage_mut() = new_iss;
                new_iss
            };
            if let Some(new_iss) = new_iss {
                debug_assert!(old_iss != new_iss);
                // SAFETY: `old_iss` and `new_iss` point to the distinct storages of `t` and of a
                // thread blocked in this queue, both guarded by this queue's lock (held per
                // `self`), with no other references live.
                unsafe {
                    *new_iss.as_ptr() = *old_iss.as_ptr();
                }
            }
            // SAFETY: `old_iss` points to `t`'s storage, which is no longer used by this queue,
            // and `t` proves `t.owner().lock()` is held.
            unsafe {
                (*old_iss.as_ptr()).reset();
            }
        }

        if let Some(iss) = self.inherited_scheduler_state_storage() {
            let min_deadline = self.collection().min_inheritable_relative_deadline();
            // SAFETY: `iss` points to the storage of a thread blocked in this queue, guarded by
            // this queue's lock (held per `self`), with no other references live.
            unsafe {
                (*iss.as_ptr()).ipvs.min_deadline = min_deadline;
            }
        }
    }

    /// Updates the inherited scheduler state storage after `t` has been added to this queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t.owner()` was just added to `self.owner()`'s collection.
    #[inline]
    unsafe fn update_sched_state_storage_thread_added(
        &mut self,
        t: &mut ChainLockProof<'_, Thread>,
    ) {
        if self.inherited_scheduler_state_storage().is_none() {
            debug_assert!(self.count() == 1, "Expected count == 1, instead of {}", self.count());
            let iss = t.wait_queue_state().inherited_scheduler_state_storage();
            // SAFETY: `t.owner()` is the only thread blocked in this queue, so its storage is a
            // valid storage location.
            unsafe {
                *self.inherited_scheduler_state_storage_mut() = Some(iss);
            }
            // SAFETY: `iss` points to `t`'s storage, now guarded by this queue's lock (held per
            // `self`), with no other references live.
            unsafe { iss.as_ref() }.assert_is_reset();
        } else {
            debug_assert!(self.count() > 1, "Expected count > 1, instead of {}", self.count());
        }
    }

    /// Sets the owner of this queue to `owner`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this queue is not destroyed while it has an owner (the
    /// `OwnedWaitQueue` ownership invariant, checked by `teardown`), since `owner`'s
    /// `owned_wait_queues_` list holds a raw pointer to it.
    #[inline]
    unsafe fn set_owner(&mut self, owner: &mut ChainLockProof<'_, Thread>) {
        // If we are adding an owner to this OWQ, we should be able to assert that
        // it does not currently have one.
        debug_assert!(self.owner_thread().is_none());
        // SAFETY: `self.owner()` is not in any list (it has no owner), and it remains alive while
        // it has an owner (per the caller).
        unsafe {
            owner.owned_wait_queues_mut().push_back_raw(NonNull::from_ref(self.owner()));
        }
        // SAFETY: `owner` now contains this queue in its `owned_wait_queues_` list.
        unsafe {
            *self.owner_thread_mut() = Some(ThreadPtr::from(owner.owner()));
        }
    }

    /// Resets the owner of this queue from `owner` to `None`. We need to be holding both the
    /// queue's lock and the owner's lock to do this.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `owner` is the current owner of this queue.
    #[inline]
    unsafe fn reset_owner(&mut self, owner: &mut ChainLockProof<'_, Thread>) {
        debug_assert!(same_thread(self.owner_thread(), Some(owner.owner())));
        // SAFETY: `self.owner()` is a member of `owner`'s `owned_wait_queues_` list (it is the
        // owner, per the caller).
        unsafe {
            owner.owned_wait_queues_mut().erase(self.owner());
        }
        // SAFETY: This queue has been removed from `owner`'s `owned_wait_queues_` list.
        unsafe {
            *self.owner_thread_mut() = None;
        }
    }
}

impl OwnedWaitQueue {
    pub const OWNED_MAGIC: u32 = OWNED_MAGIC;

    /// In-place pinned initializer for `OwnedWaitQueue`.
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init::pin_init!(Self {
            // SAFETY: `OWNED_MAGIC` is the valid magic value for the `base` subobject of an
            // `OwnedWaitQueue`.
            base <- unsafe { WaitQueueBase::init_with_magic(OWNED_MAGIC) },
            dll_node: DoublyLinkedListNode::new(),
            owner: KCell::new(None),
            inherited_scheduler_state_storage: KCell::new(None),
        })
    }

    /// Domain-specific conversion: returns raw pointer for `OwnedWaitQueue`.
    #[inline]
    pub fn as_raw(&self) -> *mut bindings::OwnedWaitQueue {
        core::ptr::from_ref(self).cast_mut().cast()
    }

    /// Returns a raw mutable pointer to the underlying C++ `OwnedWaitQueue`.
    #[inline]
    pub fn as_ptr(&self) -> *mut c_void {
        self.as_raw().cast()
    }

    /// Returns a default no-op [`DefaultWakeHooks`] instance.
    #[inline]
    pub(crate) const fn default_wake_hooks() -> DefaultWakeHooks {
        DefaultWakeHooks
    }

    /// Tears down an owned wait queue.
    ///
    /// # Safety
    ///
    /// The caller must ensure that this wait queue is being destroyed with no concurrent accesses.
    unsafe fn teardown(&self) {
        // SAFETY: `self`'s lock is not held. The caller guarantees that `self` is quiescent
        // (being destroyed with no concurrent accesses), so `owner` cannot change while `proof`
        // is used to read it.
        let proof = unsafe { assume_synchronized_read_only(self) };
        // Something is very very wrong if we have been allowed to destruct while we
        // still have an owner.
        assert!(proof.owner_thread().is_none());
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
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            // We should have cleared the "can own wait queues" flag by now, which
            // should guarantee that we cannot possibly become the owner of any new
            // queues.  Our state, however, should still be "RUNNING".  Once we have
            // finally emptied the our list of owned queues, the exiting thread can
            // fully transition to the DEATH state.
            let mut guard = OwnerGuard::new(t, token);
            {
                let t_proof = guard.proof();
                debug_assert!(!t_proof.can_own_wait_queues());
                debug_assert!(t_proof.state() == ThreadStateKind::Running);
                debug_assert!(
                    t_proof.wait_queue_state().blocking_wait_queue(t_proof.token()).is_none()
                );
            }

            while let Some(queue) = guard.proof().owned_wait_queues().front().map(NonNull::from) {
                // SAFETY: `queue` is owned by `t`, and an `OwnedWaitQueue` cannot be destroyed
                // while it has an owner, which cannot change without holding `t`'s lock (held via
                // `guard`).
                let queue: &OwnedWaitQueue = unsafe { queue.as_ref() };

                let Some(mut queue_guard) = OwnerGuard::acquire_or_backoff(queue, token) else {
                    return Action::Backoff.into();
                };

                // SAFETY: `t` is `queue`'s current owner (`queue` is on `t`'s owned list).
                unsafe {
                    queue_guard.proof().reset_owner(&mut guard.proof());
                }
                queue_guard.release();
            }

            // Finalization is a bit strange here.  Typically, we obtain all of the
            // locks we will need for an operation, finalize our transaction, and then
            // actually perform the operation.  In other words, aside from gathering our
            // locks, we are doing no work in the pre-finalization stage.
            //
            // In this case, we are unlinking `t` from all of the queues is currently
            // owns; obtaining each queue lock as we go, doing the actual work of
            // unlinking the queue, and then dropping the queue lock before proceeding
            // to the next.  So, we are doing actual work in what would have otherwise
            // been the "pre-finalization" stage of things.
            //
            // So, when looking at lock contention statistics for this transaction,
            // users need to take into account that contention times include all of the
            // actual work done in the operation, not just the lock collection phase.
            ChainLockTransaction::finalize(token);

            DONE.into()
        };
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::DisownAllQueues"),
            do_transaction,
        );
    }

    /// Assert that a thread (which we have locked exclusively) owns a specific
    /// wait queue, meaning that we should be able to examine parts of that queue's
    /// state (like, whether or not it has any blocked threads, and what the IPVs
    /// of the queue currently are).  See comments in `apply_ipv_delta_to_thread` for
    /// details.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `t`'s lock is held by the active transaction.
    #[inline]
    unsafe fn assert_owns_wait_queue(t: &Thread, owq: &OwnedWaitQueue) {
        // SAFETY: `owq`'s lock is not held. `owq.owner` cannot be changed from (or to) `t`
        // without also holding `t`'s lock, which the caller holds, so if `t` is the owner (the
        // asserted invariant) the value read through `owq_proof` is stable. `owq_proof` is only
        // used for this read.
        let owq_proof = unsafe { assume_synchronized_read_only(owq) };
        debug_assert!(same_thread(owq_proof.owner_thread(), Some(t)));
    }

    /// Helper function for propagating inherited profile value add and remove
    /// operations.  Snapshots and returns the combination of a thread's currently
    /// inherited profile values along with its base profile, which should be the
    /// profile pressure it is transmitting to the next node in the graph.
    #[inline(always)]
    fn snapshot_thread_ipv(
        thread_proof: &ChainLockProof<'_, Thread>,
        force_inheritance: ForceInheritance,
    ) -> InheritedProfileValues {
        let mut ret = *thread_proof.inherited_profile_values();
        let bp: &BaseProfile = thread_proof.base_profile();

        if bp.inheritable || force_inheritance == ForceInheritance::Yes {
            if bp.discipline() == SchedDiscipline::Fair {
                ret.total_weight += bp.fair_weight();
            } else {
                ret.assert_consistency();
                let deadline = bp.deadline();
                ret.uncapped_utilization += deadline.utilization;
                ret.min_deadline = core::cmp::min(ret.min_deadline, deadline.deadline_ns);
                if bp.critical {
                    ret.critical_count += 1;
                }
            }
        }

        ret
    }

    /// Apply a change in IPV to a thread.  When `Some`, `old_ipv` refers to the
    /// IPV values which need to be removed from the thread's IPVs, while `new_ipv`
    /// refers to the IPV vales which need to be added to the  thread's IPVs.
    ///
    /// 1) When a graph edge is added, `old_ipv` will be `None` as no IPV pressure
    ///    was removed.
    /// 2) When a graph edge is removed, `new_ipv` will be `None` as no IPV
    ///    pressure was added.
    /// 3) When a thread's base profile changes, neither `old_ipv` nor `new_ipv`
    ///    will be `None`, since the change of a thread's base profile is
    ///    equivalent to the removal of one set of IPV, and the addition of
    ///    another.
    /// 4) It is never correct for both `old_ipv` and `new_ipv` to be `None`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the locks of `thread` and of every node in the PI chain
    /// downstream of the propagation which reached `thread` are held by the active transaction,
    /// and that no other proof or guard for `thread` is used during the call.
    unsafe fn apply_ipv_delta_to_thread(
        old_ipv: Option<&InheritedProfileValues>,
        new_ipv: Option<&InheritedProfileValues>,
        thread: &Thread,
    ) {
        debug_assert!(old_ipv.is_some() || new_ipv.is_some());

        let mut weight_delta = new_ipv.map_or(SchedWeight::from_raw(0), |ipv| ipv.total_weight);
        let mut util_delta =
            new_ipv.map_or(SchedUtilization::from_raw(0), |ipv| ipv.uncapped_utilization);
        let mut critical_delta: i32 = new_ipv.map_or(0, |ipv| ipv.critical_count);
        if let Some(old_ipv) = old_ipv {
            weight_delta -= old_ipv.total_weight;
            util_delta -= old_ipv.uncapped_utilization;
            critical_delta -= old_ipv.critical_count;
        }

        // SAFETY: Forwarded from the caller.
        let mut thread_proof = unsafe { assume_locked(thread) };

        // Only the fields of the thread's `SchedulerState` which are guarded by its lock are
        // accessed (through field projections), since a thread at the end of a PI chain may be in
        // another CPU's run queue, whose `queue_lock_` guards its other fields.
        thread_proof.effective_profile_mut().mark_inherited_profile_changed();
        let thread_min_deadline = {
            let thread_ipv = thread_proof.inherited_profile_values_mut();
            thread_ipv.total_weight += weight_delta;
            thread_ipv.uncapped_utilization += util_delta;
            thread_ipv.critical_count += critical_delta;

            debug_assert!(thread_ipv.total_weight >= SchedWeight::from_raw(0));
            debug_assert!(thread_ipv.uncapped_utilization >= SchedUtilization::from_raw(0));
            debug_assert!(thread_ipv.critical_count >= 0);
            thread_ipv.min_deadline
        };

        // If a set of IPVs is going away, and the value which is going away was the
        // minimum, then we need to recompute the new minimum by checking the
        // minimum across all of this thread's owned wait queues.
        //
        // TODO(johngro): Consider keeping the set of owned wait queues as a WAVL
        // tree, indexed by minimum relative deadline, so that this can be
        // maintained in O(1) time instead of O(N).
        //
        // Notes on locking:
        //
        // We need to iterate over our set of owned queues, and find the minimum
        // deadline across all of the queues.  It is not immediately obvious why it is
        // OK to do this.  We are holding our thread's lock, meaning that queues we
        // own cannot leave our collection (mutating the collection requires holding
        // both the thread's lock as well as the lock of the queue which is being
        // added or removed), but we are not explicitly holding the queue's lock.
        //
        // So, why is it OK for us to examine the queue's state?  Specifically, how
        // many threads are blocked in it, and what the minimum deadline across those
        // threads is currently?  It is because, any modification to the upstream
        // queue's state which would mutate the inherited minimum deadline would also
        // require the mutator to hold the entire PI locking chain from (at least) our
        // upstream queue to the eventual target, which (since we currently own this
        // queue) must pass through us as well.
        //
        // So, any previous change made to this value (while we were the queue's
        // owner) had to be done holding this thread's lock as well (since the entire
        // path is locked).  This include any change which adds or removes us as the
        // queue's owner.  Since we are now holding this thread's lock, any of those
        // previous mutations must have happened-before us, and no new mutations can
        // take place until we drop our lock.
        //
        let new_min_deadline = match new_ipv {
            Some(new_ipv) if new_ipv.min_deadline <= thread_min_deadline => {
                core::cmp::min(thread_min_deadline, new_ipv.min_deadline)
            }
            _ => {
                let mut min_deadline = thread_min_deadline;
                if old_ipv.is_some_and(|old_ipv| old_ipv.min_deadline <= thread_min_deadline) {
                    min_deadline = SchedDuration::from_raw(i64::MAX);

                    for other_queue in thread_proof.owned_wait_queues().iter() {
                        // SAFETY: `other_queue`'s lock is not held. `thread`'s lock is held (per
                        // the caller). Per the "Notes on locking" above, any mutation of
                        // `other_queue`'s inherited scheduler state storage (and of the IPVs it
                        // holds) requires holding the whole PI chain through `thread`, including
                        // `thread`'s lock, so those are stable while `thread`'s lock is held.
                        // (The waiters in `other_queue`'s collection are *not* stable: a requeue
                        // locking operation may temporarily remove them while holding only the
                        // queue locks, so the collection is not read here.) `other_queue_proof` is
                        // only used for the reads of `owner` and the storage location below.
                        let other_queue_proof =
                            unsafe { assume_synchronized_read_only(other_queue) };
                        // SAFETY: `thread`'s lock is held per the caller.
                        unsafe { Self::assert_owns_wait_queue(thread, other_queue) };
                        // A queue has storage if and only if it has (or, transiently during a
                        // thread-initiated removal of its last waiter, just had) waiters. In the
                        // transient case, its `ipvs.min_deadline` has already been reset to the
                        // maximum, so it does not contribute to the minimum.
                        if let Some(other_iss) =
                            other_queue_proof.inherited_scheduler_state_storage()
                        {
                            // SAFETY: The storage of a queue owned by `thread` is stable while
                            // `thread`'s lock is held (see the "Notes on locking" above), and is
                            // only read here.
                            let other_ipvs = unsafe { &other_iss.as_ref().ipvs };
                            min_deadline = core::cmp::min(min_deadline, other_ipvs.min_deadline);
                        }
                    }
                }

                match new_ipv {
                    Some(new_ipv) => core::cmp::min(min_deadline, new_ipv.min_deadline),
                    None => min_deadline,
                }
            }
        };

        let thread_ipv = thread_proof.inherited_profile_values_mut();
        thread_ipv.min_deadline = new_min_deadline;
        thread_ipv.assert_consistency();
    }

    /// Apply a change in IPV to an owned wait queue.  See `apply_ipv_delta_to_thread`
    /// for an explanation of the parameters.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `owq`'s lock is held by the active transaction, that no other
    /// proof or guard for `owq` is used during the call, and that neither `old_ipv` nor `new_ipv`
    /// refers to `owq`'s inherited scheduler state storage.
    #[inline]
    unsafe fn apply_ipv_delta_to_owq(
        old_ipv: Option<&InheritedProfileValues>,
        new_ipv: Option<&InheritedProfileValues>,
        owq: &OwnedWaitQueue,
    ) {
        let mut weight_delta = new_ipv.map_or(SchedWeight::from_raw(0), |ipv| ipv.total_weight);
        let mut util_delta =
            new_ipv.map_or(SchedUtilization::from_raw(0), |ipv| ipv.uncapped_utilization);
        let mut critical_delta: i32 = new_ipv.map_or(0, |ipv| ipv.critical_count);

        if let Some(old_ipv) = old_ipv {
            weight_delta -= old_ipv.total_weight;
            util_delta -= old_ipv.uncapped_utilization;
            critical_delta -= old_ipv.critical_count;
        }

        // SAFETY: Forwarded from the caller.
        let owq_proof = unsafe { assume_locked(owq) };
        debug_assert!(!owq_proof.is_empty() || new_ipv.is_none());
        let iss_ptr = owq_proof
            .inherited_scheduler_state_storage()
            .expect("OwnedWaitQueue has inherited scheduler state storage");
        let min_deadline = owq_proof.collection().min_inheritable_relative_deadline();
        // SAFETY: `iss_ptr` is `owq`'s storage, guarded by `owq`'s lock (held per the caller),
        // with no other references live.
        let iss: &mut WaitQueueInheritedSchedulerState = unsafe { &mut *iss_ptr.as_ptr() };

        iss.ipvs.total_weight += weight_delta;
        iss.ipvs.uncapped_utilization += util_delta;
        iss.ipvs.critical_count += critical_delta;
        iss.ipvs.min_deadline = min_deadline;

        iss.ipvs.assert_consistency();
    }

    #[cold]
    #[inline(never)]
    #[track_caller]
    fn panic_missing_owq_iss() -> ! {
        panic!("OwnedWaitQueue has inherited scheduler state storage");
    }

    /// Begin a propagation operation starting from an upstream thread, and going
    /// through a downstream owned wait queue.  Only for use during edge add/remove
    /// operations. Base profile changes call `finish_propagate` directly when
    /// required.  Note: Links between the upstream thread and downstream queue
    /// should have already been added/removed by the time that this method is
    /// called.
    ///
    /// The entire PI chain starting from the wait queue needs to be locked before
    /// calling this. No locks are released by this operation; the caller unlocks the chain
    /// afterwards.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the locks of every node in the PI chain downstream of
    /// `downstream_node` are held by the active transaction, and that no proofs or guards for
    /// those nodes are used during the call. (The locks of `upstream_node` and `downstream_node`
    /// themselves are proven held by their proofs.)
    #[inline(always)]
    pub(crate) unsafe fn begin_propagate_thread_owq<Op: PropagateOpTag>(
        upstream_node: &mut ChainLockProof<'_, Thread>,
        downstream_node: &mut ChainLockProof<'_, OwnedWaitQueue>,
        op: Op,
        force_inheritance: ForceInheritance,
        old_target_ep: Option<&EffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        // When needed, base profile changes will directly call `finish_propagate`.
        const { assert!(!matches!(Op::OP, PropagateOp::BaseProfileChanged)) };

        let ipv_snapshot = Self::snapshot_thread_ipv(upstream_node, force_inheritance);

        // Fast path for the common case of a fair thread blocking in or unblocking from an
        // unowned wait queue (such as a standard non-PI futex): update the queue's inherited
        // profile values in place without entering the multi-hop `finish_propagate` loop.
        if ipv_snapshot.uncapped_utilization == SchedUtilization::from_raw(0)
            && downstream_node.owner_thread().is_none()
        {
            if ipv_snapshot.total_weight == SchedWeight::from_raw(0) {
                return;
            }
            // Callers insert the thread into `downstream_node` before `AddSingleEdge`,
            // so `count()` must be non-zero. Conversely, callers dequeue the thread
            // before `RemoveSingleEdge`, so `count()` is legitimately 0 when the last
            // waiter leaves (while `inherited_scheduler_state_storage` stays valid
            // until propagation finishes).
            if Op::OP == PropagateOp::AddSingleEdge {
                debug_assert!(downstream_node.count() > 0);
            }
            let Some(owq_iss_nn) = downstream_node.inherited_scheduler_state_storage() else {
                Self::panic_missing_owq_iss();
            };
            let owq_iss = owq_iss_nn.as_ptr();
            let min_deadline = downstream_node.collection().min_inheritable_relative_deadline();
            // SAFETY: `owq_iss` is `downstream_node`'s storage, guarded by `downstream_node`'s
            // lock (proven held by `downstream_node`), with no other references live.
            unsafe {
                (*owq_iss).ipvs.assert_consistency();
                if Op::OP == PropagateOp::RemoveSingleEdge {
                    (*owq_iss).ipvs.total_weight -= ipv_snapshot.total_weight;
                    (*owq_iss).ipvs.critical_count -= ipv_snapshot.critical_count;
                } else {
                    (*owq_iss).ipvs.total_weight += ipv_snapshot.total_weight;
                    (*owq_iss).ipvs.critical_count += ipv_snapshot.critical_count;
                }
                (*owq_iss).ipvs.min_deadline = min_deadline;
                (*owq_iss).ipvs.assert_consistency();
                if (*owq_iss).ipvs.uncapped_utilization == SchedUtilization::from_raw(0) {
                    (*owq_iss).assert_dynamic_parameters_are_reset();
                }
            }
            let mut len_tracker = ChainLengthTracker::<DEFAULT_CHAIN_LENGTH_TRACKER_OPT>::new();
            len_tracker.node_visited();
            return;
        }

        // `finish_propagate` walks the chain using the nodes' locks directly. The unique borrows
        // of `upstream_node` and `downstream_node` keep those proofs unused during the call.
        let upstream_node: &Thread = upstream_node.owner();
        let downstream_node: &OwnedWaitQueue = downstream_node.owner();
        // SAFETY: The locks of `upstream_node` and `downstream_node` are held (proven by the
        // proofs borrowed above), the caller guarantees the rest of the downstream chain is
        // locked by the active transaction, and no proofs or guards for those nodes are used
        // during the call.
        unsafe {
            Self::begin_propagate_thread_owq_slow(
                upstream_node,
                downstream_node,
                &ipv_snapshot,
                op,
                force_inheritance,
                old_target_ep,
                clt_token,
                preempt_disabled_token,
            );
        }
    }

    /// Out-of-line slow path for [`Self::begin_propagate_thread_owq`] when deadline pressure is
    /// present or `downstream_node` has an owner thread.
    ///
    /// # Safety
    ///
    /// Same as [`Self::finish_propagate`] for `upstream_node` and `downstream_node`.
    #[allow(clippy::too_many_arguments)]
    #[inline(never)]
    unsafe fn begin_propagate_thread_owq_slow<Op: PropagateOpTag>(
        upstream_node: &Thread,
        downstream_node: &OwnedWaitQueue,
        ipv_snapshot: &InheritedProfileValues,
        op: Op,
        force_inheritance: ForceInheritance,
        old_target_ep: Option<&EffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        // SAFETY: Forwarded from the caller.
        unsafe {
            if Op::OP == PropagateOp::RemoveSingleEdge {
                Self::finish_propagate(
                    upstream_node,
                    downstream_node,
                    None,
                    Some(ipv_snapshot),
                    op,
                    force_inheritance,
                    old_target_ep,
                    clt_token,
                    preempt_disabled_token,
                );
            } else if Op::OP == PropagateOp::AddSingleEdge {
                Self::finish_propagate(
                    upstream_node,
                    downstream_node,
                    Some(ipv_snapshot),
                    None,
                    op,
                    force_inheritance,
                    old_target_ep,
                    clt_token,
                    preempt_disabled_token,
                );
            }
        }
    }

    /// Begin a propagation operation starting from an upstream owned wait queue,
    /// and going through a downstream thread.  Only for use during edge add/remove
    /// operations. Base profile changes call `finish_propagate` directly when
    /// required.  Note: Links between the upstream queue and downstream thread
    /// should *not* have already been added/removed by the time that this method
    /// is called.  The method will handle updating the links.
    ///
    /// The entire PI chain starting from the wait queue needs to be locked before
    /// calling this. No locks are released by this operation; the caller unlocks the chain
    /// afterwards.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The locks of `downstream_node` and every node in the PI chain downstream of it are held
    ///   by the active transaction, and no proofs or guards for those nodes are used during the
    ///   call. (The lock of the upstream queue is proven held by `upstream_proof`.) The
    ///   transaction need not be finalized: the `assign_owner` paths propagate before finalizing,
    ///   as in C++.
    /// - For `RemoveSingleEdge`, `downstream_node` is the queue's current owner, and for
    ///   `AddSingleEdge`, the queue currently has no owner.
    unsafe fn begin_propagate_owq_thread<Op: PropagateOpTag>(
        upstream_proof: &mut ChainLockProof<'_, OwnedWaitQueue>,
        downstream_node: &Thread,
        op: Op,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        // When needed, base profile changes will directly call `finish_propagate`.
        const { assert!(!matches!(Op::OP, PropagateOp::BaseProfileChanged)) };

        {
            // SAFETY: `downstream_node` is the (new or current) owner of `upstream_proof.owner()`,
            // the first node of the PI chain downstream of it, whose lock the caller guarantees is
            // held by the active transaction with no other proof or guard in use.
            let mut downstream_proof = unsafe { assume_locked(downstream_node) };
            if Op::OP == PropagateOp::AddSingleEdge {
                // SAFETY: Queues are not destroyed while owned (checked by `teardown`).
                unsafe { upstream_proof.set_owner(&mut downstream_proof) };
            } else if Op::OP == PropagateOp::RemoveSingleEdge {
                // SAFETY: For edge removal, the caller passes the queue's current owner.
                unsafe { upstream_proof.reset_owner(&mut downstream_proof) };
            }
        }

        // If the OWQ we are starting from has no active waiters, then there are no
        // IPV deltas to propagate.  After updating the links, we are finished.
        if upstream_proof.is_empty() {
            return;
        }

        let iss_ptr = upstream_proof
            .inherited_scheduler_state_storage()
            .expect("non-empty OwnedWaitQueue has inherited scheduler state storage");
        // SAFETY: `iss_ptr` is `upstream_node`'s storage, guarded by its lock (proven held by
        // `upstream_proof`). Propagation from `upstream_node` never applies deltas to
        // `upstream_node` itself, so `ipvs` is not mutated while this reference is live. Only
        // `ipvs` is borrowed: during the call, the scheduler (`SplitNodeFromPiGraph`) may write
        // the sibling dynamic scheduling parameter fields of this same storage, which are
        // disjoint.
        let ipvs: &InheritedProfileValues = unsafe { &(*iss_ptr.as_ptr()).ipvs };
        ipvs.assert_consistency();

        let upstream_node: &OwnedWaitQueue = upstream_proof.owner();
        // SAFETY: `upstream_node`'s lock is held (proven by `upstream_proof`, which is mutably
        // borrowed and unused during the call), and the caller guarantees that `downstream_node`
        // and the rest of the chain downstream of it are locked by the active transaction with no
        // proofs or guards for those nodes in use.
        unsafe {
            if Op::OP == PropagateOp::RemoveSingleEdge {
                Self::finish_propagate(
                    upstream_node,
                    downstream_node,
                    None,
                    Some(ipvs),
                    op,
                    ForceInheritance::No,
                    None,
                    clt_token,
                    preempt_disabled_token,
                );
            } else if Op::OP == PropagateOp::AddSingleEdge {
                Self::finish_propagate(
                    upstream_node,
                    downstream_node,
                    Some(ipvs),
                    None,
                    op,
                    ForceInheritance::No,
                    None,
                    clt_token,
                    preempt_disabled_token,
                );
            }
        }
    }

    /// Finishing handling a propagation operations started from either version of
    /// `begin_propagate`, or from `set_thread_base_profile_and_propagate`.  Traverses the PI chain
    /// propagating IPV deltas, and calls into the scheduler to finish the
    /// operation once the end of the chain is reached.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the locks of `upstream_node`, `downstream_node`, and every node
    /// in the PI chain downstream of `downstream_node` are held by the active transaction, and
    /// that no proofs or guards for those nodes are used during the call.
    #[allow(clippy::too_many_arguments)]
    unsafe fn finish_propagate<Op: PropagateOpTag, U: PiNode, D: PiNode>(
        upstream_node: &U,
        downstream_node: &D,
        added_ipv: Option<&InheritedProfileValues>,
        lost_ipv: Option<&InheritedProfileValues>,
        _op: Op,
        force_inheritance: ForceInheritance,
        old_target_ep: Option<&EffectiveProfile>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        // Propagation must start from a(n) (OWQ|Thread) and proceed to a(n) (Thread|OWQ).
        const {
            assert!(
                U::IS_THREAD != D::IS_THREAD,
                "Bad types for finish_propagate.  Must be either OWQ -> Thread, or Thread -> OWQ"
            )
        };
        const {
            assert!(
                !matches!(Op::OP, PropagateOp::BaseProfileChanged) || U::IS_THREAD,
                "Base profile propagation changes may only start from Threads, not \
                 OwnedWaitQueues"
            )
        };

        let starting_from_thread = U::IS_THREAD;

        if Op::OP == PropagateOp::AddSingleEdge {
            debug_assert!(added_ipv.is_some());
            debug_assert!(lost_ipv.is_none());
        } else if Op::OP == PropagateOp::RemoveSingleEdge {
            debug_assert!(added_ipv.is_none());
            debug_assert!(lost_ipv.is_some());
        } else if Op::OP == PropagateOp::BaseProfileChanged {
            debug_assert!(added_ipv.is_some());
            debug_assert!(lost_ipv.is_some());
        }

        // If neither the IPVs we are adding, nor the IPVs we are removing, are
        // "consequential" (meaning, the have either some fair weight, or some
        // deadline capacity, or both), then we can just get out now.  There are no
        // effective changes to propagate.
        if !ipvs_are_consequential(added_ipv) && !ipvs_are_consequential(lost_ipv) {
            return;
        }

        // When we have finally finished updating everything, make sure to update
        // our max traversal statistic.
        let mut len_tracker = ChainLengthTracker::<DEFAULT_CHAIN_LENGTH_TRACKER_OPT>::new();

        // Set up the references we will use as iterators for traversing the inheritance
        // graph.  Snapshot the starting node's current inherited profile values which
        // we need to propagate.
        let mut owq_iter: &OwnedWaitQueue;
        let mut thread_iter: &Thread;
        let mut skip_phase1 = false;

        match (upstream_node.pi_node_ref(), downstream_node.pi_node_ref()) {
            (PiNodeRef::Thread(upstream), PiNodeRef::OwnedWaitQueue(downstream)) => {
                thread_iter = upstream;
                owq_iter = downstream;

                // Is this a base profile changed operation?  If so, we should already have
                // a link between our thread and the downstream owned wait queue.  Go ahead
                // and assert this.  We don't need to bother to check the other
                // combinations; those have already been asserted during
                // `begin_propagate`.
                if Op::OP == PropagateOp::BaseProfileChanged {
                    thread_iter.lock().assert_held(clt_token);
                    // SAFETY: `thread_iter` is `upstream_node`, whose lock the caller guarantees
                    // is held by the transaction (asserted above), and no other proof or guard
                    // for it is used during the call.
                    let thread_proof = unsafe { assume_locked(thread_iter) };
                    let blocking_wait_queue =
                        thread_proof.wait_queue_state().blocking_wait_queue(thread_proof.token());
                    debug_assert!(
                        blocking_wait_queue == Some(NonNull::from(owq_iter).cast()),
                        "blocking wait queue {:?} owq_iter {:p}",
                        blocking_wait_queue,
                        owq_iter
                    );
                }
            }
            (PiNodeRef::OwnedWaitQueue(upstream), PiNodeRef::Thread(downstream)) => {
                // Base profile changes should never start from OWQs (statically asserted at the
                // top of this function).
                owq_iter = upstream;
                thread_iter = downstream;
                owq_iter.lock().assert_held(clt_token);
                // SAFETY: `owq_iter` is `upstream_node`, whose lock the caller guarantees is held
                // by the transaction (asserted above); the proof is only used for this read.
                debug_assert!(!unsafe { assume_locked(owq_iter) }.is_empty());
                skip_phase1 = true;
            }
            // Statically excluded by the assertion at the top of this function.
            _ => unreachable!(),
        }

        loop {
            if !skip_phase1 {
                // During a thread-initiated RemoveSingleEdge, the final departing thread
                // has already been removed from the initial wait queue collection, so its
                // `count()` may be 0, while its `inherited_scheduler_state_storage` is
                // retained until after `begin_propagate` completes.
                owq_iter.lock().assert_held(clt_token);
                let owq_iss = {
                    // SAFETY: `owq_iter`'s lock is held as part of the locked PI chain.
                    let owq_proof = unsafe { assume_locked(owq_iter) };
                    if starting_from_thread && Op::OP == PropagateOp::RemoveSingleEdge {
                        debug_assert!(
                            owq_proof.count() > 0 || core::ptr::addr_eq(owq_iter, downstream_node)
                        );
                    } else {
                        debug_assert!(owq_proof.count() > 0);
                    }
                    owq_proof
                        .inherited_scheduler_state_storage()
                        .expect("OwnedWaitQueue has inherited scheduler state storage")
                        .as_ptr()
                };

                // Record what our deadline pressure was before we accumulate the upstream
                // pressure into this node.  We will need it to reason about what to do
                // with our dynamic scheduling parameters after IPV accumulation.
                //
                // OWQs which are receiving deadline pressure have defined dynamic
                // scheduler parameters (start_time, finish_time, time_slice_ns) finish
                // times, which they inherited from their upstream deadline threads.  Fair
                // threads do not have things like a defined start time while they are
                // blocked, they will get a new set of dynamic parameters the next time
                // they unblock and are scheduled to run.
                //
                // After we have finished accumulating the IPV deltas, we a few different
                // potential situations:
                //
                // 1) The utilization (deadline pressure) has not changed.  Therefore,
                //    nothing about the dynamic parameters needs to change either.
                // 2) The utilization has changed, and it was 0 before.  This is the first
                //    deadline thread to join the queue, so we can just copy its dynamic
                //    parameters.
                // 3) The utilization has changed, and it is now 0.  The final deadline
                //    thread has left this queue, and our dynamic params are now
                //    undefined.  Strictly speaking, we don't have to do anything, but in
                //    builds with extra checks enabled, we reset the dynamic parameters
                //    so that they have a deterministic value.
                // 4) The utilization has changed, but it was not zero before, and isn't
                //    zero now either.  We call into the scheduler code to compute what
                //    the new dynamic parameters should be.
                //
                // SAFETY: `owq_iss` is `owq_iter`'s storage, guarded by `owq_iter`'s lock (held
                // per the caller). No other reference to it is live during each access below.
                unsafe { (*owq_iss).ipvs.assert_consistency() };
                // SAFETY: As above.
                let utilization_before = unsafe { (*owq_iss).ipvs.uncapped_utilization };
                let old_owq_ep = (utilization_before != SchedUtilization::from_raw(0)).then(|| {
                    match old_target_ep {
                        Some(old_target_ep) => *old_target_ep,
                        // SAFETY: `owq_iter`'s lock is held as part of the locked PI chain.
                        None => unsafe { assume_locked(owq_iter) }.get_effective_profile(),
                    }
                });
                // SAFETY: `owq_iter`'s lock is held, and neither `lost_ipv` nor `added_ipv`
                // refers to `owq_iter`'s storage (they refer to snapshots or to the storage of
                // the upstream queue, which is never `owq_iter`).
                unsafe { Self::apply_ipv_delta_to_owq(lost_ipv, added_ipv, owq_iter) };
                // SAFETY: As above.
                let utilization_after = unsafe { (*owq_iss).ipvs.uncapped_utilization };

                if utilization_before != utilization_after {
                    if utilization_before == SchedUtilization::from_raw(0) {
                        // First deadline thread just arrived, copy its parameters.
                        thread_iter.lock().assert_held(clt_token);
                        // SAFETY: `thread_iter`'s lock is held as part of the locked PI chain.
                        let thread_proof = unsafe { assume_locked(thread_iter) };
                        let (start_time, finish_time, time_slice_ns, time_slice_used_ns) =
                            thread_proof.dynamic_parameters();
                        // SAFETY: As above for `owq_iss`.
                        unsafe {
                            (*owq_iss).start_time = start_time;
                            (*owq_iss).finish_time = finish_time;
                            (*owq_iss).time_slice_ns = time_slice_ns;
                            (*owq_iss).time_slice_used_ns = time_slice_used_ns;
                        }
                    } else if utilization_after == SchedUtilization::from_raw(0) {
                        // Last deadline thread just left, propagate before resetting.
                        // SAFETY: Forwarded from the caller.
                        unsafe {
                            Op::propagate(
                                upstream_node,
                                owq_iter,
                                force_inheritance,
                                old_owq_ep.as_ref(),
                                clt_token,
                                preempt_disabled_token,
                            );
                        }
                        // SAFETY: As above for `owq_iss`.
                        unsafe { (*owq_iss).reset_dynamic_parameters() };
                    } else {
                        // The overall utilization has changed, but there was deadline
                        // pressure both before and after.  We need to recompute the dynamic
                        // scheduler parameters.
                        // SAFETY: Forwarded from the caller.
                        unsafe {
                            Op::propagate(
                                upstream_node,
                                owq_iter,
                                force_inheritance,
                                old_owq_ep.as_ref(),
                                clt_token,
                                preempt_disabled_token,
                            );
                        }
                    }
                }

                // If we no longer have any deadline pressure, our parameters should now
                // be reset to initialization defaults.
                if utilization_after == SchedUtilization::from_raw(0) {
                    // SAFETY: As above for `owq_iss`.
                    unsafe { (*owq_iss).assert_dynamic_parameters_are_reset() };
                }

                // Advance to the next thread, if any.  If there isn't another thread,
                // then we are finished, simply break out of the propagation loop.
                len_tracker.node_visited();
                // SAFETY: `owq_iter`'s lock is held as part of the locked PI chain.
                let next_thread = unsafe { assume_locked(owq_iter) }.owner_thread();
                match next_thread {
                    // SAFETY: The owner of a queue is a live thread whose lock is held as part
                    // of the locked PI chain.
                    Some(next_thread) => thread_iter = unsafe { next_thread.as_ref() },
                    None => break,
                }
            }
            skip_phase1 = false;

            {
                // Propagate from the current `owq_iter` to the current `thread_iter`.
                // Apply the change in pressure to the next thread in the chain.
                thread_iter.lock().assert_held(clt_token);
                // SAFETY: `thread_iter`'s lock (and the rest of the PI chain) is held.
                unsafe { Self::apply_ipv_delta_to_thread(lost_ipv, added_ipv, thread_iter) };
                // SAFETY: Forwarded from the caller.
                unsafe {
                    Op::propagate(
                        upstream_node,
                        thread_iter,
                        force_inheritance,
                        None,
                        clt_token,
                        preempt_disabled_token,
                    );
                }
                len_tracker.node_visited();

                // SAFETY: `thread_iter`'s lock is held as part of the locked PI chain.
                let thread_proof = unsafe { assume_locked(thread_iter) };
                let blocking_wait_queue =
                    thread_proof.wait_queue_state().blocking_wait_queue(thread_proof.token());
                // SAFETY: A thread's blocking wait queue is live while the thread is blocked in
                // it and the thread's lock is held, and it carries provenance over the enclosing
                // `OwnedWaitQueue` if its magic is `OWNED_MAGIC` (`blocking_wait_queue` safety
                // invariant).
                let next = blocking_wait_queue.and_then(|wq| unsafe { Self::downcast_ptr(wq) });
                match next {
                    // SAFETY: As above; the queue's lock is held as part of the locked PI chain.
                    Some(next_owq) => owq_iter = unsafe { next_owq.as_ref() },
                    None => break,
                }
            }
        }
    }

    /// Downcast from a [`WaitQueueBase`] to an [`OwnedWaitQueue`] if possible, using the magic
    /// number to detect the underlying nature of the object. Returns `None` if the object is not
    /// an [`OwnedWaitQueue`].
    ///
    /// The returned pointer is derived from `wq` without forming an intermediate reference, so
    /// it retains `wq`'s provenance.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `wq` points to a live [`WaitQueueBase`], and that if its magic
    /// is [`OWNED_MAGIC`], it is the `base` subobject of an [`OwnedWaitQueue`] and `wq` carries
    /// provenance over the enclosing [`OwnedWaitQueue`].
    #[inline]
    pub(crate) unsafe fn downcast_ptr(wq: NonNull<WaitQueueBase>) -> Option<NonNull<Self>> {
        // SAFETY: The caller guarantees `wq` points to a live `WaitQueueBase`. The temporary
        // reference used to read `magic` does not narrow the provenance of `wq` itself.
        let magic = unsafe { wq.as_ref() }.magic();
        // `base` is at offset 0 of `OwnedWaitQueue` (statically asserted), so for an
        // `OwnedWaitQueue`, `wq` also points to the enclosing object.
        (magic == OWNED_MAGIC).then(|| wq.cast())
    }

    /// Downcast from a [`WaitQueueBase`] to an [`OwnedWaitQueue`] if possible, using the magic
    /// number to detect the underlying nature of the object. Returns `None` if `wq` is `None`, or
    /// if the object is not an [`OwnedWaitQueue`].
    ///
    /// Prefer [`Self::downcast_ptr`] where a raw pointer to the queue is available. Under Stacked
    /// Borrows, a `&WaitQueueBase` only has provenance over the `WaitQueueBase` bytes (however it
    /// was derived), so this downcast relies on Tree Borrows-style `container_of` semantics.
    ///
    /// # Safety
    ///
    /// The caller must ensure that if `wq.magic() == OWNED_MAGIC`, `wq` is the `base` subobject
    /// of a live [`OwnedWaitQueue`] and was derived from a pointer with provenance over the
    /// enclosing [`OwnedWaitQueue`].
    #[inline]
    pub(crate) unsafe fn downcast_to_owq(wq: Option<&WaitQueueBase>) -> Option<&Self> {
        let wq = wq?;
        // SAFETY: `wq` is a live `WaitQueueBase`, and the caller guarantees the provenance
        // requirement if it is the base subobject of an `OwnedWaitQueue`.
        let owq = unsafe { Self::downcast_ptr(NonNull::from(wq)) }?;
        // SAFETY: `owq` is a downcast of the live `wq`, so it is live for the same lifetime.
        Some(unsafe { owq.as_ref() })
    }

    /// Returns the owner of the queue `wq` points to, if it is an [`OwnedWaitQueue`] which has an
    /// owner.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `wq`'s lock is held by the active transaction, that no other
    /// proof or guard for `wq` is used during the call, and that the requirements of
    /// [`Self::downcast_ptr`] are met.
    #[inline]
    unsafe fn get_queue_owner(
        wq: NonNull<WaitQueueBase>,
        clt_token: &ChainLockTransactionToken,
    ) -> Option<ThreadPtr> {
        assert_transaction_active(clt_token);
        // SAFETY: The caller guarantees the requirements of `downcast_ptr`.
        let owq = unsafe { Self::downcast_ptr(wq) };

        if let Some(owq) = owq {
            // `wq`'s lock is the same lock as `owq`'s, so holding it allows us to read `owq`'s
            // owner.
            // SAFETY: The caller guarantees `wq`'s (and so `owq`'s) lock is held, with no other
            // proof or guard for it in use; the proof is only used for this read.
            return unsafe { assume_locked(owq.as_ref()) }.owner_thread();
        }

        None
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
    #[inline]
    pub unsafe fn lock_pi_chain_from_thread(
        start: &mut ChainLockProof<'_, Thread>,
        clt_token: &ChainLockTransactionToken,
    ) -> ChainLockResult {
        assert_transaction_active(clt_token);
        let wq = start.wait_queue_state().blocking_wait_queue(start.token());

        let Some(wq) = wq else {
            return ChainLockResult::Ok;
        };

        // Note: If we refuse cycles, the only possible variant return type for
        // `lock_pi_chain_common` is `ChainLockResult`.
        let start = PiChainNodeRef::WaitQueue(wq);
        // SAFETY: `wq` is `start.owner()`'s blocking wait queue, which is live while the thread is
        // blocked in it and its lock is held, and which carries provenance over the enclosing
        // `OwnedWaitQueue` if its magic is `OWNED_MAGIC` (`blocking_wait_queue` safety
        // invariant). It is the first node downstream of `start.owner()`, so (per our caller) its
        // lock is not held, and neither are the locks downstream of it.
        unsafe { Self::lock_pi_chain_common_refuse_cycle(start, clt_token) }
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
    ///   live [`OwnedWaitQueue`] and was derived from a pointer with provenance over the
    ///   enclosing [`OwnedWaitQueue`] (see [`Self::downcast_to_owq`]; prefer
    ///   [`Self::lock_pi_chain`] for an `OwnedWaitQueue`).
    /// - If this call returns [`ChainLockResult::Ok`], any downstream locks acquired in
    ///   `start.owner()`'s PI chain are properly unlocked before ending the transaction.
    #[inline]
    pub unsafe fn lock_pi_chain_from_wait_queue(
        start: &mut ChainLockProof<'_, WaitQueueBase>,
        clt_token: &ChainLockTransactionToken,
    ) -> ChainLockResult {
        assert_transaction_active(clt_token);
        // SAFETY: `start` proves `start.owner().lock()` is held (and is not used during the
        // call), and the caller upholds the provenance requirements.
        let t = unsafe { Self::get_queue_owner(NonNull::from(start.owner()), clt_token) };

        let Some(t) = t else {
            return ChainLockResult::Ok;
        };
        // SAFETY: `t` owns `start.owner()`, whose lock is held (proven by `start`); a queue's
        // owner cannot exit (and so stays live) without first disowning the queue, which requires
        // that lock.
        let t: &Thread = unsafe { t.as_ref() };

        // SAFETY: `t` is the first node downstream of `start.owner()`, so (per our caller) its
        // lock is not held, and neither are the locks downstream of it.
        unsafe { Self::lock_pi_chain_common_refuse_cycle(PiChainNodeRef::Thread(t), clt_token) }
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
    #[inline]
    pub unsafe fn lock_pi_chain(
        start: &mut ChainLockProof<'_, Self>,
        clt_token: &ChainLockTransactionToken,
    ) -> ChainLockResult {
        assert_transaction_active(clt_token);
        // The owner is read directly through `start` (rather than by downcasting a
        // `WaitQueueBase`), which is equivalent to `lock_pi_chain_from_wait_queue` for an
        // `OwnedWaitQueue`.
        let Some(t) = start.owner_thread() else {
            return ChainLockResult::Ok;
        };
        // SAFETY: `t` owns `start.owner()`, whose lock is held (proven by `start`); a queue's
        // owner cannot exit (and so stays live) without first disowning the queue, which requires
        // that lock.
        let t: &Thread = unsafe { t.as_ref() };

        // SAFETY: `t` is the first node downstream of `start.owner()`, so (per our caller) its
        // lock is not held, and neither are the locks downstream of it.
        unsafe { Self::lock_pi_chain_common_refuse_cycle(PiChainNodeRef::Thread(t), clt_token) }
    }

    /// Common handler for PI chain locking, starting from (and including) the node `start`.
    ///
    /// Returns `Err(ChainLockResult::Ok)` if the entire chain was locked, `Err(result)` if the
    /// operation failed (after unlocking any locks obtained by this call), or `Ok(stop_point)` if
    /// `B` is [`StopAtCycle`] and a cycle was detected at `stop_point` (leaving the path up to
    /// `stop_point` locked).
    ///
    /// # Safety
    ///
    /// See [`Self::lock_pi_chain_from_node`].
    #[inline]
    unsafe fn lock_pi_chain_common<B: LockingBehaviorTag, S: PiChainNode>(
        start: &S,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<*const c_void, ChainLockResult> {
        // SAFETY: Forwarded from the caller (`pi_chain_node_ref` returns `start` itself, per the
        // `PiChainNode` safety requirements).
        unsafe { Self::lock_pi_chain_from_node::<B>(start.pi_chain_node_ref(), clt_token) }
    }

    /// Common handler for PI chain locking, starting from (and including) the node `start`. See
    /// [`Self::lock_pi_chain_common`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not been finalized.
    /// - `start` is live, and is the first node this call attempts to lock. If `B` is
    ///   [`RefuseCycle`], `start`'s lock must *not* be held by the active transaction
    ///   (encountering a held lock downstream of it fails with [`ChainLockResult::Cycle`], after
    ///   releasing every lock this call acquired). If `B` is [`StopAtCycle`], the lock of `start`
    ///   or of any node downstream of it may already be held by the active transaction: the walk
    ///   stops at the first such node (without acquiring it), and returns it as the stop point.
    /// - If `start` is a wait queue whose magic is [`OWNED_MAGIC`], its pointer meets the
    ///   requirements of [`Self::downcast_ptr`].
    /// - No proofs or guards for the nodes in `start`'s PI chain are used during the call.
    /// - If this returns `Err(ChainLockResult::Ok)`, or `Ok(stop_point)`, the caller owns the
    ///   locks acquired by this call (the path from `start` up to, but excluding, `stop_point`),
    ///   and must eventually release them (for example, with [`Self::unlock_pi_chain_common`])
    ///   before the transaction ends.
    unsafe fn lock_pi_chain_from_node<B: LockingBehaviorTag>(
        start: PiChainNodeRef<'_>,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<*const c_void, ChainLockResult> {
        let stop_at_cycle = B::BEHAVIOR == LockingBehavior::StopAtCycle;
        let mut next_thread: Option<&Thread> = None;
        let mut next_wq: Option<NonNull<WaitQueueBase>> = None;
        let mut skip_thread_lock = false;

        match start {
            PiChainNodeRef::Thread(start) => next_thread = Some(start),
            PiChainNodeRef::WaitQueue(start) => {
                next_wq = Some(start);
                skip_thread_lock = true;
            }
        }

        loop {
            if !skip_thread_lock {
                // If we hit the end of the chain, we are done.
                let Some(thread) = next_thread else {
                    return Err(ChainLockResult::Ok);
                };

                // Try to lock the next thread; if we fail, implement the specified locking
                // behavior.  We always propagate Backoff error codes.  In the case of a
                // detected cycle, we either propagate the error, or we leave the current
                // path locked and report the location of the cycle in our return code.
                // SAFETY: No proof or guard for `thread` is live.
                if let Err(result) = unsafe { raw_lock(thread) }.acquire_or_result(clt_token) {
                    if stop_at_cycle && result == ChainLockResult::Cycle {
                        return Ok(core::ptr::from_ref(thread).cast());
                    }

                    // SAFETY: `start` is live (per our caller).
                    if unsafe { start.is_held(clt_token) } {
                        // SAFETY: `start`'s lock and the path up to `thread` are held, and
                        // `start` meets the provenance requirements (per our caller).
                        unsafe {
                            Self::unlock_pi_chain_from_node(
                                start,
                                core::ptr::from_ref(thread).cast(),
                                clt_token,
                            );
                        }
                    }

                    return Err(result);
                }
                // We just checked for success, and now hold the thread's lock.
                // SAFETY: `thread`'s lock was just acquired.
                let thread_proof = unsafe { assume_locked(thread) };
                let state = thread_proof.wait_queue_state();
                next_wq = state.blocking_wait_queue(thread_proof.token());
            }
            skip_thread_lock = false;

            // If we hit the end of the chain, we are done.
            let Some(wq) = next_wq else {
                return Err(ChainLockResult::Ok);
            };
            let wq_stop_point: *const c_void = wq.as_ptr().cast_const().cast();

            // SAFETY: `wq` is either `start` (live, per our caller) or the blocking wait queue of
            // the previous (locked) thread, which is live while that thread is blocked in it.
            let wq_ref: &WaitQueueBase = unsafe { wq.as_ref() };
            // SAFETY: No proof or guard for `wq` is live.
            if let Err(result) = unsafe { raw_lock(wq_ref) }.acquire_or_result(clt_token) {
                if stop_at_cycle && result == ChainLockResult::Cycle {
                    return Ok(wq_stop_point);
                }

                // SAFETY: `start` is live (per our caller).
                if unsafe { start.is_held(clt_token) } {
                    // SAFETY: `start`'s lock and the path up to `wq` are held, and `start` meets
                    // the provenance requirements (per our caller).
                    unsafe { Self::unlock_pi_chain_from_node(start, wq_stop_point, clt_token) };
                }

                return Err(result);
            }

            // SAFETY: `wq`'s lock was just acquired, and it is either `start` (which meets the
            // provenance requirements, per our caller) or a blocking wait queue (which carries
            // provenance over the enclosing `OwnedWaitQueue`, per the `blocking_wait_queue` safety
            // invariant).
            let owner = unsafe { Self::get_queue_owner(wq, clt_token) };
            // SAFETY: The owner of `wq` stays live while `wq`'s lock (just acquired, and held
            // until the chain is unlocked) is held, since an owner must disown the queue (which
            // requires that lock) before exiting.
            next_thread = owner.map(|t| unsafe { t.as_ref() });
            next_wq = None;
        }
    }

    /// Locks the rest of the PI chain starting from (and including) `start`, refusing cycles.
    ///
    /// # Safety
    ///
    /// See [`Self::lock_pi_chain_from_node`].
    #[inline]
    unsafe fn lock_pi_chain_common_refuse_cycle(
        start: PiChainNodeRef<'_>,
        clt_token: &ChainLockTransactionToken,
    ) -> ChainLockResult {
        // SAFETY: Forwarded from the caller.
        unsafe { Self::lock_pi_chain_from_node::<RefuseCycle>(start, clt_token) }.unwrap_err()
    }

    /// Common handler for PI chain unlocking. Releases `start`'s lock, and the locks of the nodes
    /// downstream of `start` which are held, stopping at `stop_point` (if non-null).
    ///
    /// # Safety
    ///
    /// See [`Self::unlock_pi_chain_from_node`].
    #[inline]
    pub(crate) unsafe fn unlock_pi_chain_common<S: PiChainNode>(
        start: &S,
        stop_point: *const c_void,
        clt_token: &ChainLockTransactionToken,
    ) {
        let start = start.pi_chain_node_ref();
        // SAFETY: Forwarded from the caller (`pi_chain_node_ref` returns `start` itself, per the
        // `PiChainNode` safety requirements).
        unsafe { Self::unlock_pi_chain_from_node(start, stop_point, clt_token) }
    }

    /// Common handler for PI chain unlocking. See [`Self::unlock_pi_chain_common`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that `start` is live, that `start`'s lock, and the locks of the held
    /// nodes downstream of `start` (up to `stop_point`, if non-null), are held by the active
    /// transaction, that no proofs or guards for those nodes are used during or after the call,
    /// and that if `start` is a wait queue whose magic is [`OWNED_MAGIC`], its pointer meets the
    /// requirements of [`Self::downcast_ptr`].
    unsafe fn unlock_pi_chain_from_node(
        start: PiChainNodeRef<'_>,
        stop_point: *const c_void,
        clt_token: &ChainLockTransactionToken,
    ) {
        let mut next_thread: Option<&Thread> = None;
        let mut next_wq: Option<NonNull<WaitQueueBase>>;

        let should_stop_thread = |t: Option<&Thread>| -> bool {
            if !stop_point.is_null() {
                let stop_point_match = t.is_some_and(|t| core::ptr::addr_eq(stop_point, t));
                debug_assert!(t.is_some());
                debug_assert!(stop_point_match || t.is_some_and(|t| t.lock().is_held(clt_token)));
                stop_point_match
            } else {
                t.is_none_or(|t| !t.lock().is_held(clt_token))
            }
        };

        // `wq` (if any) must be live: it is the blocking wait queue of a thread whose lock is
        // held, and this is called before that lock is released.
        let should_stop_wait_queue = |wq: Option<NonNull<WaitQueueBase>>| -> bool {
            // SAFETY: `wq` is live (see above); the reference is only used to read its lock.
            let wq: Option<&WaitQueueBase> = wq.map(|wq| unsafe { wq.as_ref() });
            if !stop_point.is_null() {
                let stop_point_match = wq.is_some_and(|wq| core::ptr::addr_eq(stop_point, wq));
                debug_assert!(wq.is_some());
                debug_assert!(
                    stop_point_match || wq.is_some_and(|wq| wq.lock().is_held(clt_token))
                );
                stop_point_match
            } else {
                wq.is_none_or(|wq| !wq.lock().is_held(clt_token))
            }
        };

        // Returns the blocking wait queue of `thread`.
        //
        // # Safety
        //
        // `thread`'s lock must be held by the active transaction, with no other proof or guard for
        // `thread` used during the call.
        #[inline]
        unsafe fn blocking_wait_queue_of(thread: &Thread) -> Option<NonNull<WaitQueueBase>> {
            // SAFETY: Forwarded from the caller.
            let thread_proof = unsafe { assume_locked(thread) };
            thread_proof.wait_queue_state().blocking_wait_queue(thread_proof.token())
        }

        // We must currently be holding the starting node's lock.  If we found our
        // stopping point, have no next node, or we are not currently holding the next
        // node's lock, we are done.
        debug_assert!(!core::ptr::addr_eq(start.as_ptr(), stop_point));
        match start {
            PiChainNodeRef::Thread(start_thread) => {
                // SAFETY: The caller guarantees `start`'s lock is held.
                next_wq = unsafe { blocking_wait_queue_of(start_thread) };

                // Make sure that we check our stop condition before drop our current node
                // lock (here and below).  Otherwise, it is possible that the next node is
                // not locked, and can go away as soon as we drop the lock on our upstream
                // node (here and below).
                let stop = should_stop_wait_queue(next_wq);
                // SAFETY: The caller guarantees `start`'s lock is held and unused elsewhere.
                unsafe { raw_lock(start_thread) }.release(clt_token);
                if stop {
                    return;
                }
            }
            PiChainNodeRef::WaitQueue(start_wq) => {
                // SAFETY: The caller guarantees `start_wq`'s lock is held and the requirements of
                // `downcast_ptr`.
                let owner = unsafe { Self::get_queue_owner(start_wq, clt_token) };
                // SAFETY: The owner is live while `start_wq`'s lock is held (an owner must disown
                // the queue, which requires that lock, before exiting). It is only used after
                // that lock is released if `should_stop_thread` found its own lock held.
                next_thread = owner.map(|t| unsafe { t.as_ref() });
                next_wq = None;
                let stop = should_stop_thread(next_thread);
                // SAFETY: The caller guarantees `start` is live, and that its lock is held and
                // unused elsewhere.
                unsafe { raw_lock(start_wq.as_ref()) }.release(clt_token);
                if stop {
                    return;
                }
            }
        }

        loop {
            // The first time entering the loop, the next thread may be `None` if the
            // initial node was a wait queue. After that, there should always be a next
            // thread, and it should be locked (as we always check `is_held` before
            // releasing the previous node's lock).
            if let Some(thread) = next_thread {
                // If we do not have a next node, or that next node is not locked, we can
                // drop the thread's lock and get out.  Note: it is important to make the
                // check to see if the next node is locked before dropping the thread's
                // lock.  Otherwise, it would be possible for us to realize that we have a
                // next node, drop the thread's lock, then have that next node destroyed
                // before we are able to check to see if we have it locked.
                // SAFETY: `thread`'s lock is held (checked before releasing the previous node's
                // lock).
                next_wq = unsafe { blocking_wait_queue_of(thread) };
                {
                    let stop = should_stop_wait_queue(next_wq);
                    // SAFETY: `thread`'s lock is held (checked before releasing the previous
                    // node's lock) and unused elsewhere.
                    unsafe { raw_lock(thread) }.release(clt_token);
                    if stop {
                        return;
                    }
                }
            }

            // At this point, there should always be a next wait queue, and it should
            // always be locked (as we always check `is_held` before releasing the
            // previous node's lock).
            let Some(wq) = next_wq else {
                unreachable!("PI chain unlock reached a thread with no blocking wait queue");
            };

            // See the note above, we need to check to see if there is a next node, and
            // that we have it locked _before_ dropping the locked WQ's lock.
            // SAFETY: `wq`'s lock is held, and blocking wait queues carry provenance over the
            // enclosing `OwnedWaitQueue` if their magic is `OWNED_MAGIC` (`blocking_wait_queue`
            // safety invariant).
            let owner = unsafe { Self::get_queue_owner(wq, clt_token) };
            // SAFETY: The owner is live while `wq`'s lock is held (an owner must disown the queue,
            // which requires that lock, before exiting). It is only used after that lock is
            // released if `should_stop_thread` found its own lock held.
            next_thread = owner.map(|t| unsafe { t.as_ref() });
            {
                let stop = should_stop_thread(next_thread);
                // SAFETY: `wq` is live and its lock is held (checked before releasing the
                // previous node's lock), and unused elsewhere.
                unsafe { raw_lock(wq.as_ref()) }.release(clt_token);
                next_wq = None;
                if stop {
                    return;
                }
            }
        }
    }

    /// Changes a thread's base profile and deals with profile propagation side effects.
    pub fn set_thread_base_profile_and_propagate(thread: &Thread, profile: &BaseProfile) {
        assert_no_transaction_active();
        // To change this thread's base profile, we need to start by locking the
        // entire PI chain starting from this thread.
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the
            // duration of this transaction.
            let preempt_disabled = unsafe { PreemptDisabledToken::new() };
            let mut thread_guard = OwnerGuard::new(thread, token);
            let wq = {
                let thread_proof = thread_guard.proof();
                thread_proof.wait_queue_state().blocking_wait_queue(thread_proof.token())
            };
            // SAFETY: A thread's blocking wait queue is live while the thread is blocked in it
            // and the thread's lock is held, and it carries provenance over the enclosing
            // `OwnedWaitQueue` if its magic is `OWNED_MAGIC` (`blocking_wait_queue` safety
            // invariant).
            let owq = wq.and_then(|wq| unsafe { Self::downcast_ptr(wq) });
            // SAFETY: As above, the queue is live while `thread` is blocked in it, and `thread`'s
            // lock is held until the end of this transaction.
            let owq: Option<&OwnedWaitQueue> = owq.map(|owq| unsafe { owq.as_ref() });

            // Lock the rest of the PI chain, restarting if we need to.
            // SAFETY: The transaction is not finalized and no downstream locks are held. On
            // success, the chain is unlocked below.
            if unsafe { Self::lock_pi_chain_from_thread(&mut thread_guard.proof(), token) }
                == ChainLockResult::Backoff
            {
                thread_guard.release();
                return Action::Backoff.into();
            }

            // Finished obtaining locks.  We can now propagate.
            ChainLockTransaction::finalize(token);

            // If our thread is blocked in an owned wait queue, we need observe the
            // thread's transmitted IPVs before and after the base profile change in order
            // to properly handle propagation.
            let mut old_ipvs = InheritedProfileValues::default();
            if owq.is_some() {
                old_ipvs = Self::snapshot_thread_ipv(&thread_guard.proof(), ForceInheritance::No);
            }

            // Regardless of the state of the thread whose base profile has changed, we
            // need to update the base profile and let the scheduler know.  The scheduler
            // code will handle:
            // 1) Updating our effective profile
            // 2) Repositioning us in our wait queue (if we are blocked)
            // 3) Updating our dynamic scheduling parameters (if we are either runnable or
            //    a blocked deadline thread)
            // 4) Updating the scheduler's state (if we happen to be a runnable thread).
            {
                let mut thread_proof = thread_guard.proof();
                *thread_proof.base_profile_mut() = *profile;
                thread_proof.effective_profile_mut().mark_base_profile_changed();
            }
            // SAFETY: The entire PI chain starting from `thread` is locked, the transaction is
            // finalized, and `thread`'s base profile was just changed.
            unsafe {
                scheduler::thread_base_profile_changed(thread, token);
            }

            // Now, if we are blocked in an owned wait queue, propagate the consequences
            // of the base profile change downstream.
            if let Some(owq) = owq {
                owq.lock().assert_held(token);
                let new_ipvs =
                    Self::snapshot_thread_ipv(&thread_guard.proof(), ForceInheritance::No);
                // SAFETY: The entire PI chain starting from `thread` is locked, and no proofs or
                // guards for its nodes are used during the call.
                unsafe {
                    Self::finish_propagate(
                        thread,
                        owq,
                        Some(&new_ipvs),
                        Some(&old_ipvs),
                        BASE_PROFILE_CHANGED_OP,
                        ForceInheritance::No,
                        None,
                        token,
                        &preempt_disabled,
                    );
                }
            }

            // Finished, drop the locks we are holding and make sure we break out of the
            // retry loop.
            let thread = thread_guard.into_unguarded();
            // SAFETY: The entire PI chain starting from `thread` is held, and `thread_guard` has
            // been disarmed.
            unsafe {
                Self::unlock_pi_chain_common(thread, core::ptr::null(), token);
            }
            DONE.into()
        };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::SetThreadBaseProfileAndPropagate"),
            do_transaction,
        );
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
        // SAFETY: `self`'s lock need not be held. The caller guarantees that `self.owner` cannot
        // be mutated for the duration of this call (`self.lock()` is held, or the queue is
        // quiescent in a pool), and `proof` is only used to read it.
        let proof = unsafe { assume_synchronized_read_only(self) };
        debug_assert!(proof.owner_thread().is_none());
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
        // SAFETY: `self`'s lock need not be held. The caller guarantees that `self` cannot be
        // mutated for the duration of this call (`self.lock()` is held, or the queue is quiescent
        // in a pool), and `proof` is only used to read `owner` and the collection.
        let proof = unsafe { assume_synchronized_read_only(self) };
        debug_assert!(
            proof.owner_thread().is_none(),
            "Cannot recycle an OwnedWaitQueue that has an owner."
        );
        debug_assert!(proof.is_empty(), "Cannot recycle an OwnedWaitQueue that has waiters.");
        debug_assert!(
            !self.dll_node.in_container(),
            "Cannot recycle an OwnedWaitQueue that is owned by a thread."
        );
    }
}

/// Locked operations for an [`OwnedWaitQueue`] guarded by its lock.
impl ChainLockProof<'_, OwnedWaitQueue> {
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
    #[inline]
    pub(crate) unsafe fn try_lock_for_baao_operation_locked<'a>(
        &mut self,
        current_thread: &'a Thread,
        new_owner: Option<&Thread>,
        clt_token: &'a ChainLockTransactionToken,
    ) -> Option<(BAAOLockingDetails, OwnerGuard<'a, Thread>)> {
        assert_transaction_active(clt_token);
        debug_assert_lock_not_held(current_thread.lock());
        debug_assert!(core::ptr::eq(
            current_thread.as_raw(),
            // SAFETY: Multi-threading is initialized when blocking a thread.
            unsafe { ThreadPtr::current() }.as_raw()
        ));

        // Start by trying to obtain the current thread's lock.
        let current_thread_guard = OwnerGuard::acquire_or_backoff(current_thread, clt_token)?;

        // Now attempt to lock the rest of the old owner/new owner chain, backing
        // off if we have to.  This common code path with automatically deal with
        // nack'ing the proposed new owner when appropriate.
        // SAFETY: `self` proves this queue's lock is held, the transaction is not finalized, the
        // caller guarantees no other PI chain locks are held (`current_thread`, the blocking
        // thread, was just locked above), and `new_owner` is a live thread.
        let result = unsafe {
            self.lock_for_owner_replacement(
                new_owner,
                Some(ThreadPtr::from(current_thread)),
                false,
                false,
                clt_token,
            )
        };
        let details = match result {
            Err(lock_res) => {
                // The only valid lock result we can get back from this operation is
                // Backoff.  `lock_for_owner_replacement` should handle any cycles, and if the
                // locking operation succeeds, it is going to return locking details
                // instead of an "OK" `ChainLockResult`.
                debug_assert!(lock_res == ChainLockResult::Backoff);
                current_thread_guard.release();
                return None;
            }
            Ok(details) => details,
        };

        // Success, return the locking details.
        Some((details, current_thread_guard))
    }

    /// Obtains the locks needed to replace this queue's owner with `new_owner_in`, potentially
    /// rejecting (replacing with `None`) the proposed new owner if it would form a cycle or is not
    /// allowed to own wait queues.
    ///
    /// Returns:
    /// - `Ok(details)` on success, holding the locks of the PI chains of the validated new owner
    ///   (`details.new_owner`, which is `None` if the proposal was rejected) and of the current
    ///   owner (up to `details.stop_point`, if non-null). The caller must eventually release them.
    /// - `Err(ChainLockResult::Backoff)` if the transaction must back off, after releasing every
    ///   lock acquired by this call.
    /// - `Err(ChainLockResult::Cycle)` (only if `propagate_new_owner_cycle_error`) if the proposed
    ///   new owner would form a cycle, holding no locks acquired by this call.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the active transaction is not finalized, and that no locks in
    /// the PI chains of this queue's current owner or `new_owner_in` are held (other than
    /// `new_owner_in`'s own lock if `new_owner_is_locked`, and `blocking_thread`'s lock, if any).
    unsafe fn lock_for_owner_replacement(
        &mut self,
        new_owner_in: Option<&Thread>,
        blocking_thread: Option<ThreadPtr>,
        propagate_new_owner_cycle_error: bool,
        new_owner_is_locked: bool,
        clt_token: &ChainLockTransactionToken,
    ) -> Result<ReplaceOwnerLockingDetails, ChainLockResult> {
        // We are holding the queue lock, and we have a proposed new owner.  We need
        // to hold all of appropriate locks, and potentially nak the new owner
        // proposal.
        //
        // Start with the most trivial reason we might disallow a chosen new owner.
        // The new owner of a wait queue cannot be the same as the thread who is
        // blocking as this would obviously generate an immediate cycle.  We can
        // trivially check for that and rule it out first thing.
        //
        // After that, we need to consider a few different special cases.
        //
        // In each of these cases, it is possible for us to detect a cycle which would
        // be created if the operation were allowed to proceed as requested.
        // Currently, we do not allow these operations to fail (users must always be
        // able to block their threads), and we do not have a defined job policy which
        // would allow us to terminate a process which attempts to create a PI cycle.
        // So, for now, instead we simply change the new owner to become "nothing",
        // preventing the cycle from forming in the first place.
        //
        // Case 1 : new owner == old owner
        //
        // If the old owner and the new owner are the same, we simply attempt to
        // lock the path starting from the current owner.  If this detects a cycle,
        // we will start again, but this time with new owner == None != old
        // owner.  This can happen if there is a thread blocking which the new owner
        // is currently blocked upstream from.  For example, in the diagram below, if
        // T3 attempts to block in Q1, declaring T1 to be the queue owner.
        //
        // +----+     +----+     +----+     +----+     +----+     +----+
        // | Q1 | --> | T1 | --> | Q2 | --> | T2 | --> | Q3 | --> | T3 |
        // +----+     +----+     +----+     +----+     +----+     +----+
        //
        // Case 2 : new owner != old owner && new owner != None
        //
        // We are either adding a new owner, or replacing an exiting owner with a
        // new one.  Start by locking the new owner path.  If this detects a cycle,
        // we set the newly proposed owner to None and start again.  Otherwise,
        // once the new owner path is locked, we proceed as in case #2 but with a
        // small modification.  If following the old owner path leads to a detected
        // cycle, it could be for one of two reasons.  If this is a BAAO operation, it
        // may be that the addition of the blocking thread to the target wait queue
        // would introduce a cycle (as in case #2), or it could be that the old owner
        // path converges with the new owner path.  Neither one of these cases is
        // illegal; we just need to stop locking the old path when we hit this
        // intersection point, and remember that when it comes time to unlock the old
        // path after removing the old owner, we need to stop when we encounter this
        // intersection point.
        //
        // Case 3 : new owner != old owner && new owner == None
        //
        // If we are removing an existing owner, then we can simply lock the old owner
        // path, tolerating any cycles we encounter (but marking where they are) in
        // the process.  If we end up detecting a cycle, it can only be because we are
        // involved in a BAAO operation, and the old owner is was blocked behind the
        // thread who is currently blocking.  All we need to do is assert this, and
        // make sure that when we are unlocking the old owner path that we stop at the
        // blocking thread and do not go any further.
        //
        // If we detect a cycle for any other reason, there must be something wrong.
        // The old owner cannot be involved in a path which does not pass through a
        // thread which is blocking.  If it were, it would imply that there was
        // already a cycle in the graph before the proposed BAAO operation (which
        // would violate the PI graph invariants).

        let mut details = ReplaceOwnerLockingDetails::new(new_owner_in.map(ThreadPtr::from));
        let new_owner = &mut details.new_owner;
        let old_owner = self.owner_thread();
        let old_owner_is_blocking = blocking_thread == old_owner;
        let this_ptr: *const c_void = core::ptr::from_ref(self.owner()).cast();

        // Disallow the trivial case of "the new owner cannot also be the blocking
        // thread" before getting in handling the more complicated cases.
        if *new_owner == blocking_thread {
            *new_owner = None;
        }

        // Case 1, try to validate our existing owner (if any)
        if *new_owner == old_owner {
            // If we have no owner, and we are not assigning a new owner, then we are
            // done.
            let Some(owner) = *new_owner else {
                return Ok(details);
            };
            // SAFETY: The owner of a queue is a live thread which cannot change while this
            // queue's lock is held.
            let owner: &Thread = unsafe { owner.as_ref() };
            // If the new owner is locked, then we are done.
            debug_assert!(!new_owner_is_locked);

            // Lock the chain starting from the current owner.  This is case 1 where the old
            // owner and proposed new owner are the same.  Since the new owner cannot be
            // the blocking thread, this means that we can be sure that the old owner is
            // also not the blocking thread.
            debug_assert!(!old_owner_is_blocking);
            // SAFETY: `owner` is live (see above), and the transaction is not finalized. With
            // `StopAtCycle`, any lock which is already held (such as the blocking thread's) is
            // reported as the stop point rather than acquired.
            let result = unsafe {
                OwnedWaitQueue::lock_pi_chain_common::<StopAtCycle, Thread>(owner, clt_token)
            };
            let mut stop_point: *const c_void = core::ptr::null();

            // If we didn't detect a cycle, and the existing owner is allowed to own
            // wait queues (not in the process of exiting) we are finished.  We either
            // succeeded and can simply get out holding the locks we have, or we failed
            // an need to back-off (which we can do immediately since we didn't obtain
            // any new locks).
            match result {
                Err(lock_res) => {
                    if lock_res != ChainLockResult::Ok {
                        debug_assert!(lock_res == ChainLockResult::Backoff);
                        return Err(lock_res);
                    }

                    owner.lock().assert_held(clt_token);
                    // SAFETY: `owner`'s lock was just acquired.
                    if unsafe { assume_locked(owner) }.can_own_wait_queues() {
                        return Ok(details);
                    }
                }
                Ok(cycle_stop_point) => {
                    // We found a cycle which would have been formed by attempting to block a
                    // thread who is currently downstream of this queue's owner.  Assert this,
                    // then drop the locks we acquired and force the new owner to be None in
                    // order to break the cycle.  Then proceed to the new_owner != old_owner
                    // logic (since, they no longer match).
                    //
                    // Note: we may not have locked anything at all.  It is possible that
                    // someone attempted to block this queue's current owner in the queue
                    // itself.  We need to make sure to check for this special case before
                    // calling `unlock_pi_chain_common`.
                    stop_point = cycle_stop_point;
                    debug_assert!(
                        stop_point == thread_stop_point(blocking_thread),
                        "Detected cycle point is not the blocking thread.  (BT {:?} this {:p} \
                         stop_point {:p})",
                        blocking_thread,
                        this_ptr,
                        stop_point
                    );
                }
            }

            // If we made it to this point, we either detected an unacceptable cycle, or
            // our new owner was rejected because it is exiting and is not allowed to
            // take ownership of the queue.  Unlock any locks that we held, reject the
            // new owner proposal, and fall into case 3.
            if owner.lock().is_held(clt_token) {
                // SAFETY: `owner`'s chain up to `stop_point` was locked above.
                unsafe { OwnedWaitQueue::unlock_pi_chain_common(owner, stop_point, clt_token) };
            }
            *new_owner = None;
        }

        // If we made it this far, we must be in case 2 or 3 (the owner is changing)
        debug_assert!(*new_owner != old_owner);

        // Case 2, we are replacing the old owner (if any) with a new one;
        if let Some(new_owner_ref) = new_owner_in.filter(|_| new_owner.is_some()) {
            // `new_owner` is only ever reset to `None`, so if it is `Some`, it is `new_owner_in`.
            debug_assert!(same_thread(*new_owner, Some(new_owner_ref)));
            // Start by attempting to lock the new owner path.
            let new_owner_lock_res = if !new_owner_is_locked {
                // SAFETY: `new_owner_ref` is live, the transaction is not finalized, and (per our
                // caller) its lock is not held, since `new_owner_is_locked` is false and it is not
                // the blocking thread (checked above).
                unsafe {
                    OwnedWaitQueue::lock_pi_chain_common::<RefuseCycle, Thread>(
                        new_owner_ref,
                        clt_token,
                    )
                }
                .unwrap_err()
            } else {
                ChainLockResult::Ok
            };

            if new_owner_lock_res == ChainLockResult::Ok {
                // Things went well and we got the locks.  Check to make sure that our new
                // owner is actually allowed to own wait queues before proceeding.
                let can_own_wait_queues = {
                    new_owner_ref.lock().assert_held(clt_token);
                    // SAFETY: `new_owner_ref`'s lock is held.
                    unsafe { assume_locked(new_owner_ref) }.can_own_wait_queues()
                };
                if !can_own_wait_queues {
                    // Our new owner is not allowed to own queues.  Unlock it, and reject the
                    // proposal.
                    if !new_owner_is_locked {
                        // SAFETY: `new_owner_ref`'s chain was locked above.
                        unsafe {
                            OwnedWaitQueue::unlock_pi_chain_common(
                                new_owner_ref,
                                core::ptr::null(),
                                clt_token,
                            );
                        }
                    }
                    *new_owner = None;
                    if old_owner.is_none() {
                        return Ok(details);
                    }
                }

                // We now either hold the new owner chain, or have rejected the new owned
                // because it is not allowed to own queues.  If there is no current owner
                // we are finished.
                let Some(old_owner_ptr) = old_owner else {
                    return Ok(details);
                };

                // If we didn't reject the new owner, we now need to lock the old owner
                // chain.
                if new_owner.is_some() {
                    // Lock the old owner path, but tolerate a detected cycle.  There are
                    // two special edge cases to consider here:
                    //
                    // First, the old owner may be the thread who is blocking (and is
                    // declaring a different owner in the process).  The blocking thread is
                    // currently running, and therefore cannot itself be blocked (there is
                    // nothing downstream of it), and we already hold its lock.  So, if the
                    // blocking thread is the same as the old owner, there are no new locks
                    // to obtain. We just need to specify the blocking thread as the unlock
                    // stopping point for the operation.
                    //
                    // Second, while locking the old_owner path (old owner != blocking
                    // thread), we may run into the blocking thread (as in case 2), or we
                    // may run into a node which is also on the new owner path.  Either is
                    // OK since we are replacing the old owner with a (now validated) new
                    // one, we just need to remember where to stop unlocking after we have
                    // replaced the old owner with a new one.
                    let result = if !old_owner_is_blocking {
                        // SAFETY: The owner of a queue is a live thread which cannot change while
                        // this queue's lock is held, and the transaction is not finalized. Its
                        // lock is not held (per our caller, since it is not the blocking thread),
                        // and with `StopAtCycle`, any held downstream lock (on the new owner path,
                        // or the blocking thread's) is reported as the stop point.
                        unsafe {
                            OwnedWaitQueue::lock_pi_chain_common::<StopAtCycle, Thread>(
                                old_owner_ptr.as_ref(),
                                clt_token,
                            )
                        }
                    } else {
                        Ok(thread_stop_point(blocking_thread))
                    };
                    match result {
                        Err(old_owner_lock_res) => {
                            // Since we chose StopAtCycle behavior for locking the old_owner path,
                            // we know the result cannot be Cycle.  It is either OK (and
                            // we are done) or Backoff (and we need to try again).
                            debug_assert!(old_owner_lock_res != ChainLockResult::Cycle);
                            if old_owner_lock_res == ChainLockResult::Ok {
                                // Things went well, we are done.
                                return Ok(details);
                            }

                            // Need to back off and try again.  Drop the locks we were holding
                            // before restarting.
                            debug_assert!(old_owner_lock_res == ChainLockResult::Backoff);
                            if !new_owner_is_locked {
                                new_owner_ref.lock().assert_acquired(clt_token);
                                // SAFETY: `new_owner_ref`'s chain was locked above.
                                unsafe {
                                    OwnedWaitQueue::unlock_pi_chain_common(
                                        new_owner_ref,
                                        core::ptr::null(),
                                        clt_token,
                                    );
                                }
                            }
                            return Err(old_owner_lock_res);
                        }
                        Ok(stop_point) => {
                            // Success (although our paths intersect in some way).  Record our
                            // stopping point and get out.
                            details.stop_point = stop_point;
                            debug_assert!(details.stop_point != this_ptr);
                            return Ok(details);
                        }
                    }
                }
            } else if new_owner_lock_res == ChainLockResult::Backoff {
                // Need to back off and try again.  We should not be holding any new
                // locks, so we can just propagate the error up and out.
                return Err(new_owner_lock_res);
            } else {
                // We found a cycle, so we have to nack this new owner.  If the caller asked us to
                // propagate the error up the stack, we can just do so now.  We are not holding
                // any extra locks (yet). Otherwise, reject the new owner proposal (storing None
                // in the details structure we are going to return) and proceed as if the caller
                // is going remove the current owner entirely, instead of replacing it. If we
                // don't have a current owner, then we are finished.  If we do, we can simply
                // drop into the Case 2 handler (below).
                debug_assert!(new_owner_lock_res == ChainLockResult::Cycle);
                if propagate_new_owner_cycle_error {
                    return Err(ChainLockResult::Cycle);
                }

                *new_owner = None;
                if old_owner.is_none() {
                    return Ok(details);
                }
            }
        }

        // The only option left is that we are now in case 3; we are removing the
        // existing owner. We either started in that situation, or managed to get
        // there via one of our cycle detection mitigations.  Check the old owner path
        // to see if there is a temporary cycle that we need to deal with.
        debug_assert!(new_owner.is_none());
        let old_owner_ptr = old_owner.expect("case 3 requires an existing owner");

        // See case 2 (above).  If the old owner is the blocking thread, there is
        // nothing more to lock, and we need to remember to stop unlocking at the old
        // owner during owner reassignment.
        let result = if !old_owner_is_blocking {
            // SAFETY: The owner of a queue is a live thread which cannot change while this queue's
            // lock is held, and the transaction is not finalized. Its lock is not held (per our
            // caller, since it is not the blocking thread), and with `StopAtCycle`, any held
            // downstream lock (the blocking thread's) is reported as the stop point.
            unsafe {
                OwnedWaitQueue::lock_pi_chain_common::<StopAtCycle, Thread>(
                    old_owner_ptr.as_ref(),
                    clt_token,
                )
            }
        } else {
            Ok(thread_stop_point(blocking_thread))
        };
        let stop_point = match result {
            Err(lock_res) => {
                // Since we chose StopAtCycle behavior for locking the old_owner path, we
                // know the result cannot be Cycle.  It is either OK (and we are
                // done) or Backoff (and we need to try again).
                debug_assert!(lock_res != ChainLockResult::Cycle);
                if lock_res == ChainLockResult::Ok {
                    // Things went well, we are done.
                    return Ok(details);
                }

                // Need to back off and try again.  We are not holding any new locks, so we
                // can just propagate the error.
                debug_assert!(lock_res == ChainLockResult::Backoff);
                return Err(lock_res);
            }
            Ok(stop_point) => stop_point,
        };

        // We must have found a cycle.  If we did, we successfully locked up until
        // the point in the graph where that cycle was detected, and marked that as
        // our stop point.  Since we are removing the old owner, we know that there
        // must be a blocking thread, and the cycle we detected must have been found
        // at that point.  Assert this, then Record our stop point in the results
        // and we are done.
        details.stop_point = stop_point;
        debug_assert!(
            details.stop_point == thread_stop_point(blocking_thread),
            "Detected cycle point is not the blocking thread.  (BT {:?} this {:p} stop_point \
             {:p})",
            blocking_thread,
            this_ptr,
            details.stop_point
        );
        Ok(details)
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
    pub(crate) unsafe fn lock_for_wake_operation_locked<H: IWakeRequeueHook + ?Sized>(
        &mut self,
        max_wake: u32,
        wake_hooks: &mut H,
        unblock_list: Pin<&mut ThreadUnblockList>,
        clt_token: &ChainLockTransactionToken,
    ) -> bool {
        assert_transaction_active(clt_token);
        // TODO(https://fxbug.dev/333747818): Find a good way to assert that
        // `max_wake` is greater than zero while still supporting a zero count coming
        // from the futex APIs (zx_futex_wake and zx_futex_requeue).

        // If we have an owner, locking the path starting from it.  We are either going
        // to set the ownership of this queue to (singular) thread that we wake, or
        // we are going to be removing ownership entirely.  Either way, we need to
        // hold the locks along the path of the existing owner (if any).
        // SAFETY: Forwarded from the caller.
        let lock_res = unsafe { OwnedWaitQueue::lock_pi_chain(self, clt_token) };
        if lock_res == ChainLockResult::Backoff {
            return false;
        }
        debug_assert!(lock_res == ChainLockResult::Ok);

        // Lock as many threads as we can, moving them out of our wait collection
        // and onto a temporary unblock list as we go.
        // SAFETY: Forwarded from the caller.
        let locked = unsafe {
            self.lock_and_make_waiter_list_locked(max_wake, wake_hooks, unblock_list, clt_token)
        };

        // If we failed to lock the set of threads to wake, then we need to back off
        // and try again.  Drop the locks we obtained starting from our owner (if
        // any).
        if !locked {
            if let Some(owner) = self.owner_thread() {
                // SAFETY: The owner of a queue is a live thread which cannot change while this
                // queue's lock is held.
                let owner: &Thread = unsafe { owner.as_ref() };
                owner.lock().assert_acquired(clt_token);
                // SAFETY: The chain starting from `owner` was locked by `lock_pi_chain` above.
                unsafe {
                    OwnedWaitQueue::unlock_pi_chain_common(owner, core::ptr::null(), clt_token);
                }
            }
            return false;
        }

        // Success!  We now have all of the locks we need; the list of threads we have locked and
        // now need to wake has been populated.
        true
    }

    /// Tries to lock a set of up to `max_count` threads to wake, placing them in `res` and back
    /// into the collection on success.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the transaction is not finalized, that no waiter thread locks
    /// are held, and that `res` is empty.
    unsafe fn lock_and_make_waiter_list_locked<H: IWakeRequeueHook + ?Sized>(
        &mut self,
        max_count: u32,
        hooks: &mut H,
        mut res: Pin<&mut ThreadUnblockList>,
        clt_token: &ChainLockTransactionToken,
    ) -> bool {
        // Try to lock a set of threads to wake.  If we succeeded, make sure we place
        // them back into the collection before returning.
        // SAFETY: Forwarded from the caller.
        let locked = unsafe {
            self.try_lock_and_make_waiter_list_locked(max_count, hooks, res.as_mut(), clt_token)
        };
        if locked {
            for t in res.iter() {
                // SAFETY: Every thread in `res` is live and was locked by
                // `try_lock_and_make_waiter_list_locked`.
                let t: &Thread = unsafe { t.as_ref() };
                t.lock().assert_held(clt_token);
                // SAFETY: `t` was removed from the collection by
                // `try_lock_and_make_waiter_list_locked` and is being put back.
                unsafe {
                    self.collection_mut().insert(&mut assume_locked(t));
                }
            }
        }

        locked
    }

    /// Tries to lock the sets of threads to wake and to requeue, placing them in `ret` and back
    /// into the collection on success.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the transaction is not finalized, that no waiter thread locks
    /// are held, and that the lists in `ret` are empty.
    #[allow(clippy::too_many_arguments)]
    unsafe fn lock_and_make_wake_requeue_thread_lists_locked<
        WH: IWakeRequeueHook + ?Sized,
        RH: IWakeRequeueHook + ?Sized,
    >(
        &mut self,
        max_wake_count: u32,
        wake_hooks: &mut WH,
        max_requeue_count: u32,
        requeue_hooks: &mut RH,
        ret: Pin<&mut WakeRequeueThreadDetails>,
        clt_token: &ChainLockTransactionToken,
    ) -> bool {
        let mut ret = ret.project();
        // Start by trying to lock the set of threads to wake.
        // SAFETY: Forwarded from the caller.
        if !unsafe {
            self.try_lock_and_make_waiter_list_locked(
                max_wake_count,
                wake_hooks,
                ret.wake_threads.as_mut(),
                clt_token,
            )
        } {
            return false;
        }

        // Success!  The threads we have selected for waking have been temporarily
        // removed from the wake-queue, allowing us to now select the set of threads
        // to requeue.  If we fail, make sure we unlock the threads we selected for
        // wake and return them to the collection.
        // SAFETY: Forwarded from the caller.
        if !unsafe {
            self.try_lock_and_make_waiter_list_locked(
                max_requeue_count,
                requeue_hooks,
                ret.requeue_threads.as_mut(),
                clt_token,
            )
        } {
            // SAFETY: The threads in `ret.wake_threads` were locked and removed from the
            // collection above.
            unsafe {
                self.unlock_and_clear_waiter_list_locked(ret.wake_threads.as_mut(), clt_token)
            };
            return false;
        }

        // Success, we have managed to lock both sets of threads.  Add them back to
        // the collection before proceeding.
        for list in [ret.wake_threads.as_ref(), ret.requeue_threads.as_ref()] {
            for t in list.iter() {
                // SAFETY: Every thread in `list` is live and was locked above.
                let t: &Thread = unsafe { t.as_ref() };
                t.lock().assert_held(clt_token);
                // SAFETY: `t` was removed from the collection above and is being put back.
                unsafe {
                    self.collection_mut().insert(&mut assume_locked(t));
                }
            }
        }

        true
    }

    /// Tries to lock up to `max_count` threads, removing them from the collection and placing them
    /// on `list`. On failure, every thread this call placed on `list` is unlocked and returned to
    /// the collection, and `list` is left empty.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the transaction is not finalized, that no waiter thread locks
    /// are held, that `list` is empty on entry, and that on success, the threads moved onto `list`
    /// are returned to the collection before this queue's lock is released.
    unsafe fn try_lock_and_make_waiter_list_locked<H: IWakeRequeueHook + ?Sized>(
        &mut self,
        max_count: u32,
        hooks: &mut H,
        mut list: Pin<&mut ThreadUnblockList>,
        clt_token: &ChainLockTransactionToken,
    ) -> bool {
        debug_assert!(list.is_empty());
        // Lock as many threads as we can, placing them on a temporary unblock list as
        // we go.  We remove the threads from the collection as we go, but we do not
        // remove the bookkeeping (see the note on optimization below).

        // TODO(johngro): Figure out a way to optimize this.
        //
        // We need to go over the wait queue and select the "next N best threads to
        // wake up".  The total order of the wait queue, however, is complicated.
        // Which thread is the "best" thread depends on what time it is, if there are
        // deadline threads in the queue, and whether or not that thread's deadline is
        // in the future or the past.
        //
        // The queue is arranged to make it easy to find the "best thread to wake"
        // quickly, but is not arranged to easily iterate in the proper order (best to
        // worst to wake) based on a given time.
        //
        // One way around this is to simply remove the threads from the collection as
        // we lock then, and then peek again using the same `now` time as before.
        // There are two annoying issues with this, however.  If we fail to grab the
        // locks, we now have to drop all of the locks and put the threads back into
        // the collection as we unwind.  This is a lot of pointless re-balancing of
        // the tree structure for no good reason.
        //
        // The second issue is currently that OWQ code involved in waking and
        // requeueing threads expects those threads to be a proper member of the
        // collection, right up until the point that the threads are actually woken or
        // transferred to another queue.  This allows for some optimizations ("I don't
        // need to propagate X because there are now no more waiters") as well as some
        // heavy-duty debug checks (I can add up the IPVs of all the threads in the
        // queue, and it should equal my current IPV totals) which become confused if
        // we have speculatively removed threads from the collection before actually
        // removing them and their bookkeeping (something which requires all of the
        // locks to be held).
        //
        // So, for now, we have a solution which is basically the worst-of-all-worlds.
        // During the locking, we _do_ actually remove the threads from the wait queue
        // collection as we add them into our temporary unblock list.  If we fail and
        // need to unwind, we need to unlock and put the threads back into the
        // collection. But, _additionally_, if we succeed, we need to go over the list
        // again and put the threads back into the collection, only so the rest of the
        // OWQ can take them back out again at the proper point in time.
        //
        // Ideally, we would find an efficient wait to optimize the iteration of the
        // queue given a specific time so that we didn't have to fall back on such
        // heavy handed approaches.  Failing that, it might be best to special case
        // various scenarios.  The most common scenarios are ones where we wake either
        // a single thread (in which case the search works fine), or all of the
        // threads, in which case the enumeration order is less important, although
        // could cause some scheduler thrash if done in the wrong order.
        // Additionally, the total order is just the enumeration order of the tree if
        // all of the deadline threads have absolute deadlines in the future (this
        // includes where there are no deadline threads in the queue), so that is a
        // simple case as well.  The only tricky case is when there exist deadline
        // threads in the queue whose deadline is in the past, and we want to wake
        // more than one thread.
        //
        for _count in 0..max_count {
            let Some(t) = self.peek_front() else {
                break;
            };
            // SAFETY: `t` is blocked in this queue, and cannot exit while this queue's lock is
            // held.
            let t: &Thread = unsafe { t.as_ref() };

            // Does our caller approve of this thread?  If not, stop here.
            let stop = {
                // This lock is not actually held, but the wait queue state of `t` cannot change
                // while it is a member of this queue and this queue's lock is held, which allows
                // read-only access (for `assert_in_wait_queue` and `hooks.allow`).
                // SAFETY: `t`'s lock is not held. `t` is blocked in this queue, whose lock is
                // held (proven by `self`), and a blocked thread's wait queue state cannot change
                // without also holding that lock. `t_proof` is only used for those reads.
                let t_proof = unsafe { assume_synchronized_read_only(t) };
                assert_in_wait_queue(&t_proof, &self.as_base());
                !hooks.allow(t)
            };

            if stop {
                break;
            }

            // We need to back off.  Drop any locks we have acquired so far.
            // SAFETY: No proof or guard for `t` is live.
            if !unsafe { raw_lock(t) }.acquire_or_backoff(clt_token) {
                if !list.is_empty() {
                    // SAFETY: The threads in `list` were locked and removed from the collection
                    // by this call.
                    unsafe { self.unlock_and_clear_waiter_list_locked(list.as_mut(), clt_token) };
                }
                return false;
            }

            // Cycles should be impossible at this point, and we now hold the lock; add the
            // thread to the list of threads we have locked.  Then, move on to locking more
            // threads if we need to.
            // SAFETY: `t`'s lock was just acquired, and `t` is a member of the collection. It is
            // returned to the collection before this queue's lock is released, either by this call
            // (on failure) or by the caller (on success), which the `OwnedWaitQueue` storage
            // invariant permits. `t` is not in any unblock list while it is blocked in this queue.
            unsafe {
                self.collection_mut().remove(&mut assume_locked(t));
                list.as_mut().push_back(t);
            }
        }

        // Success!  Do not attempt to put our threads back just yet.  If we are
        // making two lists for a `wake_and_requeue` operation, we need to keep the
        // threads we have selected for wake out of the collection while we select the
        // threads for requeue.
        true
    }

    /// Unlocks each thread on `list`, placing it back into the collection and clearing the list.
    ///
    /// # Safety
    ///
    /// The caller must ensure that every thread on `list` was locked and removed from this queue's
    /// collection by [`Self::try_lock_and_make_waiter_list_locked`].
    unsafe fn unlock_and_clear_waiter_list_locked(
        &mut self,
        mut list: Pin<&mut ThreadUnblockList>,
        clt_token: &ChainLockTransactionToken,
    ) {
        // If we encountered a back-off error, we need to unlock each thread we had on
        // our list, placing threads back into the collection and clearing the list as
        // we go.
        while let Some(t) = list.as_mut().pop_front() {
            // SAFETY: Every thread on the list is live and locked (per the caller).
            let t: &Thread = unsafe { t.as_ref() };
            t.lock().assert_acquired(clt_token);
            // SAFETY: `t` is locked and was removed from the collection (per the caller).
            unsafe {
                self.collection_mut().insert(&mut assume_locked(t));
            }
            // SAFETY: `t`'s lock is held and no proof or guard for it is live.
            unsafe { raw_lock(t) }.release(clt_token);
        }
    }

    /// Common logic for [`OwnedWaitQueue::assign_owner`] and
    /// [`OwnedWaitQueue::assign_owner_locked`].
    ///
    /// # Safety
    ///
    /// The caller must ensure that the transaction is not finalized, that no locks in the PI
    /// chains of this queue's owner or `new_owner` are held (other than `new_owner`'s own lock if
    /// `new_owner_is_locked`), and that no proofs or guards for `new_owner` are used during the
    /// call.
    unsafe fn assign_owner_common(
        &mut self,
        new_owner: Option<&Thread>,
        new_owner_is_locked: bool,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> TransactionResult<Result<(), Status>> {
        // If the owner is not changing, we are already done.
        if same_thread(self.owner_thread(), new_owner) {
            return TransactionResult::Value(Ok(()));
        }

        // Obtain the locks needed to change owner.  If, while obtaining locks, we
        // detect that the new owner proposal would generate a cycle, we nak the
        // entire operation with a BAD_STATE error and change nothing.
        // SAFETY: Forwarded from the caller (`new_owner` is a live thread).
        let result = unsafe {
            self.lock_for_owner_replacement(new_owner, None, true, new_owner_is_locked, clt_token)
        };
        let details = match result {
            Err(lock_res) => {
                if lock_res == ChainLockResult::Cycle {
                    return TransactionResult::Value(Err(Status::BAD_STATE));
                }

                // The only other option here is that we are supposed to back off and try
                // again.  If we had succeeded, we would have gotten back locking details
                // instead.
                debug_assert!(lock_res == ChainLockResult::Backoff);
                return Action::Backoff.into();
            }
            Ok(details) => details,
        };

        // Go ahead and perform the assignment.  Start by removing any current
        // owner we have, dropping the locks along that path immediately after we
        // are done.
        if let Some(old_owner) = self.owner_thread() {
            // SAFETY: The owner of a queue is a live thread which cannot change while this
            // queue's lock is held.
            let old_owner: &Thread = unsafe { old_owner.as_ref() };
            old_owner.lock().assert_held(clt_token);
            // SAFETY: `old_owner` is this queue's current owner, and the PI chain starting from
            // it was locked (up to `details.stop_point`) by `lock_for_owner_replacement`.
            unsafe {
                OwnedWaitQueue::begin_propagate_owq_thread(
                    self,
                    old_owner,
                    REMOVE_SINGLE_EDGE_OP,
                    clt_token,
                    preempt_disabled_token,
                );
            }
            // SAFETY: The chain starting from `old_owner` is still locked up to
            // `details.stop_point`, and no proofs or guards for its nodes are live.
            unsafe {
                OwnedWaitQueue::unlock_pi_chain_common(old_owner, details.stop_point, clt_token);
            }
        }

        // Now, if we have a new owner to assign, perform the assignment, then
        // drop the locks along the new owner path.
        debug_assert!(same_thread(details.new_owner, new_owner));
        if let Some(new_owner) = new_owner {
            new_owner.lock().assert_held(clt_token);
            // SAFETY: This queue now has no owner, and the PI chain starting from `new_owner`
            // was locked by `lock_for_owner_replacement` (or `new_owner` was already locked by
            // the caller), with no proofs or guards for its nodes live.
            unsafe {
                OwnedWaitQueue::begin_propagate_owq_thread(
                    self,
                    new_owner,
                    ADD_SINGLE_EDGE_OP,
                    clt_token,
                    preempt_disabled_token,
                );
            }
            if !new_owner_is_locked {
                let stop: *const c_void = core::ptr::null();
                // SAFETY: The chain starting from `new_owner` is locked, and no proofs or guards
                // for its nodes are live.
                unsafe { OwnedWaitQueue::unlock_pi_chain_common(new_owner, stop, clt_token) };
            }
        }

        // We are finished.  We will drop our queue lock and re-enable preemption as
        // we unwind.
        TransactionResult::Value(Ok(()))
    }

    /// Note that the locks for the entire PI chain starting from this OWQ, as well
    /// as from the one starting from the new owner, need to be held at this point.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the locks of the PI chains starting from this queue and from
    /// `new_owner` are held by the active (finalized) transaction, and that no proofs or guards
    /// for those nodes (other than `self`) are used during the call.
    unsafe fn assign_owner_internal(
        &mut self,
        new_owner: Option<&Thread>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) {
        // If there is no change, then we are done already.
        if same_thread(self.owner_thread(), new_owner) {
            return;
        }

        // Start by releasing the old owner (if any) and propagating the PI effects.
        if let Some(owner) = self.owner_thread() {
            // SAFETY: The owner of a queue is a live thread which cannot change while this
            // queue's lock is held.
            let owner: &Thread = unsafe { owner.as_ref() };
            owner.lock().assert_held(clt_token);
            // SAFETY: `owner` is this queue's current owner, and the caller guarantees that the
            // PI chain starting from it is locked by the finalized transaction.
            unsafe {
                OwnedWaitQueue::begin_propagate_owq_thread(
                    self,
                    owner,
                    REMOVE_SINGLE_EDGE_OP,
                    clt_token,
                    preempt_disabled_token,
                );
            }
        }

        // If there is a new owner to assign, do so now and propagate the PI effects.
        if let Some(new_owner) = new_owner {
            new_owner.lock().assert_held(clt_token);
            // SAFETY: `new_owner`'s lock is held (per the caller and asserted above), and the
            // proof is only used for this read.
            debug_assert!(unsafe { assume_locked(new_owner) }.can_own_wait_queues());
            // SAFETY: This queue now has no owner, and the caller guarantees that the PI chain
            // starting from `new_owner` is locked by the finalized transaction.
            unsafe {
                OwnedWaitQueue::begin_propagate_owq_thread(
                    self,
                    new_owner,
                    ADD_SINGLE_EDGE_OP,
                    clt_token,
                    preempt_disabled_token,
                );
            }
        }

        self.validate_sched_state_storage();
    }

    /// Wakes the locked `threads` from this wait queue while already holding the queue lock.
    ///
    /// The PI-chain locks and the locks of the threads in `threads` are released as the threads
    /// are unblocked, while `self.owner().lock()` remains held.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - Every thread in `threads` is live, locked by the active transaction, and blocked in
    ///   `self.owner()`'s collection, and the PI chain downstream of `self.owner()` (starting from
    ///   its current owner, if any) is locked by the active transaction. This is the state
    ///   produced by a successful [`OwnedWaitQueue::lock_for_wake_operation_or_backoff`] or
    ///   [`Self::lock_for_wake_operation_locked`] on `self.owner()` in the current transaction
    ///   (or, for its wake list, by `OwnedWaitQueue::lock_for_requeue_operation_or_backoff`), with
    ///   no intervening modifications to `self.owner()`, `threads`, or the acquired locks.
    /// - No [`ChainLockProof<'_, Thread>`] or [`OwnerGuard<'_, Thread>`] for any thread in
    ///   `threads` is live across this call.
    /// - The active transaction has been finalized via
    ///   [`ChainLockTransaction::finalize`](crate::kconcurrent::ChainLockTransaction::finalize).
    /// - If `option == WakeOption::AssignOwner`, `threads` contains at most one thread.
    pub(crate) unsafe fn wake_threads_locked<H: IWakeRequeueHook + ?Sized>(
        &mut self,
        mut threads: Pin<&mut ThreadUnblockList>,
        wake_hooks: &mut H,
        option: WakeOption,
        force_inheritance: ForceInheritance,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> WakeThreadsResult {
        assert_transaction_active(clt_token);
        preempt_disabled_token.assert_held();
        debug_assert!(self.owner().magic() == OWNED_MAGIC);
        debug_assert!(option != WakeOption::AssignOwner || threads.iter().nth(1).is_none());
        let mut woken: u32 = 0;

        // Start by removing any existing owner.  We will either wake a thread an make
        // it the new owner, or we are simply going to remove the owner entirely.
        // Once we have removed the old owner, we can unlock the PI chain starting
        // from the old owner (if any)
        if let Some(old_owner) = self.owner_thread() {
            // SAFETY: Forwarded from the caller.
            unsafe { self.assign_owner_internal(None, clt_token, preempt_disabled_token) };
            // SAFETY: `old_owner` was the owner of this queue, and its chain is still locked.
            let old_owner: &Thread = unsafe { old_owner.as_ref() };
            old_owner.lock().assert_acquired(clt_token);
            // SAFETY: The chain starting from `old_owner` is locked (per the caller).
            unsafe {
                OwnedWaitQueue::unlock_pi_chain_common(old_owner, core::ptr::null(), clt_token);
            }
        }

        // Now wake all of the threads we had selected
        while let Some(t) = threads.as_mut().pop_front() {
            // SAFETY: Every thread in `threads` is live and locked (per the caller).
            let t: &Thread = unsafe { t.as_ref() };
            t.lock().assert_acquired(clt_token);

            {
                // SAFETY: `t`'s lock is held and no other proof or guard for it is live.
                let mut t_proof = unsafe { assume_locked(t) };

                // Call the user's hook, allowing them to maintain any internal bookkeeping
                // they need to maintain.
                wake_hooks.on_wake_or_requeue(&mut t_proof);

                // Remove this thread from our collection, then remove its contributions to
                // our IPVs.
                let old_target_ep =
                    self.has_deadline_pressure().then(|| self.get_effective_profile());
                // SAFETY: `t` is blocked in this queue, and the storage is updated below.
                unsafe { self.as_base().dequeue(&mut t_proof, Ok(())) };
                // SAFETY: `t` was just removed from this queue, and the caller guarantees that the
                // PI chain downstream of this queue is locked by the finalized transaction.
                unsafe {
                    OwnedWaitQueue::begin_propagate_thread_owq(
                        &mut t_proof,
                        self,
                        REMOVE_SINGLE_EDGE_OP,
                        force_inheritance,
                        old_target_ep.as_ref(),
                        clt_token,
                        preempt_disabled_token,
                    );
                }
                // SAFETY: `t` was just removed from this queue's collection.
                unsafe { self.update_sched_state_storage_thread_removed(&mut t_proof) };
            }

            // If we still have blocked threads, and we are supposed to make this thread
            // our owner, do so now.  We removed our existing owner at the start of this
            // operation, and it is illegal to attempt to wake multiple threads and make
            // them each our owner, so our owner should still be `None`.
            //
            // TODO(johngro): We may need a tweak to the propagation.  Our thread is in
            // an intermediate state right now.  It has been removed from its wait
            // queue, but its state is still blocked since it has not been through
            // `scheduler::unblock` yet.  The easiest thing to do might be to just
            // directly assign our IPVs from our current wait queue stats and set owner
            // directly.
            if !self.is_empty() && (option == WakeOption::AssignOwner) {
                debug_assert!(self.owner_thread().is_none());
                // SAFETY: `t`'s lock is held, and it is not blocked in any queue, so it has no
                // downstream PI chain.
                unsafe { self.assign_owner_internal(Some(t), clt_token, preempt_disabled_token) };
            }

            // Finally, unblock the thread, drop its lock in the process.
            // SAFETY: `t`'s lock is held (per the caller, and debug-asserted above), and no other
            // proof or guard for it is live.
            let t_guard = unsafe { OwnerGuard::adopt_unchecked(t, clt_token) };
            // SAFETY: `t` was dequeued from this queue and the active transaction is finalized.
            unsafe { scheduler::unblock(t_guard, clt_token) };
            woken += 1;
        }

        // If our `WakeOption` was `None`, we should no longer have an owner.  Otherwise,
        // it was `AssignOwner`, in which case we should only have an owner if we still
        // have blocked threads.
        debug_assert!(
            ((option == WakeOption::None) && self.owner_thread().is_none())
                || ((option == WakeOption::AssignOwner)
                    && (!self.is_empty() || self.owner_thread().is_none()))
        );

        self.validate_sched_state_storage();
        WakeThreadsResult { woken, still_waiting: self.count(), owner: self.owner_thread() }
    }
}

impl OwnedWaitQueue {
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
    #[inline]
    pub(crate) unsafe fn lock_for_wake_operation_or_backoff<'a, H: IWakeRequeueHook + ?Sized>(
        &'a self,
        max_wake: u32,
        wake_hooks: &mut H,
        unblock_list: Pin<&mut ThreadUnblockList>,
        clt_token: &'a ChainLockTransactionToken,
    ) -> Option<OwnerGuard<'a, Self>> {
        assert_transaction_active(clt_token);
        debug_assert_lock_not_held(self.lock());
        // Grab our lock first.
        let mut guard = OwnerGuard::new(self, clt_token);

        // Try to lock all of the other things we need to lock.
        // SAFETY: Forwarded from the caller.
        if !unsafe {
            guard.proof().lock_for_wake_operation_locked(
                max_wake,
                wake_hooks,
                unblock_list,
                clt_token,
            )
        } {
            // Failure; unlock, back off, and try again.
            guard.release();
            return None;
        }

        // Success!  The list of locked threads ready to be woken has been populated.
        Some(guard)
    }

    /// Obtains all of the locks needed for a `wake_and_requeue` operation.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the active transaction is not finalized, that no locks are
    /// held, that `requeue_locking_details_out` is freshly initialized (by
    /// [`RequeueLockingDetails::init`]: empty lists, no new owner, and null stop points), and that
    /// on success the transaction is finalized and the locks are released via
    /// [`Self::wake_and_requeue_internal`]. On failure (`None`), the contents of
    /// `requeue_locking_details_out` are unspecified (but its lists are empty).
    #[allow(clippy::too_many_arguments)]
    unsafe fn lock_for_requeue_operation_or_backoff<
        'a,
        WH: IWakeRequeueHook + ?Sized,
        RH: IWakeRequeueHook + ?Sized,
    >(
        &'a self,
        requeue_target: &'a OwnedWaitQueue,
        new_requeue_target_owner: Option<&Thread>,
        max_wake: u32,
        max_requeue: u32,
        wake_hooks: &mut WH,
        requeue_hooks: &mut RH,
        requeue_locking_details_out: Pin<&mut RequeueLockingDetails>,
        clt_token: &'a ChainLockTransactionToken,
    ) -> Option<(OwnerGuard<'a, Self>, OwnerGuard<'a, Self>)> {
        // Helpers which will release the locks for the threads we just locked,
        // taking them off their unblock lists in the process, if we have to backoff
        // and retry.
        //
        // # Safety
        //
        // Every thread on `list` must be locked by the active transaction.
        unsafe fn unlock_and_clear_thread_list(
            mut list: Pin<&mut ThreadUnblockList>,
            clt_token: &ChainLockTransactionToken,
        ) {
            while let Some(t) = list.as_mut().pop_front() {
                // SAFETY: Every thread on the list is live and locked (per the caller).
                let t: &Thread = unsafe { t.as_ref() };
                t.lock().assert_acquired(clt_token);
                // SAFETY: `t`'s lock is held and no proof or guard for it is live.
                unsafe { raw_lock(t) }.release(clt_token);
            }
        }

        // # Safety
        //
        // Every thread on the lists in `threads` must be locked by the active transaction.
        unsafe fn unlock_locked_threads(
            threads: Pin<&mut WakeRequeueThreadDetails>,
            clt_token: &ChainLockTransactionToken,
        ) {
            let threads = threads.project();
            // SAFETY: Forwarded from the caller.
            unsafe {
                unlock_and_clear_thread_list(threads.wake_threads, clt_token);
                unlock_and_clear_thread_list(threads.requeue_threads, clt_token);
            }
        }

        // A helper which we can use to unlock owner chains we may be holding during
        // a backoff situation.
        //
        // # Safety
        //
        // If `owner` is `Some` and is not `stop`, the chain starting from `owner` must be locked
        // up to `stop`.
        unsafe fn unlock_owner_chain(
            owner: Option<ThreadPtr>,
            stop: *const c_void,
            clt_token: &ChainLockTransactionToken,
        ) {
            if let Some(owner) = owner.filter(|owner| thread_stop_point(Some(*owner)) != stop) {
                // SAFETY: The owner is a live thread whose lock is held (per the caller).
                let owner: &Thread = unsafe { owner.as_ref() };
                owner.lock().assert_acquired(clt_token);
                // SAFETY: Forwarded from the caller.
                unsafe { OwnedWaitQueue::unlock_pi_chain_common(owner, stop, clt_token) };
            }
        }

        // Start by locking the this queue and the requeue target.
        // SAFETY: No proof or guard for either queue is live.
        if !acquire_both_or_backoff(
            unsafe { raw_lock(self) },
            unsafe { raw_lock(requeue_target) },
            clt_token,
        ) {
            return None;
        }
        // SAFETY: Both locks were just acquired by the active transaction, and no other proof or
        // guard for either queue is live.
        let (mut guard, mut requeue_target_guard) = unsafe {
            (
                OwnerGuard::adopt_unchecked(self, clt_token),
                OwnerGuard::adopt_unchecked(requeue_target, clt_token),
            )
        };

        // Next, the threads we plan to wake/requeue, placing them on two different lists.
        let mut res = requeue_locking_details_out.project();
        debug_assert!(res.new_requeue_target_owner.is_none());
        debug_assert!(res.new_requeue_target_owner_stop_point.is_null());
        debug_assert!(res.old_wake_queue_owner_stop_point.is_null());
        debug_assert!(res.old_requeue_target_owner_stop_point.is_null());
        {
            // SAFETY: Forwarded from the caller.
            let locked = unsafe {
                guard.proof().lock_and_make_wake_requeue_thread_lists_locked(
                    max_wake,
                    wake_hooks,
                    max_requeue,
                    requeue_hooks,
                    res.threads.as_mut(),
                    clt_token,
                )
            };

            if !locked {
                guard.release();
                requeue_target_guard.release();
                return None;
            }
        }

        // Next, lock the proposed new owner of the requeue target (if any),
        // validating the choice of owner in the process.  We need to reject this
        // new owner selection if it is not allowed to own wait queues (in the
        // process of exiting) or if it would form a cycle after this operation.  A
        // cycle can be formed if the new owner chain passes through a thread
        // already in the requeue target (stopping at the requeue target queue
        // itself), or if it passes through any of the threads we intend to requeue.
        if let Some(new_requeue_target_owner) = new_requeue_target_owner {
            let new_owner_ptr = Some(ThreadPtr::from(new_requeue_target_owner));
            // SAFETY: The proposed new owner is a live thread and the transaction is not
            // finalized. With `StopAtCycle`, any lock in its chain which is already held (one of
            // the queues or the threads locked above) is reported as the stop point.
            let result = unsafe {
                Self::lock_pi_chain_common::<StopAtCycle, Thread>(
                    new_requeue_target_owner,
                    clt_token,
                )
            };

            // Since we chose the "StopAtCycle" behavior, if we got a `ChainLockResult` back,
            // then we either succeeded locking the chain, or we need to back off.
            match result {
                Err(lock_res) => {
                    if lock_res == ChainLockResult::Backoff {
                        // SAFETY: The threads were locked above.
                        unsafe { unlock_locked_threads(res.threads.as_mut(), clt_token) };
                        guard.release();
                        requeue_target_guard.release();
                        return None;
                    }
                    *res.new_requeue_target_owner = new_owner_ptr;
                    debug_assert!(lock_res == ChainLockResult::Ok);
                }
                Ok(stop) => {
                    // if we found a stopping point, and that stopping point is not the
                    // wake-queue, or a thread we plan to wake, we need to deny the new
                    // owner, dropping the locks we just obtained in the process.
                    let accept = (|| {
                        // If we hit nothing, or we hit the wake-queue, we can accept this owner.
                        if stop.is_null() || core::ptr::addr_eq(stop, self) {
                            return true;
                        }

                        // If we anything in the set of threads to wake, we can accept this
                        // owner.
                        for t in res.threads.as_ref().get_ref().wake_threads.iter() {
                            if core::ptr::addr_eq(stop, t.as_ptr()) {
                                return true;
                            }
                        }

                        // We hit a lock we already hold, but it was not the wake-queue or a
                        // thread to be woken.  It must have been either a thread being
                        // requeued or the requeue target itself, and we cannot accept this
                        // new requeue owner.
                        false
                    })();

                    // If we can accept this new owner, stash it and its stop point in the
                    // result we plan to return.  Otherwise, unlock what we have locked and
                    // stash nothing.  Note: We don't want to unlock anything if the stop
                    // point is the same as the proposed new owner.  This can happen if the
                    // proposed new owner is one of the threads which is being requeued.
                    if accept {
                        *res.new_requeue_target_owner = new_owner_ptr;
                        *res.new_requeue_target_owner_stop_point = stop;
                    } else {
                        // SAFETY: The chain starting from the new owner was locked up to `stop`.
                        unsafe { unlock_owner_chain(new_owner_ptr, stop, clt_token) };
                    }
                }
            }

            // Reject the proposed new owner if it isn't allowed to own wait queues.
            if let Some(owner) = *res.new_requeue_target_owner {
                // SAFETY: The proposed new owner is a live thread (per the caller).
                let owner: &Thread = unsafe { owner.as_ref() };
                owner.lock().assert_held(clt_token);
                // SAFETY: `owner`'s lock is held, and only read here.
                if !unsafe { assume_locked(owner) }.can_own_wait_queues() {
                    // SAFETY: The chain starting from the new owner was locked up to its stop
                    // point.
                    unsafe {
                        unlock_owner_chain(
                            *res.new_requeue_target_owner,
                            *res.new_requeue_target_owner_stop_point,
                            clt_token,
                        );
                    }
                    *res.new_requeue_target_owner = None;
                    *res.new_requeue_target_owner_stop_point = core::ptr::null();
                }
            }
        }

        // Now, if the requeue-target has a current owner, and that owner is
        // changing, lock its chain. It is OK if this chain hits any of the locks we
        // already hold.
        let requeue_target_owner = requeue_target_guard.proof().owner_thread();
        let locked_current_requeue_target_owner = requeue_target_owner.is_some()
            && (requeue_target_owner != *res.new_requeue_target_owner);
        if let Some(owner) = requeue_target_owner.filter(|_| locked_current_requeue_target_owner) {
            // SAFETY: The owner of a queue is a live thread which cannot change while the queue's
            // lock is held, and the transaction is not finalized. With `StopAtCycle`, any lock in
            // its chain which is already held is reported as the stop point.
            let result = unsafe {
                Self::lock_pi_chain_common::<StopAtCycle, Thread>(owner.as_ref(), clt_token)
            };

            // If we have a result, we either succeeded or need to back off.
            match result {
                Err(lock_res) => {
                    if lock_res == ChainLockResult::Backoff {
                        // SAFETY: The chain starting from the new owner and the threads were
                        // locked above.
                        unsafe {
                            unlock_owner_chain(
                                *res.new_requeue_target_owner,
                                *res.new_requeue_target_owner_stop_point,
                                clt_token,
                            );
                            unlock_locked_threads(res.threads.as_mut(), clt_token);
                        }
                        guard.release();
                        requeue_target_guard.release();
                        return None;
                    }
                    debug_assert!(lock_res == ChainLockResult::Ok);
                }
                Ok(stop) => {
                    // Record the stopping point.
                    *res.old_requeue_target_owner_stop_point = stop;
                }
            }
        }

        // Finally, lock the exiting owner chain for the wake-queue.  It is OK if
        // this chain hits any of the locks we already hold.
        if let Some(owner) = guard.proof().owner_thread() {
            // SAFETY: The owner of a queue is a live thread which cannot change while the queue's
            // lock is held, and the transaction is not finalized. With `StopAtCycle`, any lock in
            // its chain which is already held is reported as the stop point.
            let result = unsafe {
                Self::lock_pi_chain_common::<StopAtCycle, Thread>(owner.as_ref(), clt_token)
            };

            // If we have a result, we either succeeded or need to back off.
            match result {
                Err(lock_res) => {
                    if lock_res == ChainLockResult::Backoff {
                        // SAFETY: The chains and threads unlocked here were locked above.
                        unsafe {
                            if locked_current_requeue_target_owner {
                                unlock_owner_chain(
                                    requeue_target_guard.proof().owner_thread(),
                                    *res.old_requeue_target_owner_stop_point,
                                    clt_token,
                                );
                            }
                            unlock_owner_chain(
                                *res.new_requeue_target_owner,
                                *res.new_requeue_target_owner_stop_point,
                                clt_token,
                            );
                            unlock_locked_threads(res.threads.as_mut(), clt_token);
                        }
                        guard.release();
                        requeue_target_guard.release();
                        return None;
                    }
                    debug_assert!(lock_res == ChainLockResult::Ok);
                }
                Ok(stop) => {
                    // Record the stopping point.
                    *res.old_wake_queue_owner_stop_point = stop;
                }
            }
        }

        // Success!
        Some((guard, requeue_target_guard))
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
    #[inline]
    pub unsafe fn lock_for_baao_operation_or_backoff<'a>(
        &'a self,
        current_thread: &'a Thread,
        new_owner: Option<&Thread>,
        clt_token: &'a ChainLockTransactionToken,
    ) -> Option<(BAAOLockingDetails, OwnerGuard<'a, Self>, OwnerGuard<'a, Thread>)> {
        assert_transaction_active(clt_token);
        debug_assert_lock_not_held(self.lock());
        // Start by locking this queue (`try_lock_for_baao_operation_locked` checks `current_thread`
        // and its lock immediately afterwards).
        let mut guard = OwnerGuard::new(self, clt_token);

        // Now try to lock the rest of the things we need to lock, adjusting the
        // proposed owner as needed to prevent any cycles which might otherwise form.
        // If we got details back, then we are done.  We should be holding all of the
        // locks we need to hold (including the current thread's lock).
        // SAFETY: Forwarded from the caller.
        if let Some((details, current_thread_guard)) = unsafe {
            guard.proof().try_lock_for_baao_operation_locked(current_thread, new_owner, clt_token)
        } {
            return Some((details, guard, current_thread_guard));
        }

        // We encountered some form of backoff error.  Drop the queue lock and try again.
        guard.release();
        None
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
        debug_assert!(self.magic() == OWNED_MAGIC);
        let do_transaction =
            |token: &ChainLockTransactionToken| -> TransactionResult<Result<(), Status>> {
                // SAFETY: `EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the
                // duration of this transaction.
                let preempt_disabled = unsafe { PreemptDisabledToken::new() };
                let mut guard = OwnerGuard::new(self, token);
                // SAFETY: The transaction is not finalized and only this queue's lock is held.
                unsafe {
                    guard.proof().assign_owner_common(new_owner, false, token, &preempt_disabled)
                }
            };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::AssignOwner"),
            do_transaction,
        )
    }

    /// Assigns ownership of this wait queue to an already-locked `new_owner`.
    ///
    /// Returns `Err(Status::SHOULD_WAIT)` if a back-off is required, and `Ok(())` otherwise. If
    /// `new_owner` is not allowed to own wait queues ([`ChainLockProof::can_own_wait_queues`] is
    /// `false`), any existing owner is replaced with no owner. Callers must not create a cycle:
    /// one is only detected by a `debug_assert!`, and returns `Err(Status::BAD_STATE)` (without
    /// changing the owner) in release builds.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - The active transaction has not yet been finalized, and `self.lock()` is not held by it.
    /// - `new_owner.owner()` has been started (`state != Initial`, so that
    ///   [`ChainLockProof::can_own_wait_queues`] is valid).
    /// - `new_owner.owner()`'s downstream PI chain is either empty (not blocked in any wait queue)
    ///   or already locked by the active
    ///   [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) without forming a
    ///   cycle with `self`.
    pub(crate) unsafe fn assign_owner_locked(
        &self,
        new_owner: &mut ChainLockProof<'_, Thread>,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Result<(), Status> {
        assert_transaction_active(clt_token);
        debug_assert_lock_not_held(self.lock());
        preempt_disabled_token.assert_held();
        let Some(mut guard) = OwnerGuard::acquire_or_backoff(self, clt_token) else {
            return Err(Status::SHOULD_WAIT);
        };
        // SAFETY: Forwarded from the caller; `new_owner` is not used during the call.
        let result = unsafe {
            guard.proof().assign_owner_common(
                Some(new_owner.owner()),
                /*new_owner_is_locked=*/ true,
                clt_token,
                preempt_disabled_token,
            )
        };
        if result.is_action() {
            // If there is an action to perform, we expect to be Backoff.
            debug_assert!(result.action() == Action::Backoff);
            return Err(Status::SHOULD_WAIT);
        }
        // If there is a value, we expect `Ok(())`. Anything else suggests a cycle was detected.
        let result = result.take_value();
        debug_assert!(result == Ok(()));
        result
    }

    /// Resets the queue owner if there are no waiters currently queued.
    ///
    /// # Panics
    ///
    /// Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is already
    /// active on the current CPU.
    pub fn reset_owner_if_no_waiters(&self) {
        assert_no_transaction_active();
        debug_assert!(self.magic() == OWNED_MAGIC);
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            let mut guard = OwnerGuard::new(self, token);

            // If we have no owner, or we have waiters, we are finished.
            let owner = {
                let proof = guard.proof();
                proof.owner_thread().filter(|_| proof.is_empty())
            };
            let Some(owner) = owner else {
                guard.release();
                return DONE.into();
            };

            // We have an owner, but no waiters behind us.  We need to lock our owner in
            // order to clear out its bookkeeping, but there is nothing to propagate
            // down the PI chain since we are not inheriting anything.
            // SAFETY: The owner of a queue is a live thread which cannot change while the queue's
            // lock is held.
            let owner: &Thread = unsafe { owner.as_ref() };
            let Some(mut owner_guard) = OwnerGuard::acquire_or_backoff(owner, token) else {
                guard.release();
                return Action::Backoff.into();
            };

            ChainLockTransaction::finalize(token);

            // SAFETY: `owner` is this queue's current owner (read above under this queue's lock,
            // which has been held since).
            unsafe { guard.proof().reset_owner(&mut owner_guard.proof()) };
            owner_guard.release();
            guard.release();

            DONE.into()
        };
        ChainLockTransaction::until_done(
            EAGER_RESCHED_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::ResetOwnerIfNoWaiters"),
            do_transaction,
        );
    }

    /// Blocks the current thread on this wait queue, and re-assigns ownership to the specified
    /// thread (or removes ownership if `new_owner` is `None`). If assigning `new_owner` would
    /// produce a cycle, the proposed owner is rejected (and any existing owner is removed), but
    /// the current thread still blocks.
    ///
    /// Note, if the new owner exists, but is dead or dying, it will not be permitted to become the
    /// new owner of the wait queue. Any existing owner will be replaced with no owner in this
    /// situation.
    ///
    /// # Safety
    ///
    /// The caller must ensure that:
    /// - No [`ChainLockTransaction`] is active on the current CPU.
    /// - The current thread is in a valid state to block (`THREAD_RUNNING` and not holding any
    ///   spinlocks).
    /// - `self` (and `new_owner`, if `Some`) remains alive and valid at its pinned address for the
    ///   entire duration that the current thread remains blocked in `self`, and remains allocated
    ///   until this call returns (even if it is no longer valid as a wait queue once the current
    ///   thread has been unblocked), since reference arguments must be dereferenceable for the
    ///   whole call.
    pub unsafe fn block_and_assign_owner(
        &self,
        deadline: &Deadline,
        new_owner: Option<&Thread>,
        resource_ownership: ResourceOwnership,
        interruptible: Interruptible,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> Result<(), Status> {
        debug_assert_no_transaction_active();
        preempt_disabled_token.assert_held();
        // SAFETY: Multi-threading is initialized when blocking a thread.
        let current_thread: &Thread = unsafe { ThreadPtr::current().as_ref() };
        let do_transaction =
            |token: &ChainLockTransactionToken| -> TransactionResult<Result<(), Status>> {
                preempt_disabled_token.assert_held();
                // SAFETY: The transaction holds no locks, and `current_thread` is the current
                // thread. On success, the locks are released by `block_and_assign_owner_locked`
                // and `current_thread_guard.release()`.
                if let Some((details, guard, mut current_thread_guard)) = unsafe {
                    self.lock_for_baao_operation_or_backoff(current_thread, new_owner, token)
                } {
                    ChainLockTransaction::finalize(token);
                    // SAFETY: The transaction is finalized with the BAAO locks held, and the
                    // caller upholds the blocking preconditions.
                    let result = unsafe {
                        Self::block_and_assign_owner_locked(
                            guard,
                            &mut current_thread_guard.proof(),
                            deadline,
                            &details,
                            resource_ownership,
                            interruptible,
                            ForceInheritance::No,
                            token,
                            preempt_disabled_token,
                        )
                    };
                    // After the block operation, our thread is going to be locked.  Make sure to
                    // drop the lock before exiting.
                    current_thread_guard.release();
                    return TransactionResult::Value(result);
                }
                Action::Backoff.into()
            };
        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::BlockAndAssignOwner"),
            do_transaction,
        )
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
    ///   that `current_thread.owner()` remains blocked in `guard.owner()`, and remains allocated
    ///   until this call returns (even if it is no longer valid as a wait queue once
    ///   `current_thread.owner()` has been unblocked), since the reference held by `guard` must be
    ///   dereferenceable for the whole call.
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
        debug_assert!(core::ptr::eq(
            current_thread.owner().as_raw(),
            // SAFETY: Multi-threading is initialized when blocking a thread.
            unsafe { ThreadPtr::current() }.as_raw()
        ));
        debug_assert!(guard.owner().magic() == OWNED_MAGIC);
        debug_assert!(current_thread.state() == ThreadStateKind::Running);
        let active_clt =
            (!cfg!(lk_debuglevel = "0")).then(|| ChainLockTransaction::active_ref(clt_token));
        let current_thread_ptr = Some(ThreadPtr::from(current_thread.owner()));

        let this: *const WaitQueueBase = {
            let mut guard = guard;

            // TODO(johngro):
            //
            // Locking for this is more tricky than it seems.  If we have an original
            // owner (A), and a new owner (B), and both of them are blocked, and both of
            // them share the same PI target, simply attempting to lock both PI chains
            // would result in deadlock detection.
            //
            // What really needs to happen here is that, as we lock for a BAAO operation
            // which involves changing owners, we need to lock the current owner chain
            // first (before the new owner chain) and remove the current existing owner.
            // Once that is finished, we can attempt to lock the new-owner chain (again,
            // if any).  If this fails, we need to be prepared to back-off and try again,
            // including the possibility that we might need to drop the queue lock, and
            // displace yet another new owner in the process.
            //
            // We should not need to check for any cycles at this point, locking should
            // have taken care of that for us. If the there is a change of owner
            // happening, start by releasing the current owner.  After this, the PI chain
            // starting from our previous owner (if any) should be released.
            let owner = guard.proof().owner_thread();
            let owner_changed = owner != details.new_owner;
            if let Some(old_owner) = owner.filter(|_| owner_changed) {
                // Remove our old owner, then release the locks which had been held on the
                // old owner path, stopping early if we need to do to (see
                // `lock_for_baao_operation_or_backoff` for details).  Note, if the old owner
                // being removed is the same as the thread which is currently blocking, there is
                // nothing to unlock.  We need to continue to hold the current thread's lock until
                // it blocks, and there cannot be anything downstream of the current thread
                // since it is currently running.
                // SAFETY: Forwarded from the caller; `current_thread` is not used during the
                // call.
                unsafe {
                    guard.proof().assign_owner_internal(None, clt_token, preempt_disabled_token);
                }
                if current_thread_ptr != Some(old_owner) {
                    // SAFETY: The old owner's chain is still locked, so it is live.
                    let old_owner_ref: &Thread = unsafe { old_owner.as_ref() };
                    // Do not unlock the old owner chain if the old owner is the same as the
                    // stop point.  This can happen if the new owner chain intersects the old
                    // owner chain start at the old owner itself. In this case, the new owner
                    // chain is a superset of the old owner chain, and all of the locks will
                    // be properly released as we finish propagation down the new owner chain.
                    if thread_stop_point(Some(old_owner)) != details.stop_point {
                        old_owner_ref.lock().assert_acquired(clt_token);
                        // SAFETY: The old owner's chain is locked up to `details.stop_point`.
                        unsafe {
                            Self::unlock_pi_chain_common(
                                old_owner_ref,
                                details.stop_point,
                                clt_token,
                            );
                        }
                    } else {
                        old_owner_ref.lock().assert_held(clt_token);
                    }
                } else {
                    debug_assert!(details.stop_point == thread_stop_point(current_thread_ptr));
                }
            }

            // Perform the first half of the `block_etc` operation.  This will attempt to add
            // an edge between the thread which is blocking, and the OWQ it is blocking in
            // (this).  We know that this cannot produce a cycle in the graph because we
            // know that this OWQ does not currently have an owner.
            //
            // If the block preamble fails, then the state of the actual wait queue is
            // unchanged and we can just get out now.
            // SAFETY: The caller upholds the blocking preconditions, `guard.owner()` is an
            // `OwnedWaitQueue` and `wq_ptr` (derived from `&OwnedWaitQueue`) carries provenance
            // over it, and the storage is updated below on success.
            let res = unsafe {
                let wq_ptr = NonNull::from(guard.owner()).cast::<WaitQueueBase>();
                guard.proof().as_base().block_etc_preamble_with_ptr(
                    wq_ptr,
                    current_thread,
                    deadline,
                    0,
                    resource_ownership,
                    interruptible,
                )
            };
            if let Err(res) = res {
                // There are only three reasons why the pre-wait operation should ever fail.
                //
                // 1) ZX_ERR_TIMED_OUT            : The timeout has already expired.
                // 2) ZX_ERR_INTERNAL_INTR_KILLED : The thread has been signaled for death.
                // 3) ZX_ERR_INTERNAL_INTR_RETRY  : The thread has been signaled for suspend.
                //
                // No matter what, we are not actually going to block in the wait queue.
                // Even so, however, we still need to assign the owner to what was
                // requested by the thread.  Just because we didn't manage to block does
                // not mean that ownership assignment gets skipped.
                debug_assert!(
                    (res == Status::TIMED_OUT)
                        || (res.into_raw() == ZX_ERR_INTERNAL_INTR_KILLED)
                        // `ZX_ERR_INTERNAL_INTR_RETRY` is `ZX_ERR_INTERRUPTED_RETRY` here.
                        || (res == Status::INTERRUPTED_RETRY)
                );

                // If the owner was changing, the existing owner should already be `None`
                // at this point.  If we have a new owner to assign, do so now, and unlock
                // the rest of the PI chain.
                //
                // Otherwise, if the owner has not changed, and we still have an old owner,
                // don't forget to unlock the old owner chain before propagating our error
                // out.
                if owner_changed {
                    debug_assert!(guard.proof().owner_thread().is_none());
                    if let Some(new_owner) = details.new_owner {
                        // SAFETY: The new owner's chain is locked, so it is live.
                        let new_owner: &Thread = unsafe { new_owner.as_ref() };
                        new_owner.lock().assert_acquired(clt_token);
                        // SAFETY: The new owner's chain is locked (per the caller).
                        unsafe {
                            guard.proof().assign_owner_internal(
                                Some(new_owner),
                                clt_token,
                                preempt_disabled_token,
                            );
                            Self::unlock_pi_chain_common(new_owner, core::ptr::null(), clt_token);
                        }
                    }
                } else if let Some(owner) = guard.proof().owner_thread() {
                    // SAFETY: The owner's chain is locked, so it is live.
                    let owner: &Thread = unsafe { owner.as_ref() };
                    owner.lock().assert_acquired(clt_token);
                    // SAFETY: The owner's chain is locked (per the caller).
                    unsafe { Self::unlock_pi_chain_common(owner, core::ptr::null(), clt_token) };
                }

                // Now drop our lock and get out.  We should be holding only the current
                // thread's lock as we exit.
                guard.release();
                if let Some(active_clt) = active_clt {
                    active_clt.assert_num_locks_held(1);
                }
                return Err(res);
            }

            // We succeeded in placing our thread into our wait collection.  Make sure we
            // update the scheduler state storage location if needed, then propagate the
            // effects down the chain.
            // SAFETY: `current_thread` was just added to this queue's collection.
            unsafe { guard.proof().update_sched_state_storage_thread_added(current_thread) };
            // SAFETY: `current_thread` was just added to this queue, and the caller guarantees
            // that the PI chain downstream of this queue is locked by the finalized transaction.
            unsafe {
                Self::begin_propagate_thread_owq(
                    current_thread,
                    &mut guard.proof(),
                    ADD_SINGLE_EDGE_OP,
                    force_inheritance,
                    None,
                    clt_token,
                    preempt_disabled_token,
                );
            }

            // If we have an new_owner, we need to do one of two things.
            //
            // 1) If the owner has not changed, then the PI consequences were propagated
            //    when we added the edge from the blocking thread to this OWQ.  We just need
            //    to unlock the PI chain starting from the already existing owner and we
            //    should be finished.
            // 2) If the owner changed, then we should have unassigned the original owner
            //    at the start of this routine.  We need to assign the new owner now, and
            //    then unlock the chain starting from the new owner.
            //
            if owner_changed {
                debug_assert!(guard.proof().owner_thread().is_none());
                if let Some(new_owner) = details.new_owner {
                    // SAFETY: The new owner's chain is locked, so it is live.
                    let new_owner: &Thread = unsafe { new_owner.as_ref() };
                    new_owner.lock().assert_acquired(clt_token);
                    // SAFETY: The new owner's chain is locked (per the caller).
                    unsafe {
                        guard.proof().assign_owner_internal(
                            Some(new_owner),
                            clt_token,
                            preempt_disabled_token,
                        );
                        Self::unlock_pi_chain_common(new_owner, core::ptr::null(), clt_token);
                    }
                }
            } else if let Some(owner) = guard.proof().owner_thread() {
                // SAFETY: The owner's chain is locked, so it is live.
                let owner: &Thread = unsafe { owner.as_ref() };
                owner.lock().assert_acquired(clt_token);
                // SAFETY: The owner's chain is locked (per the caller).
                unsafe { Self::unlock_pi_chain_common(owner, core::ptr::null(), clt_token) };
            }

            // OK, we are almost done at this point in time.  We have dealt with all of
            // the ownership and PI related bookkeeping, and we have added the blocking
            // thread to this owned wait queue.  We can now drop the OWQ lock and finish
            // the BAAO operation by calling `block_etc_postamble`.  When we return from the
            // block operation, we should have re-acquired the current thread's lock
            // (however, it may be with a different lock token).
            let this: *const WaitQueueBase = core::ptr::from_ref(guard.owner()).cast();
            guard.release();
            if let Some(active_clt) = active_clt {
                active_clt.assert_num_locks_held(1);
            }
            this
        };

        // Finally, go ahead and run the second half of the `block_etc` operation.
        // This will actually block our thread and handle setting any timeout timers
        // in the process.
        let t = current_thread.reborrow();

        // DANGER!! DANGER!! DANGER!! DANGER!! DANGER!! DANGER!! DANGER!! DANGER!!
        //
        // It is very important that no attempts to access `this` are made after
        // either of the calls to `block_etc_postamble` (below). When someone eventually
        // comes along and unblocks us from the queue, they have already taken care of
        // removing us from the this wait queue.  It it totally possible that the wait
        // queue we were blocking in has been destroyed by the time we make it out of
        // `block_etc_postamble`, making `this` no longer a valid pointer.
        //
        // DANGER!! DANGER!! DANGER!! DANGER!! DANGER!! DANGER!! DANGER!! DANGER!!
        //
        // SAFETY: `block_etc_preamble` succeeded, the queue lock has been released,
        // `current_thread.owner().lock()` is the only held chain lock in the finalized
        // transaction, `this` was captured before releasing the queue lock, and the caller
        // upholds the blocking preconditions.
        let res = unsafe { WaitQueueBase::block_etc_postamble(this, t, deadline, clt_token) };
        if let Some(active_clt) = active_clt {
            debug_assert!(ChainLockTransaction::active() == Some(NonNull::from(active_clt)));
            active_clt.assert_num_locks_held(1);
        }
        res
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
        mut guard: OwnerGuard<'_, Self>,
        current_thread_guard: OwnerGuard<'_, Thread>,
        details: &BAAOLockingDetails,
        clt_token: &ChainLockTransactionToken,
    ) {
        assert_transaction_active(clt_token);
        let current_thread_ptr = Some(ThreadPtr::from(current_thread_guard.owner()));

        // If there is a current owner, handle unlocking the current owner chain.
        // There are a few special cases to consider here.
        //
        // 1) We don't need to unlock starting from here if the owner wasn't going to
        //    change.  If the existing owner == the proposed new owner, we are going
        //    to unconditionally unlock the new owner chain anyway.
        // 2) If the new owner is the same as the current thread (trying to block)
        //    thread, then the owner must have changed (the blocking thread cannot be the
        //    owning thread), but we don't want to drop the locks yet.  There cannot
        //    be anything downstream of the current thread (it is currently running),
        //    and we are going to unconditionally drop the current thread's lock on
        //    our way out of this method.
        // 3) If we are going to unlock starting from the existing owner's chain, we
        //    need to remember to stop at the stop-point provided by our details.  It
        //    is possible that our old owner, and a proposed new owner, have chains
        //    that intersect down-the-line, and we don't want to try to double release
        //    any of the locks held on the shared path between the two.
        //
        let owner = guard.proof().owner_thread();
        let owner_changed = owner != details.new_owner;
        if let Some(owner) =
            owner.filter(|owner| owner_changed && (current_thread_ptr != Some(*owner)))
        {
            // SAFETY: The owner's chain is locked, so it is live.
            let owner: &Thread = unsafe { owner.as_ref() };
            owner.lock().assert_acquired(clt_token);
            // SAFETY: The owner's chain is locked up to `details.stop_point` (per the caller).
            unsafe { Self::unlock_pi_chain_common(owner, details.stop_point, clt_token) };
        }

        // If we had a newly proposed owner, unlock its path.  This will also handle
        // unlocking the current owner path in the case that the current owner was not
        // changing.
        if let Some(new_owner) = details.new_owner {
            // SAFETY: The new owner's chain is locked, so it is live.
            let new_owner: &Thread = unsafe { new_owner.as_ref() };
            new_owner.lock().assert_acquired(clt_token);
            // SAFETY: The new owner's chain is locked (per the caller).
            unsafe { Self::unlock_pi_chain_common(new_owner, core::ptr::null(), clt_token) };
        }

        // We should be holding exactly 2 locks at this point in time.  The queue's
        // lock and the current thread's lock.  Drop these and we should be done.
        ChainLockTransaction::active_ref(clt_token).assert_num_locks_held(2);
        guard.release();
        current_thread_guard.release();
    }

    /// Wakes up to the specified number of threads from the wait queue, removing any current queue
    /// owner in the process.
    pub fn wake_threads<H: IWakeRequeueHook + ?Sized>(
        &self,
        wake_count: u32,
        wake_hooks: &mut H,
    ) -> WakeThreadsResult {
        assert_no_transaction_active();
        let mut result = WakeThreadsResult::default();
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the duration
            // of this transaction.
            let preempt_disabled = unsafe { PreemptDisabledToken::new() };
            pin_init::stack_pin_init!(let threads = ThreadUnblockList::init());
            // SAFETY: The transaction holds no locks and `threads` is empty. On success, the
            // locks are processed by `wake_threads_locked` and `guard.release()`.
            if let Some(mut guard) = unsafe {
                self.lock_for_wake_operation_or_backoff(
                    wake_count,
                    wake_hooks,
                    threads.as_mut(),
                    token,
                )
            } {
                ChainLockTransaction::finalize(token);
                // SAFETY: `threads` was populated by `lock_for_wake_operation_or_backoff` and the
                // transaction is finalized.
                result = unsafe {
                    guard.proof().wake_threads_locked(
                        threads.as_mut(),
                        wake_hooks,
                        WakeOption::None,
                        ForceInheritance::No,
                        token,
                        &preempt_disabled,
                    )
                };
                guard.release();
                return DONE.into();
            }
            Action::Backoff.into()
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::WakeThreadsResult"),
            do_transaction,
        );
        result
    }

    /// Attempts to wake exactly one thread, and assigns ownership of the queue to the woken thread
    /// (if any).
    pub fn wake_thread_and_assign_owner<H: IWakeRequeueHook + ?Sized>(
        &self,
        wake_hooks: &mut H,
    ) -> WakeThreadsResult {
        assert_no_transaction_active();
        let mut result = WakeThreadsResult::default();
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the duration
            // of this transaction.
            let preempt_disabled = unsafe { PreemptDisabledToken::new() };
            pin_init::stack_pin_init!(let threads = ThreadUnblockList::init());
            // SAFETY: The transaction holds no locks and `threads` is empty. On success, the
            // locks are processed by `wake_threads_locked` and `guard.release()`.
            if let Some(mut guard) = unsafe {
                self.lock_for_wake_operation_or_backoff(1, wake_hooks, threads.as_mut(), token)
            } {
                ChainLockTransaction::finalize(token);
                // SAFETY: `threads` was populated by `lock_for_wake_operation_or_backoff` with
                // `max_wake == 1`, and the transaction is finalized.
                result = unsafe {
                    guard.proof().wake_threads_locked(
                        threads.as_mut(),
                        wake_hooks,
                        WakeOption::AssignOwner,
                        ForceInheritance::No,
                        token,
                        &preempt_disabled,
                    )
                };
                guard.release();
                return DONE.into();
            }
            Action::Backoff.into()
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::WakeThreadsResult"),
            do_transaction,
        );
        result
    }

    /// Wakes up to `wake_count` threads from this queue and moves up to `requeue_count` threads to
    /// `requeue_target`, updating ownership bookkeeping according to `wake_option` and
    /// `new_requeue_owner`.
    ///
    /// This is used by futexes in order to implement futex_requeue. See
    /// [`Self::wake_and_requeue_internal`] for why it is a single specialized operation.
    ///
    /// Note, if `new_requeue_owner` exists, but is dead or dying, it will not be permitted to
    /// become the new owner of `requeue_target`. Any existing owner will be replaced with no owner
    /// in this situation.
    ///
    /// # Panics
    ///
    /// Panics if a [`ChainLockTransaction`](crate::kconcurrent::ChainLockTransaction) is already
    /// active on the current CPU, or if either of the following preconditions is violated:
    /// - `self` and `requeue_target` must be distinct queues. Otherwise both `OwnerGuard`s (and
    ///   the mutable proofs minted from them) would alias the same queue and lock, which would be
    ///   undefined behavior (and the second acquisition would be detected as a cycle).
    /// - `wake_option == WakeOption::AssignOwner` implies `wake_count <= 1`. Assigning ownership
    ///   to each of several woken threads would modify the `owned_wait_queues_` list of a woken
    ///   thread after its lock has already been released by `scheduler::unblock`, which would be
    ///   a data race (and violates the safety contract of `wake_threads_locked`).
    #[allow(clippy::too_many_arguments)]
    pub fn wake_and_requeue<WH: IWakeRequeueHook + ?Sized, RH: IWakeRequeueHook + ?Sized>(
        &self,
        requeue_target: &OwnedWaitQueue,
        new_requeue_owner: Option<&Thread>,
        wake_count: u32,
        requeue_count: u32,
        wake_hooks: &mut WH,
        requeue_hooks: &mut RH,
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
        let mut result = WakeThreadsResult::default();
        let do_transaction = |token: &ChainLockTransactionToken| -> TransactionResult<()> {
            // SAFETY: `PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION` disables preemption for the duration
            // of this transaction.
            let preempt_disabled = unsafe { PreemptDisabledToken::new() };
            pin_init::stack_pin_init!(let details = RequeueLockingDetails::init());
            // SAFETY: The transaction holds no locks and the lists in `details` are empty. On
            // success, the locks are released by `wake_and_requeue_internal`.
            if let Some((guard, requeue_target_guard)) = unsafe {
                self.lock_for_requeue_operation_or_backoff(
                    requeue_target,
                    new_requeue_owner,
                    wake_count,
                    requeue_count,
                    wake_hooks,
                    requeue_hooks,
                    details.as_mut(),
                    token,
                )
            } {
                ChainLockTransaction::finalize(token);
                // SAFETY: `details`, `guard`, and `requeue_target_guard` were produced by
                // `lock_for_requeue_operation_or_backoff`, and the transaction is finalized.
                result = unsafe {
                    Self::wake_and_requeue_internal(
                        guard,
                        requeue_target_guard,
                        details.as_mut(),
                        wake_hooks,
                        requeue_hooks,
                        wake_option,
                        token,
                        &preempt_disabled,
                    )
                };
                return DONE.into();
            }
            Action::Backoff.into()
        };
        ChainLockTransaction::until_done(
            PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
            clt_tag!("OwnedWaitQueue::WakeAndRequeue"),
            do_transaction,
        );
        result
    }

    /// A specialization of `wake_threads` which will...
    ///
    /// 1) Wake the number of threads indicated by `wake_count`
    /// 2) Move the number of threads indicated by `requeue_count` to the `requeue_target`.
    /// 3) Update ownership bookkeeping as indicated by `wake_option` and
    ///    `details.new_requeue_target_owner`.
    ///
    /// This method is used by futexes in order to implement futex_requeue.  It
    /// is wrapped up into a specialized form instead of broken into individual
    /// parts in order to minimize any thrash in re-computing effective
    /// profiles for PI purposes.  We don't want to re-evaluate ownership or PI
    /// pressure until after all of the changes to wait queue have taken place.
    ///
    /// Note, if the `details.new_requeue_target_owner` exists, but is dead or dying, it will not
    /// be permitted to become the new owner of the `requeue_target`.  Any existing
    /// owner will be replaced with no owner in this situation.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `details`, `guard`, and `requeue_target_guard` were produced by
    /// a preceding successful call to [`Self::lock_for_requeue_operation_or_backoff`] in the
    /// current transaction, with no intervening mutations, and that the transaction is finalized.
    #[allow(clippy::too_many_arguments)]
    unsafe fn wake_and_requeue_internal<
        WH: IWakeRequeueHook + ?Sized,
        RH: IWakeRequeueHook + ?Sized,
    >(
        mut guard: OwnerGuard<'_, Self>,
        mut requeue_target_guard: OwnerGuard<'_, Self>,
        details: Pin<&mut RequeueLockingDetails>,
        wake_hooks: &mut WH,
        requeue_hooks: &mut RH,
        wake_option: WakeOption,
        clt_token: &ChainLockTransactionToken,
        preempt_disabled_token: &PreemptDisabledToken,
    ) -> WakeThreadsResult {
        let this = guard.owner();
        let requeue_target = requeue_target_guard.owner();
        debug_assert!(this.magic() == OWNED_MAGIC);
        debug_assert!(requeue_target.magic() == OWNED_MAGIC);
        let details = details.project();

        // Step 1, remove the target of the wait-queue (this) and unlock its chain,
        // assuming that it has any unique nodes in it.
        if let Some(old_owner) = guard.proof().owner_thread() {
            // SAFETY: Forwarded from the caller.
            unsafe {
                guard.proof().assign_owner_internal(None, clt_token, preempt_disabled_token);
            }

            // Don't attempt to unlock this chain if the old owner of the wake-queue is
            // the same as the old owner stop-point.  The implication here is that the
            // old-wake-queue-owner locking chain overlaps 100% with one of the other
            // chains currently held, but which has yet to be dealt with.  For example,
            // the old owner of the wake-queue could be the same as the old owner of the
            // requeue-target, or the proposed new owner of the requeue-target.  We need
            // to keep holding those chains until later on in the process (after we have
            // dealt with their effects).
            if thread_stop_point(Some(old_owner)) != *details.old_wake_queue_owner_stop_point {
                // SAFETY: The old owner's chain is still locked, so it is live.
                let old_owner: &Thread = unsafe { old_owner.as_ref() };
                old_owner.lock().assert_acquired(clt_token);
                // SAFETY: The old owner's chain is locked up to its stop point.
                unsafe {
                    Self::unlock_pi_chain_common(
                        old_owner,
                        *details.old_wake_queue_owner_stop_point,
                        clt_token,
                    );
                }
            }
        }

        // Step 2: If the requeue target has an owner, and that owner is changing,
        // remove the existing owner and unlock the unique nodes in its chain.
        let requeue_owner_changed =
            requeue_target_guard.proof().owner_thread() != *details.new_requeue_target_owner;
        if let Some(old_requeue_owner) =
            requeue_target_guard.proof().owner_thread().filter(|_| requeue_owner_changed)
        {
            // SAFETY: Forwarded from the caller.
            unsafe {
                requeue_target_guard.proof().assign_owner_internal(
                    None,
                    clt_token,
                    preempt_disabled_token,
                );
            }
            if thread_stop_point(Some(old_requeue_owner))
                != *details.old_requeue_target_owner_stop_point
            {
                // SAFETY: The old requeue owner's chain is still locked, so it is live.
                let old_requeue_owner: &Thread = unsafe { old_requeue_owner.as_ref() };
                old_requeue_owner.lock().assert_acquired(clt_token);
                // SAFETY: The old requeue owner's chain is locked up to its stop point.
                unsafe {
                    Self::unlock_pi_chain_common(
                        old_requeue_owner,
                        *details.old_requeue_target_owner_stop_point,
                        clt_token,
                    );
                }
            }
        }

        // Step #3, take any of the threads we need to requeue and move them over into
        // the requeue target, transferring the bookkeeping from the wake queue over
        // to the requeue_target, and dropping each of the threads' locks, in the
        // process.
        let threads = details.threads.project();
        let mut requeue_threads = threads.requeue_threads;
        let wake_threads = threads.wake_threads;
        while let Some(t) = requeue_threads.as_mut().pop_front() {
            // SAFETY: Every thread in the requeue list is live and locked (per the caller).
            let t: &Thread = unsafe { t.as_ref() };
            t.lock().assert_acquired(clt_token);

            {
                // SAFETY: `t`'s lock is held and no other proof or guard for it is live.
                let mut t_proof = unsafe { assume_locked(t) };

                // Call the user's hook, allowing them to maintain any internal bookkeeping
                // they need to maintain.
                requeue_hooks.on_wake_or_requeue(&mut t_proof);

                // Actually move the thread from this to the requeue_target.
                let old_target_ep = {
                    let proof = guard.proof();
                    proof.has_deadline_pressure().then(|| proof.get_effective_profile())
                };
                // SAFETY: `t` is blocked in this queue, and the storage is updated below.
                unsafe { guard.proof().collection_mut().remove(&mut t_proof) };
                // SAFETY: `t` was just removed from this queue's collection.
                unsafe {
                    t_proof.wait_queue_state().set_blocking_wait_queue(None, t_proof.token_mut())
                };
                // SAFETY: `t` was just removed from this queue, and the caller guarantees that the
                // PI chain downstream of this queue is locked by the finalized transaction.
                unsafe {
                    Self::begin_propagate_thread_owq(
                        &mut t_proof,
                        &mut guard.proof(),
                        REMOVE_SINGLE_EDGE_OP,
                        ForceInheritance::No,
                        old_target_ep.as_ref(),
                        clt_token,
                        preempt_disabled_token,
                    );
                }
                // SAFETY: `t` was just removed from this queue's collection.
                unsafe { guard.proof().update_sched_state_storage_thread_removed(&mut t_proof) };

                // SAFETY: `t` is not in any wait queue collection, and the storage is updated
                // below.
                unsafe { requeue_target_guard.proof().collection_mut().insert(&mut t_proof) };
                // Derive the pointer from `&OwnedWaitQueue`, so that it carries provenance over
                // the whole `OwnedWaitQueue`, as the `blocking_wait_queue` invariant requires.
                let requeue_target_ptr = NonNull::from(requeue_target).cast::<WaitQueueBase>();
                // SAFETY: `t` was just inserted into `requeue_target`'s collection, which stays
                // live while `t` is blocked in it (per the caller of `wake_and_requeue`), and
                // `requeue_target_ptr` carries provenance over the enclosing `OwnedWaitQueue`.
                unsafe {
                    t_proof
                        .wait_queue_state()
                        .set_blocking_wait_queue(Some(requeue_target_ptr), t_proof.token_mut());
                }
                // SAFETY: `t` was just added to the requeue target's collection.
                unsafe {
                    requeue_target_guard
                        .proof()
                        .update_sched_state_storage_thread_added(&mut t_proof);
                }
                // SAFETY: `t` was just added to the requeue target, and the caller guarantees
                // that the PI chain downstream of the requeue target is locked by the finalized
                // transaction.
                unsafe {
                    Self::begin_propagate_thread_owq(
                        &mut t_proof,
                        &mut requeue_target_guard.proof(),
                        ADD_SINGLE_EDGE_OP,
                        ForceInheritance::No,
                        None,
                        clt_token,
                        preempt_disabled_token,
                    );
                }
            }

            // SAFETY: `t`'s lock is held and no proof or guard for it is live.
            unsafe { raw_lock(t) }.release(clt_token);
        }

        // Step #4, if we have a new owner for the requeue target, assign it now and
        // drop the locking chain starting from the new owner.  Otherwise, if our
        // owner didn't change, we still need to drop the locking chain starting from
        // the old owner (we had been holding it in order to propagate the PI
        // consequences of adding the newly requeued threads).
        if requeue_owner_changed {
            debug_assert!(requeue_target_guard.proof().owner_thread().is_none());
            if let Some(new_requeue_target_owner) = *details.new_requeue_target_owner {
                // SAFETY: The new requeue owner's chain is locked, so it is live.
                let new_owner: &Thread = unsafe { new_requeue_target_owner.as_ref() };
                // If there is no one waiting in the requeue target, then it is not
                // allowed to have an owner.
                if !requeue_target_guard.proof().is_empty() {
                    new_owner.lock().assert_held(clt_token);
                    // SAFETY: The new requeue owner's chain is locked (per the caller).
                    unsafe {
                        requeue_target_guard.proof().assign_owner_internal(
                            Some(new_owner),
                            clt_token,
                            preempt_disabled_token,
                        );
                    }
                }

                // Only unlock the new requeue-target owner chain if the new owner != the
                // chain's unlock stopping point.  This can happen when the new requeue
                // target owner specified is a thread which already exists in the
                // wake-queue.  These threads (and the wake queue itself) are already
                // locked, and the locks will be dropped during the final wake operation
                // (below).
                if thread_stop_point(Some(new_requeue_target_owner))
                    != *details.new_requeue_target_owner_stop_point
                {
                    new_owner.lock().assert_acquired(clt_token);
                    // SAFETY: The new requeue owner's chain is locked up to its stop point.
                    unsafe {
                        Self::unlock_pi_chain_common(
                            new_owner,
                            *details.new_requeue_target_owner_stop_point,
                            clt_token,
                        );
                    }
                }
            }
        } else if let Some(owner) = requeue_target_guard.proof().owner_thread() {
            // The owner did not change, so it is the (validated) new requeue target owner.
            debug_assert!(Some(owner) == *details.new_requeue_target_owner);
            // SAFETY: The requeue owner's chain is locked, so it is live.
            let new_owner: &Thread = unsafe { owner.as_ref() };
            new_owner.lock().assert_acquired(clt_token);
            // SAFETY: The requeue owner's chain is locked up to its stop point.
            unsafe {
                Self::unlock_pi_chain_common(
                    new_owner,
                    *details.new_requeue_target_owner_stop_point,
                    clt_token,
                );
            }
        }

        // We are finished with the requeue target and can drop its lock now.
        requeue_target_guard.proof().validate_sched_state_storage();
        requeue_target_guard.release();

        // Finally, step #5.  Wake the threads we identified as needing to be woken
        // during the locking operation, dealing with ownership assignment in the
        // process.
        // SAFETY: `wake_threads` was populated by the locking operation, and the transaction is
        // finalized.
        let wtr = unsafe {
            guard.proof().wake_threads_locked(
                wake_threads,
                wake_hooks,
                wake_option,
                ForceInheritance::No,
                clt_token,
                preempt_disabled_token,
            )
        };
        guard.release();
        wtr
    }
}

/// C++ FFI entry point for `OwnedWaitQueue::LockPiChain(Thread&)`.
///
/// # Safety
///
/// `start` must point to a live [`Thread`] whose lock is held by the active, unfinalized
/// transaction, and the preconditions of [`OwnedWaitQueue::lock_pi_chain_from_thread`] must hold.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_lock_pi_chain_thread(
    start: *mut bindings::Thread,
) -> i32 {
    debug_assert!(!start.is_null());
    // SAFETY: C++ passes a non-null reference (`Thread& start`) to a live `Thread`.
    let thread: &Thread = unsafe { &*start.cast::<Thread>() };
    // SAFETY: The C++ declaration is `TA_REQ(chainlock_transaction_token, ...)`, so a transaction
    // is active on this CPU.
    let token = unsafe { ChainLockTransaction::mark_active() };
    // SAFETY: The C++ declaration is `TA_REQ(start.get_lock())`, and no other Rust proof or guard
    // for `thread` exists.
    let mut proof = unsafe { ChainLockProof::assume_held_unchecked(thread) };
    // SAFETY: The caller upholds the preconditions of `lock_pi_chain_from_thread` (the
    // transaction is not finalized and no downstream locks are held).
    let res = unsafe { OwnedWaitQueue::lock_pi_chain_from_thread(&mut proof, &token) };
    res as i32
}

/// C++ FFI entry point for `OwnedWaitQueue::LockPiChain(WaitQueueBase&)`.
///
/// # Safety
///
/// `start` must point to a live [`WaitQueueBase`] whose lock is held by the active, unfinalized
/// transaction, and the preconditions of [`OwnedWaitQueue::lock_pi_chain_from_wait_queue`] must
/// hold.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_lock_pi_chain_wait_queue(
    start: *mut bindings::WaitQueueBase,
) -> i32 {
    debug_assert!(!start.is_null());
    // SAFETY: C++ passes a non-null reference (`WaitQueueBase& start`) to a live wait queue; the
    // raw pointer carries provenance over the full (possibly `OwnedWaitQueue`) object.
    let wq = unsafe { NonNull::new_unchecked(start.cast::<WaitQueueBase>()) };
    // SAFETY: The C++ declaration is `TA_REQ(chainlock_transaction_token, ...)`, so a transaction
    // is active on this CPU.
    let token = unsafe { ChainLockTransaction::mark_active() };
    // Downcast the raw pointer before forming any reference, so that an `OwnedWaitQueue` is
    // accessed with provenance over the whole object.
    // SAFETY: `wq` points to a live wait queue, with provenance over the enclosing object.
    let res = if let Some(owq) = unsafe { OwnedWaitQueue::downcast_ptr(wq) } {
        // SAFETY: `owq` points to the live `OwnedWaitQueue` which encloses `wq`.
        let owq: &OwnedWaitQueue = unsafe { owq.as_ref() };
        // SAFETY: The C++ declaration is `TA_REQ(start.get_lock())`, and no other Rust proof or
        // guard for `owq` exists.
        let mut proof = unsafe { ChainLockProof::assume_held_unchecked(owq) };
        // SAFETY: The caller upholds the preconditions of `lock_pi_chain` (the transaction is not
        // finalized and no downstream locks are held).
        unsafe { OwnedWaitQueue::lock_pi_chain(&mut proof, &token) }
    } else {
        // SAFETY: `wq` points to a live wait queue.
        let wq: &WaitQueueBase = unsafe { wq.as_ref() };
        // SAFETY: The C++ declaration is `TA_REQ(start.get_lock())`, and no other Rust proof or
        // guard for `wq` exists.
        let mut proof = unsafe { ChainLockProof::assume_held_unchecked(wq) };
        // SAFETY: The caller upholds the preconditions of `lock_pi_chain_from_wait_queue` (the
        // transaction is not finalized and no downstream locks are held), and `wq` is not an
        // `OwnedWaitQueue`.
        unsafe { OwnedWaitQueue::lock_pi_chain_from_wait_queue(&mut proof, &token) }
    };
    res as i32
}

/// C++ FFI entry point for `OwnedWaitQueue::AssertNotOwned()`.
///
/// # Safety
///
/// `wait_queue` must point to a live [`OwnedWaitQueue`] which satisfies the preconditions of
/// [`OwnedWaitQueue::assert_not_owned`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_assert_not_owned(
    wait_queue: *const bindings::OwnedWaitQueue,
) {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: The C++ caller (`TA_NO_THREAD_SAFETY_ANALYSIS`) guarantees that `owner_` cannot be
    // concurrently mutated (the lock is held, or the queue is quiescent).
    unsafe { owq.assert_not_owned() };
}

/// C++ FFI entry point for `OwnedWaitQueue::AssertSafeForPooling()`.
///
/// # Safety
///
/// `wait_queue` must point to a live [`OwnedWaitQueue`] which satisfies the preconditions of
/// [`OwnedWaitQueue::assert_safe_for_pooling`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_assert_safe_for_pooling(
    wait_queue: *const bindings::OwnedWaitQueue,
) {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: The C++ caller (`TA_NO_THREAD_SAFETY_ANALYSIS`) guarantees that the queue cannot be
    // concurrently mutated (it is quiescent while being returned to the pool).
    unsafe { owq.assert_safe_for_pooling() };
}

/// C++ FFI entry point for `OwnedWaitQueue::~OwnedWaitQueue()`.
///
/// # Safety
///
/// `wait_queue` must point to a live [`OwnedWaitQueue`] which is being destroyed with no
/// concurrent accesses.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_destroy(wait_queue: *mut bindings::OwnedWaitQueue) {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: C++ passes `this` from the destructor, a non-null pointer to a live
    // `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: An object being destroyed by C++ is not concurrently accessed.
    unsafe { owq.teardown() };
}

/// C++ FFI entry point for `OwnedWaitQueue::DisownAllQueues()`.
///
/// # Safety
///
/// `t` must point to a live [`Thread`] satisfying the preconditions of
/// [`OwnedWaitQueue::disown_all_queues`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_disown_all_queues(t: *mut bindings::Thread) {
    debug_assert!(!t.is_null());
    // SAFETY: C++ passes a non-null pointer to a live `Thread`.
    let thread: &Thread = unsafe { &*t.cast::<Thread>() };
    // SAFETY: The C++ declaration is `TA_EXCL(chainlock_transaction_token, t->get_lock())`, and
    // the caller (the exiting thread) upholds the remaining preconditions of
    // `disown_all_queues`.
    unsafe { OwnedWaitQueue::disown_all_queues(thread) };
}

/// C++ FFI entry point for `OwnedWaitQueue::GetEffectiveProfile()`.
///
/// # Safety
///
/// `wait_queue` must point to a live [`OwnedWaitQueue`] whose lock is held by the active
/// transaction, and `out_profile` must be valid for writes of an `EffectiveProfile`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_get_effective_profile(
    wait_queue: *const bindings::OwnedWaitQueue,
    out_profile: *mut bindings::SchedulerState_EffectiveProfile,
) {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!out_profile.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: The C++ declaration is `TA_REQ(get_lock())`, and the proof is only used for reads.
    let proof = unsafe { ChainLockProof::assume_held_unchecked(owq) };
    // SAFETY: `out_profile` points to a C++ `SchedulerState::EffectiveProfile` in the caller's
    // frame, which has the same layout as `EffectiveProfile`.
    unsafe { out_profile.cast::<EffectiveProfile>().write(proof.get_effective_profile()) };
}

/// C++ FFI entry point for `OwnedWaitQueue::SetThreadBaseProfileAndPropagate()`.
///
/// # Safety
///
/// `thread` and `profile` must point to a live [`Thread`] and [`BaseProfile`], and no
/// [`ChainLockTransaction`] may be active on the current CPU.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_set_thread_base_profile_and_propagate(
    thread: *mut bindings::Thread,
    profile: *const bindings::SchedulerState_BaseProfile,
) {
    debug_assert!(!thread.is_null());
    debug_assert!(!profile.is_null());
    // SAFETY: C++ passes a non-null reference (`Thread& thread`) to a live `Thread`.
    let thread: &Thread = unsafe { &*thread.cast::<Thread>() };
    // SAFETY: C++ passes a non-null reference to a live `SchedulerState::BaseProfile`, which has
    // the same layout as `BaseProfile`.
    let profile: &BaseProfile = unsafe { &*profile.cast::<BaseProfile>() };
    // `set_thread_base_profile_and_propagate` is safe; it asserts that no transaction is active
    // (`TA_EXCL(chainlock_transaction_token, thread.get_lock())` in C++).
    OwnedWaitQueue::set_thread_base_profile_and_propagate(thread, profile);
}

/// C++ FFI entry point for `OwnedWaitQueue::ResetOwnerIfNoWaiters()`.
///
/// # Safety
///
/// `wait_queue` must point to a live [`OwnedWaitQueue`], and no [`ChainLockTransaction`] may be
/// active on the current CPU.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_reset_owner_if_no_waiters(
    wait_queue: *mut bindings::OwnedWaitQueue,
) {
    debug_assert!(!wait_queue.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // `reset_owner_if_no_waiters` is safe; it asserts that no transaction is active (`TA_EXCL` in
    // C++).
    owq.reset_owner_if_no_waiters();
}

/// C++ FFI entry point for `OwnedWaitQueue::BlockAndAssignOwner()`.
///
/// # Safety
///
/// `wait_queue` and `deadline` must point to a live [`OwnedWaitQueue`] and [`Deadline`],
/// `new_owner` must be null or point to a live [`Thread`], preemption must be disabled, and the
/// preconditions of [`OwnedWaitQueue::block_and_assign_owner`] must hold.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_block_and_assign_owner(
    wait_queue: *mut bindings::OwnedWaitQueue,
    deadline: *const bindings::Deadline,
    new_owner: *mut bindings::Thread,
    resource_ownership: ResourceOwnership,
    interruptible: Interruptible,
) -> zx_status_t {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!deadline.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: C++ passes a non-null reference (`const Deadline&`) to a live `Deadline`.
    let deadline: &Deadline = unsafe { &*deadline.cast::<Deadline>() };
    // SAFETY: C++ passes null or a pointer to a live `Thread`.
    let new_owner: Option<&Thread> = unsafe { new_owner.cast::<Thread>().as_ref() };
    // SAFETY: The C++ declaration is `TA_REQ(preempt_disabled_token)`.
    let preempt_disabled = unsafe { PreemptDisabledToken::new() };
    // SAFETY: The C++ declaration is `TA_EXCL(chainlock_transaction_token)`, and the caller (the
    // current thread) upholds the remaining blocking preconditions.
    let res = unsafe {
        owq.block_and_assign_owner(
            deadline,
            new_owner,
            resource_ownership,
            interruptible,
            &preempt_disabled,
        )
    };
    Status::result_into_raw(res)
}

/// C++ FFI entry point for `OwnedWaitQueue::BlockAndAssignOwnerLocked()`.
///
/// # Safety
///
/// `wait_queue`, `current_thread`, `deadline`, and `details` must point to a live
/// [`OwnedWaitQueue`], [`Thread`], [`Deadline`], and [`BAAOLockingDetails`], an active finalized
/// transaction must be in progress with preemption disabled, and the preconditions of
/// [`OwnedWaitQueue::block_and_assign_owner_locked`] must hold (`wait_queue`'s lock is released
/// by this call).
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_block_and_assign_owner_locked(
    wait_queue: *mut bindings::OwnedWaitQueue,
    current_thread: *mut bindings::Thread,
    deadline: *const bindings::Deadline,
    details: *const BAAOLockingDetails,
    resource_ownership: ResourceOwnership,
    interruptible: Interruptible,
    force_inheritance: ForceInheritance,
) -> zx_status_t {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!current_thread.is_null());
    debug_assert!(!deadline.is_null());
    debug_assert!(!details.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: C++ passes a non-null pointer to the live, currently running `Thread`.
    let thread: &Thread = unsafe { &*current_thread.cast::<Thread>() };
    // SAFETY: C++ passes a non-null reference (`const Deadline&`) to a live `Deadline`.
    let deadline: &Deadline = unsafe { &*deadline.cast::<Deadline>() };
    // SAFETY: C++ passes a non-null reference to a live `BAAOLockingDetails` in the caller's
    // frame, which has the same layout as the C++ type.
    let details: &BAAOLockingDetails = unsafe { &*details };
    // SAFETY: The C++ declaration is `TA_REQ(chainlock_transaction_token, ...)`.
    let token = unsafe { ChainLockTransaction::mark_active() };
    // SAFETY: The C++ declaration is `TA_REQ(preempt_disabled_token)`.
    let preempt_disabled = unsafe { PreemptDisabledToken::new() };
    // SAFETY: The C++ declaration is `TA_REL(get_lock())`: the queue lock is held on entry and
    // released by this call, so ownership of it is transferred to the guard.
    let guard = unsafe { OwnerGuard::adopt_unchecked(owq, &token) };
    // SAFETY: The C++ declaration is `TA_REQ(current_thread->get_lock())`, and no other Rust proof
    // or guard for `thread` exists.
    let mut t_proof = unsafe { ChainLockProof::assume_held_unchecked(thread) };
    // SAFETY: The caller upholds the remaining preconditions of `block_and_assign_owner_locked`
    // (`details` came from a successful BAAO lock operation in this finalized transaction).
    let res = unsafe {
        OwnedWaitQueue::block_and_assign_owner_locked(
            guard,
            &mut t_proof,
            deadline,
            details,
            resource_ownership,
            interruptible,
            force_inheritance,
            &token,
            &preempt_disabled,
        )
    };
    Status::result_into_raw(res)
}

/// C++ FFI entry point for `OwnedWaitQueue::TryLockForBAAOOperationLocked()`.
///
/// # Safety
///
/// `wait_queue`, `current_thread`, and `details_out` must point to a live [`OwnedWaitQueue`]
/// (whose lock is held by the active transaction), [`Thread`], and writable
/// [`BAAOLockingDetails`], `new_owner` must be null or point to a live [`Thread`], and the
/// preconditions of [`ChainLockProof::try_lock_for_baao_operation_locked`] must hold. On success,
/// the caller takes responsibility for the acquired locks.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_try_lock_for_baao_operation_locked(
    wait_queue: *mut bindings::OwnedWaitQueue,
    current_thread: *mut bindings::Thread,
    new_owner: *mut bindings::Thread,
    details_out: *mut BAAOLockingDetails,
) -> bool {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!current_thread.is_null());
    debug_assert!(!details_out.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: C++ passes a non-null pointer to the live, currently running `Thread`.
    let thread: &Thread = unsafe { &*current_thread.cast::<Thread>() };
    // SAFETY: C++ passes null or a pointer to a live `Thread`.
    let new_owner: Option<&Thread> = unsafe { new_owner.cast::<Thread>().as_ref() };
    // SAFETY: The C++ declaration is `TA_REQ(chainlock_transaction_token, ...)`.
    let token = unsafe { ChainLockTransaction::mark_active() };
    // SAFETY: The C++ declaration is `TA_REQ(get_lock())`, and no other Rust proof or guard for
    // `owq` exists.
    let mut proof = unsafe { ChainLockProof::assume_held_unchecked(owq) };
    // SAFETY: The caller upholds the preconditions of `try_lock_for_baao_operation_locked` (the
    // transaction is not finalized and the current thread's and PI-chain locks are not held).
    let locked = unsafe { proof.try_lock_for_baao_operation_locked(thread, new_owner, &token) };
    let Some((details, thread_guard)) = locked else {
        return false;
    };
    // Ownership of the acquired locks passes to the C++ caller (`TA_TRY_ACQ(true,
    // current_thread->get_lock())`).
    let _ = thread_guard.into_unguarded();
    // SAFETY: `details_out` points to a writable `BAAOLockingDetails` in the caller's frame.
    unsafe { details_out.write(details) };
    true
}

/// C++ FFI entry point for `OwnedWaitQueue::WakeThreads()`.
///
/// # Safety
///
/// `wait_queue`, `wake_hooks`, and `out_result` must point to a live [`OwnedWaitQueue`], a live
/// C++ `OwnedWaitQueue::IWakeRequeueHook`, and a writable [`WakeThreadsResult`], and no
/// [`ChainLockTransaction`] may be active on the current CPU.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_wake_threads(
    wait_queue: *mut bindings::OwnedWaitQueue,
    wake_count: u32,
    wake_hooks: *mut bindings::OwnedWaitQueue_IWakeRequeueHook,
    out_result: *mut WakeThreadsResult,
) {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!out_result.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: C++ passes a reference (`IWakeRequeueHook&`) to a live hook which outlives the call.
    let mut wake_hooks = unsafe { CppWakeRequeueHook::new(wake_hooks) };
    // `wake_threads` is safe; it asserts that no transaction is active (`TA_EXCL` in C++).
    let result = owq.wake_threads(wake_count, &mut wake_hooks);
    // SAFETY: `out_result` points to a writable `WakeThreadsResult` in the caller's frame.
    unsafe { out_result.write(result) };
}

/// C++ FFI entry point for `OwnedWaitQueue::WakeThreadAndAssignOwner()`.
///
/// # Safety
///
/// `wait_queue`, `wake_hooks`, and `out_result` must point to a live [`OwnedWaitQueue`], a live
/// C++ `OwnedWaitQueue::IWakeRequeueHook`, and a writable [`WakeThreadsResult`], and no
/// [`ChainLockTransaction`] may be active on the current CPU.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_wake_thread_and_assign_owner(
    wait_queue: *mut bindings::OwnedWaitQueue,
    wake_hooks: *mut bindings::OwnedWaitQueue_IWakeRequeueHook,
    out_result: *mut WakeThreadsResult,
) {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!out_result.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: C++ passes a reference (`IWakeRequeueHook&`) to a live hook which outlives the call.
    let mut wake_hooks = unsafe { CppWakeRequeueHook::new(wake_hooks) };
    // `wake_thread_and_assign_owner` is safe; it asserts that no transaction is active
    // (`TA_EXCL` in C++).
    let result = owq.wake_thread_and_assign_owner(&mut wake_hooks);
    // SAFETY: `out_result` points to a writable `WakeThreadsResult` in the caller's frame.
    unsafe { out_result.write(result) };
}

/// C++ FFI entry point for `OwnedWaitQueue::WakeThreadsLocked()`.
///
/// # Safety
///
/// `wait_queue`, `threads`, `wake_hooks`, and `out_result` must point to a live
/// [`OwnedWaitQueue`] (whose lock is held by the active, finalized transaction with preemption
/// disabled), a live `Thread::UnblockList`, a live C++ `OwnedWaitQueue::IWakeRequeueHook`, and a
/// writable [`WakeThreadsResult`], and the preconditions of
/// [`ChainLockProof::wake_threads_locked`] must hold.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_wake_threads_locked(
    wait_queue: *mut bindings::OwnedWaitQueue,
    threads: *mut bindings::Thread_UnblockList,
    wake_hooks: *mut bindings::OwnedWaitQueue_IWakeRequeueHook,
    option: WakeOption,
    force_inheritance: ForceInheritance,
    out_result: *mut WakeThreadsResult,
) {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!threads.is_null());
    debug_assert!(!out_result.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: The C++ `Thread::UnblockList` (the by-value parameter of `WakeThreadsLocked`)
    // lives in the caller's frame and is not moved for the duration of the call, no other
    // reference to it exists, and the C++ caller runs `~UnblockList` (the equivalent of
    // `ThreadUnblockList`'s `PinnedDrop`) before its storage is reused.
    let threads = unsafe { Pin::new_unchecked(&mut *threads.cast::<ThreadUnblockList>()) };
    // SAFETY: C++ passes a reference (`IWakeRequeueHook&`) to a live hook which outlives the call.
    let mut wake_hooks = unsafe { CppWakeRequeueHook::new(wake_hooks) };
    // SAFETY: The C++ declaration is `TA_REQ(chainlock_transaction_token, ...)`.
    let token = unsafe { ChainLockTransaction::mark_active() };
    // SAFETY: The C++ declaration is `TA_REQ(preempt_disabled_token)`.
    let preempt_disabled = unsafe { PreemptDisabledToken::new() };
    // SAFETY: The C++ declaration is `TA_REQ(get_lock())`, and no other Rust proof or guard for
    // `owq` exists.
    let mut proof = unsafe { ChainLockProof::assume_held_unchecked(owq) };
    // SAFETY: The caller upholds the preconditions of `wake_threads_locked` (`threads` came from
    // a successful lock-for-wake operation in this transaction, which has been finalized).
    let result = unsafe {
        proof.wake_threads_locked(
            threads,
            &mut wake_hooks,
            option,
            force_inheritance,
            &token,
            &preempt_disabled,
        )
    };
    // SAFETY: `out_result` points to a writable `WakeThreadsResult` in the caller's frame.
    unsafe { out_result.write(result) };
}

/// C++ FFI entry point for `OwnedWaitQueue::LockForWakeOperationOrBackoff()`.
///
/// # Safety
///
/// `wait_queue`, `wake_hooks`, and `unblock_list_out` must point to a live [`OwnedWaitQueue`], a
/// live C++ `OwnedWaitQueue::IWakeRequeueHook`, and a live empty `Thread::UnblockList`, and the
/// preconditions of [`OwnedWaitQueue::lock_for_wake_operation_or_backoff`] must hold. On success,
/// the caller takes responsibility for `wait_queue`'s lock.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_lock_for_wake_operation_or_backoff(
    wait_queue: *mut bindings::OwnedWaitQueue,
    max_wake: u32,
    wake_hooks: *mut bindings::OwnedWaitQueue_IWakeRequeueHook,
    unblock_list_out: *mut bindings::Thread_UnblockList,
) -> bool {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!unblock_list_out.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: The C++ `Thread::UnblockList` (the `unblock_list_out` reference parameter) lives in
    // the caller's frame and is not moved for the duration of the call, no other reference to it
    // exists, and the C++ caller runs `~UnblockList` (the equivalent of `ThreadUnblockList`'s
    // `PinnedDrop`) before its storage is reused.
    let unblock_list =
        unsafe { Pin::new_unchecked(&mut *unblock_list_out.cast::<ThreadUnblockList>()) };
    // SAFETY: C++ passes a reference (`IWakeRequeueHook&`) to a live hook which outlives the call.
    let mut wake_hooks = unsafe { CppWakeRequeueHook::new(wake_hooks) };
    // SAFETY: The C++ declaration is `TA_REQ(chainlock_transaction_token)`.
    let token = unsafe { ChainLockTransaction::mark_active() };
    // SAFETY: The caller upholds the preconditions of `lock_for_wake_operation_or_backoff` (the
    // transaction is not finalized, no locks are held, and `unblock_list` is empty).
    let locked = unsafe {
        owq.lock_for_wake_operation_or_backoff(max_wake, &mut wake_hooks, unblock_list, &token)
    };
    let Some(guard) = locked else {
        return false;
    };
    // Ownership of the queue lock passes to the C++ caller (`TA_TRY_ACQ(true, get_lock())`).
    let _ = guard.into_unguarded();
    true
}

/// C++ FFI entry point for `OwnedWaitQueue::LockForWakeOperationLocked()`.
///
/// # Safety
///
/// `wait_queue`, `wake_hooks`, and `unblock_list_out` must point to a live [`OwnedWaitQueue`]
/// (whose lock is held by the active transaction), a live C++
/// `OwnedWaitQueue::IWakeRequeueHook`, and a live empty `Thread::UnblockList`, and the
/// preconditions of [`ChainLockProof::lock_for_wake_operation_locked`] must hold.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_lock_for_wake_operation_locked(
    wait_queue: *mut bindings::OwnedWaitQueue,
    max_wake: u32,
    wake_hooks: *mut bindings::OwnedWaitQueue_IWakeRequeueHook,
    unblock_list_out: *mut bindings::Thread_UnblockList,
) -> bool {
    debug_assert!(!wait_queue.is_null());
    debug_assert!(!unblock_list_out.is_null());
    // SAFETY: C++ passes `this`, a non-null pointer to a live `OwnedWaitQueue`.
    let owq: &OwnedWaitQueue = unsafe { &*wait_queue.cast::<OwnedWaitQueue>() };
    // SAFETY: The C++ `Thread::UnblockList` (a local in the C++ caller's frame) is not moved for
    // the duration of the call, no other reference to it exists, and the C++ caller runs
    // `~UnblockList` (the equivalent of `ThreadUnblockList`'s `PinnedDrop`) before its storage is
    // reused.
    let unblock_list =
        unsafe { Pin::new_unchecked(&mut *unblock_list_out.cast::<ThreadUnblockList>()) };
    // SAFETY: C++ passes a reference (`IWakeRequeueHook&`) to a live hook which outlives the call.
    let mut hooks = unsafe { CppWakeRequeueHook::new(wake_hooks) };
    // SAFETY: The C++ declaration is `TA_REQ(chainlock_transaction_token, ...)`.
    let token = unsafe { ChainLockTransaction::mark_active() };
    // SAFETY: The C++ declaration is `TA_REQ(get_lock())`, and no other Rust proof or guard for
    // `owq` exists.
    let mut proof = unsafe { ChainLockProof::assume_held_unchecked(owq) };
    // SAFETY: The caller upholds the preconditions of `lock_for_wake_operation_locked` (the
    // transaction is not finalized, no downstream locks are held, and `unblock_list` is empty).
    unsafe { proof.lock_for_wake_operation_locked(max_wake, &mut hooks, unblock_list, &token) }
}

/// Unit tests for the Rust `OwnedWaitQueue` implementation.
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
    use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
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

    /// Blocks the current thread in `owq` using `block_and_assign_owner`, proposing `new_owner` as
    /// the queue's owner.
    fn block_current_thread_unlocked(
        owq: &OwnedWaitQueue,
        new_owner: Option<&Thread>,
        deadline: Deadline,
    ) -> Result<(), Status> {
        let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        // SAFETY: Preemption is disabled by `_eager_resched_disabler`.
        let preempt_token = unsafe { PreemptDisabledToken::new() };
        // SAFETY: No transaction is active, the current thread may block, and `owq` (and
        // `new_owner`, if any) are kept alive by the test until this call returns.
        unsafe {
            owq.block_and_assign_owner(
                &deadline,
                new_owner,
                ResourceOwnership::Normal,
                Interruptible::No,
                &preempt_token,
            )
        }
    }

    /// The API a waiter thread uses to block in its queue.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum BlockMode {
        /// `lock_for_baao_operation_or_backoff` and `block_and_assign_owner_locked`.
        Locked,
        /// `block_and_assign_owner`.
        Unlocked,
    }

    struct WaiterContext<'a> {
        owq: &'a OwnedWaitQueue,
        new_owner: Option<ThreadPtr>,
        mode: BlockMode,
        started: AtomicBool,
    }

    impl<'a> WaiterContext<'a> {
        fn new(owq: &'a OwnedWaitQueue, new_owner: Option<ThreadPtr>) -> Self {
            Self::with_mode(owq, new_owner, BlockMode::Locked)
        }

        fn with_mode(
            owq: &'a OwnedWaitQueue,
            new_owner: Option<ThreadPtr>,
            mode: BlockMode,
        ) -> Self {
            Self { owq, new_owner, mode, started: AtomicBool::new(false) }
        }
    }

    extern "C" fn waiter_entry(arg: *mut c_void) -> i32 {
        // SAFETY: `arg` points to a `WaiterContext` which outlives this thread, since the test
        // joins the thread before the context is destroyed.
        let ctx = unsafe { &*arg.cast::<WaiterContext<'_>>() };
        ctx.started.store(true, Ordering::SeqCst);
        // SAFETY: `new_owner` (if `Some`) is kept alive by the test while the waiter runs.
        let new_owner = unsafe { ctx.new_owner.map(|t| t.as_ref()) };
        let result = match ctx.mode {
            BlockMode::Locked => block_current_thread(ctx.owq, new_owner, Deadline::infinite()),
            BlockMode::Unlocked => {
                block_current_thread_unlocked(ctx.owq, new_owner, Deadline::infinite())
            }
        };
        match result {
            Ok(()) => 0,
            Err(status) => status.into_raw(),
        }
    }

    /// The longest time to wait for a waiter thread to start and block.
    const WAITER_BLOCK_TIMEOUT: DurationMono = DurationMono::from_seconds(10);

    /// A waiter thread, which blocks (or is about to block) in a queue.
    ///
    /// Unless it is explicitly joined, dropping a `Waiter` (for example, when an assertion returns
    /// early from a test) wakes every thread blocked in `cleanup_queues` and joins the waiter, so
    /// that it never outlives its `WaiterContext`, and the test thread does not keep a profile it
    /// inherited from the waiter.
    struct Waiter<'a> {
        thread: ThreadPtr,
        cleanup_queues: &'a [&'a OwnedWaitQueue],
        joined: bool,
    }

    impl Waiter<'_> {
        /// Returns the waiter's thread.
        fn thread(&self) -> ThreadPtr {
            self.thread
        }

        /// Joins the waiter, which must already have been woken, returning its exit code.
        fn join(mut self) -> Result<i32, Status> {
            self.joined = true;
            // SAFETY: `self.thread` has not been joined yet (`self.joined` was `false`).
            unsafe { self.thread.join(InstantMono::INFINITE) }
        }
    }

    impl Drop for Waiter<'_> {
        fn drop(&mut self) {
            if self.joined {
                return;
            }
            // The waiter may not have blocked yet, so retry until it has been woken and exits.
            loop {
                for owq in self.cleanup_queues {
                    let _ = owq.wake_threads(u32::MAX, &mut DefaultWakeHooks);
                }
                let deadline = current_mono_time() + DurationMono::from_millis(10);
                // SAFETY: `self.thread` has not been joined yet, and a join which times out may be
                // retried.
                if unsafe { self.thread.join(deadline) } != Err(Status::TIMED_OUT) {
                    break;
                }
            }
        }
    }

    /// Spawns a thread which blocks in `ctx.owq`, and waits (for up to `WAITER_BLOCK_TIMEOUT`)
    /// until it has blocked. If it does not block in time, the waiter is cleaned up (see
    /// [`Waiter`]) and `Status::TIMED_OUT` is returned.
    ///
    /// # Safety
    ///
    /// The returned `Waiter` must be dropped or joined before `ctx` is destroyed, and
    /// `cleanup_queues` must contain every queue which the waiter may block in.
    unsafe fn spawn_waiter<'a>(
        ctx: &WaiterContext<'_>,
        cleanup_queues: &'a [&'a OwnedWaitQueue],
    ) -> Result<Waiter<'a>, Status> {
        let arg = core::ptr::from_ref(ctx).cast_mut().cast::<c_void>();
        // SAFETY: The thread is joined (through the returned `Waiter`, which is also dropped on
        // failure below) before `ctx` is destroyed.
        let thread = unsafe { thread::spawn(c"owq_rust_waiter".as_ptr(), waiter_entry, arg)? };
        let waiter = Waiter { thread, cleanup_queues, joined: false };
        let deadline = current_mono_time() + WAITER_BLOCK_TIMEOUT;
        // Nothing else blocks the waiter between setting `started` and blocking in `ctx.owq`.
        // SAFETY: `thread` has not been joined yet.
        while !ctx.started.load(Ordering::SeqCst) || !unsafe { thread.is_blocked() } {
            if current_mono_time() >= deadline {
                return Err(Status::TIMED_OUT);
            }
            thread::r#yield();
        }
        Ok(waiter)
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
        let owq_ref: &OwnedWaitQueue = &owq;
        let queues = [owq_ref];
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };

        // Block a waiter which declares the test thread to be the owner of the queue.
        let ctx_a = WaiterContext::new(&owq, Some(current));
        // SAFETY: `waiter_a` is dropped (or joined) before `ctx_a`, and only blocks in `owq`.
        let waiter_a = unwrap_ok!(unsafe { spawn_waiter(&ctx_a, &queues) });
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
        // SAFETY: `waiter_b` is dropped (or joined) before `ctx_b`, and only blocks in `owq`.
        let waiter_b = unwrap_ok!(unsafe { spawn_waiter(&ctx_b, &queues) });

        // Wake one thread and make it the owner, since another thread is still waiting.
        let mut assign_hook = CountHook::default();
        let res = owq.wake_thread_and_assign_owner(&mut assign_hook);
        assert_eq!(res.woken, 1);
        assert_eq!(res.still_waiting, 1);
        assert_true!(assign_hook.allow_count >= 1);
        assert_eq!(assign_hook.wake_count, 1);
        assert_true!(res.owner.is_some());
        assert_true!(res.owner == assign_hook.last_thread);
        assert_true!(res.owner == Some(waiter_a.thread()) || res.owner == Some(waiter_b.thread()));

        // Wake the remaining thread, which removes any remaining owner.
        let mut wake_hook = CountHook::default();
        let res = owq.wake_threads(1, &mut wake_hook);
        assert_true!(res == WakeThreadsResult { woken: 1, still_waiting: 0, owner: None });
        assert_eq!(wake_hook.wake_count, 1);
        assert_true!(wake_hook.last_thread.is_some());
        assert_true!(wake_hook.last_thread != assign_hook.last_thread);

        assert_eq!(unwrap_ok!(waiter_a.join()), 0);
        assert_eq!(unwrap_ok!(waiter_b.join()), 0);
        assert_true!(observe_owner(&owq).is_none());
    }

    /// Verifies requeueing a BAAO-blocked thread to an owned queue, then waking it from there.
    #[test]
    fn baao_block_requeue_and_wake() {
        stack_pin_init!(let owq1 = OwnedWaitQueue::init());
        stack_pin_init!(let owq2 = OwnedWaitQueue::init());
        let queues = [&*owq1, &*owq2];
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };

        let ctx = WaiterContext::new(&owq1, None);
        // SAFETY: `waiter` is dropped (or joined) before `ctx`, and only blocks in `owq1` or
        // `owq2`.
        let waiter = unwrap_ok!(unsafe { spawn_waiter(&ctx, &queues) });

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
        assert_true!(requeue_hook.last_thread == Some(waiter.thread()));
        assert_true!(observe_owner(&owq2) == Some(current));

        let mut hook = CountHook::default();
        let res = owq2.wake_threads(1, &mut hook);
        assert_true!(res == WakeThreadsResult { woken: 1, still_waiting: 0, owner: None });
        assert_eq!(hook.wake_count, 1);
        assert_true!(hook.last_thread == Some(waiter.thread()));

        assert_eq!(unwrap_ok!(waiter.join()), 0);
        assert_true!(observe_owner(&owq1).is_none());
        assert_true!(observe_owner(&owq2).is_none());
    }

    /// Verifies the non-locked `block_and_assign_owner`, with expired and infinite deadlines.
    #[test]
    fn unlocked_baao_block_and_wake() {
        stack_pin_init!(let owq = OwnedWaitQueue::init());
        let owq_ref: &OwnedWaitQueue = &owq;
        let queues = [owq_ref];
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };

        assert_err!(
            block_current_thread_unlocked(owq_ref, None, Deadline::infinite_past()),
            Status::TIMED_OUT
        );
        assert_true!(observe_owner(owq_ref).is_none());

        // Block a waiter which declares the test thread to be the owner of the queue.
        let ctx = WaiterContext::with_mode(owq_ref, Some(current), BlockMode::Unlocked);
        // SAFETY: `waiter` is dropped (or joined) before `ctx`, and only blocks in `owq`.
        let waiter = unwrap_ok!(unsafe { spawn_waiter(&ctx, &queues) });
        assert_true!(observe_owner(owq_ref) == Some(current));

        let mut hook = CountHook::default();
        let res = owq_ref.wake_threads(1, &mut hook);
        assert_true!(res == WakeThreadsResult { woken: 1, still_waiting: 0, owner: None });
        assert_eq!(hook.wake_count, 1);
        assert_true!(hook.last_thread == Some(waiter.thread()));

        assert_eq!(unwrap_ok!(waiter.join()), 0);
        assert_true!(observe_owner(owq_ref).is_none());
    }

    /// Verifies `wake_and_requeue` waking and requeueing into a queue keeping its existing owner.
    #[test]
    fn wake_and_requeue_with_requeue_target_owner() {
        stack_pin_init!(let owq1 = OwnedWaitQueue::init());
        stack_pin_init!(let owq2 = OwnedWaitQueue::init());
        let owq1_ref: &OwnedWaitQueue = &owq1;
        let owq2_ref: &OwnedWaitQueue = &owq2;
        let queues = [owq1_ref, owq2_ref];
        // SAFETY: Called from a running kernel thread.
        let current = unsafe { ThreadPtr::current() };
        // SAFETY: `current` is the currently executing thread.
        let current_ref = unsafe { current.as_ref() };

        // Block two waiters in `owq1` without an owner, and one in `owq2` which declares the test
        // thread to be the owner of `owq2`.
        let ctx_a = WaiterContext::new(owq1_ref, None);
        // SAFETY: `waiter_a` is dropped (or joined) before `ctx_a`, and only blocks in `owq1` or
        // `owq2`.
        let waiter_a = unwrap_ok!(unsafe { spawn_waiter(&ctx_a, &queues) });
        let ctx_b = WaiterContext::new(owq1_ref, None);
        // SAFETY: As above.
        let waiter_b = unwrap_ok!(unsafe { spawn_waiter(&ctx_b, &queues) });
        let ctx_c = WaiterContext::new(owq2_ref, Some(current));
        // SAFETY: As above.
        let waiter_c = unwrap_ok!(unsafe { spawn_waiter(&ctx_c, &queues) });
        assert_true!(observe_owner(owq1_ref).is_none());
        assert_true!(observe_owner(owq2_ref) == Some(current));

        // Wake one waiter from `owq1`, and requeue the other one to `owq2`, proposing the existing
        // owner of `owq2` as its owner.
        let mut wake_hook = CountHook::default();
        let mut requeue_hook = CountHook::default();
        let res = owq1_ref.wake_and_requeue(
            owq2_ref,
            Some(current_ref),
            1,
            1,
            &mut wake_hook,
            &mut requeue_hook,
            WakeOption::AssignOwner,
        );
        // No waiters remain in `owq1`, so the woken thread does not become its owner.
        assert_true!(res == WakeThreadsResult { woken: 1, still_waiting: 0, owner: None });
        assert_eq!(wake_hook.wake_count, 1);
        assert_eq!(requeue_hook.wake_count, 1);
        let (a, b) = (Some(waiter_a.thread()), Some(waiter_b.thread()));
        assert_true!(
            (wake_hook.last_thread == a && requeue_hook.last_thread == b)
                || (wake_hook.last_thread == b && requeue_hook.last_thread == a)
        );
        assert_true!(observe_owner(owq1_ref).is_none());
        // The owner of `owq2` did not change.
        assert_true!(observe_owner(owq2_ref) == Some(current));

        // Wake the requeued waiter and the original waiter from `owq2`, which removes its owner.
        let mut hook = CountHook::default();
        let res = owq2_ref.wake_threads(u32::MAX, &mut hook);
        assert_true!(res == WakeThreadsResult { woken: 2, still_waiting: 0, owner: None });
        assert_eq!(hook.wake_count, 2);
        assert_true!(observe_owner(owq2_ref).is_none());

        assert_eq!(unwrap_ok!(waiter_a.join()), 0);
        assert_eq!(unwrap_ok!(waiter_b.join()), 0);
        assert_eq!(unwrap_ok!(waiter_c.join()), 0);
    }

    /// Verifies `cancel_baao_operation_locked` and waking waiters via locked wake operations.
    #[test]
    fn locked_wake_and_cancel_baao() {
        stack_pin_init!(let owq = OwnedWaitQueue::init());
        let owq_ref: &OwnedWaitQueue = &owq;
        let queues = [owq_ref];
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
        // SAFETY: `waiter_a` is dropped (or joined) before `ctx_a`, and only blocks in `owq`.
        let waiter_a = unwrap_ok!(unsafe { spawn_waiter(&ctx_a, &queues) });
        let ctx_b = WaiterContext::new(owq_ref, None);
        // SAFETY: `waiter_b` is dropped (or joined) before `ctx_b`, and only blocks in `owq`.
        let waiter_b = unwrap_ok!(unsafe { spawn_waiter(&ctx_b, &queues) });

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

        assert_eq!(unwrap_ok!(waiter_a.join()), 0);
        assert_eq!(unwrap_ok!(waiter_b.join()), 0);
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
