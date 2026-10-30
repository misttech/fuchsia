// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Priority inheritance tests for [`OwnedWaitQueue`].

use crate::kconcurrent::{
    ChainLockTransaction, ChainLocked, ClassedChainLock, IRQ_SAVE_OPTION, OwnerGuard,
    PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION, TransactionResult, acquire_both_or_backoff, clt_tag,
    single_chain_lock_guard_owned,
};
use crate::kernel::deadline::Deadline;
use crate::kernel::event::Event;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::kernel::scheduler::{
    BaseProfile, EffectiveProfile, InheritedProfileValues, SchedDeadlineParams, SchedDiscipline,
    SchedDuration, SchedTime, SchedUtilization, SchedWeight, SchedulerState,
    WaitQueueInheritedSchedulerState,
};
use crate::kernel::scheduler_state::{
    DEFAULT_PRIORITY, HIGHEST_PRIORITY, LOWEST_PRIORITY, ThreadStateKind,
};
use crate::kernel::thread::{
    AnnotatedAutoEagerReschedDisabler, PreemptDisabledToken, THREAD_SIGNAL_KILL, Thread, ThreadPtr,
    create_with_priority, current_get, sleep_relative, r#yield,
};
use crate::kernel::wait_queue::{Interruptible, ResourceOwnership, WaitQueue, WaitQueueLockClass};
use crate::platform_rs::timer::{
    DurationMono, InstantMono, current_mono_time, zx_msec, zx_sec, zx_usec,
};
use core::cell::{Cell, RefCell, UnsafeCell};
use core::convert::Infallible;
use core::ffi::{c_int, c_void};
use core::fmt::Write;
use core::mem::{MaybeUninit, align_of, size_of};
use core::ops::Deref;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use fbl::{Recyclable, RefPtr, UninitRecyclable, UniquePtr, pin_make_ref_counted, ref_counted};
use kprint::kprintln;
use lazy_init::LazyInit;
use pin_init::{
    InPlaceWrite as _, PinInit, pin_data, pin_init, pin_init_array_from_fn, stack_pin_init,
};
use unittest::{
    assert_eq, assert_false, assert_le, assert_lt, assert_true, expect_eq, expect_false, expect_gt,
    expect_lt, expect_ne, expect_true, subtest,
};
use zx_status::Status;
use zx_types::{zx_duration_mono_t, zx_instant_mono_t};

unsafe extern "C" {
    /// Returns a pseudo-random integer using the state pointed to by `seed`.
    ///
    /// # Safety
    ///
    /// `seed` must be a valid, aligned, mutable pointer to a `usize`.
    fn rand_r(seed: *mut usize) -> c_int;
}

const TEST_LOWEST_WEIGHT: SchedWeight =
    SchedulerState::convert_priority_to_weight(LOWEST_PRIORITY + 1);
const TEST_HIGHEST_WEIGHT: SchedWeight =
    SchedulerState::convert_priority_to_weight(HIGHEST_PRIORITY);
const TEST_DEFAULT_WEIGHT: SchedWeight =
    SchedulerState::convert_priority_to_weight(DEFAULT_PRIORITY);
const TEST_EPSILON_WEIGHT: SchedWeight = SchedWeight::from_raw(1);

#[derive(Clone, Copy, PartialEq, Eq)]
enum InheritableProfile {
    No,
    Yes,
}

/// An RAII style helper which automatically assigns a deadline profile (with a
/// short deadline and high utilization) to a thread, restoring the base profile
/// automatically when the test ends.  Many of these tests need to rely on timing
/// in order to control the order with which threads time out of various wait
/// queues.  Since we don't have deterministic control over timing in our tests,
/// we rely on our high priority test thread being scheduled and pre-empting all
/// other threads when its timer goes off in order to reduce the chances of
/// timing related flake in the tests.
struct AutoProfileBooster {
    initial_base_profile: BaseProfile,
}

impl AutoProfileBooster {
    fn new() -> Self {
        // SAFETY: `current_get()` returns a valid pointer to the currently executing thread.
        let current = unsafe { &*current_get() };
        let initial_base_profile = current.snapshot_base_profile();
        let utilization: SchedUtilization = ffl::from_ratio(90i64, 100i64).into();
        let deadline = SchedDuration::from_raw(zx_usec(200));
        let capacity = SchedDuration::from(utilization * deadline);
        let new_base_profile =
            BaseProfile::new_deadline(SchedDeadlineParams::new(capacity, deadline), false);
        current.set_base_profile(&new_base_profile);
        Self { initial_base_profile }
    }
}

impl Drop for AutoProfileBooster {
    fn drop(&mut self) {
        // SAFETY: `current_get()` returns a valid pointer to the currently executing thread.
        let current = unsafe { &*current_get() };
        current.set_base_profile(&self.initial_base_profile);
    }
}

/// A small helper which creates different permutations of an input array based
/// on a distribution method, and optional random seed. Used for things like
/// determining which profiles will be assigned to which test threads, or which
/// order threads will be released from the blocked state during various tests.
#[derive(Clone, Copy)]
struct DistroSpec {
    kind: DistroSpecType,
    seed: u64,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum DistroSpecType {
    Ascending,
    Descending,
    Random,
    Shuffle,
}

impl DistroSpec {
    const fn new(kind: DistroSpecType) -> Self {
        Self { kind, seed: 0 }
    }

    const fn with_seed(kind: DistroSpecType, seed: u64) -> Self {
        Self { kind, seed }
    }

    fn apply<T: Clone, const N: usize>(&self, input: &[T; N], out: &mut [T; N]) {
        let mut prng = self.seed as usize;
        match self.kind {
            DistroSpecType::Ascending => {
                out.clone_from_slice(input);
            }
            DistroSpecType::Descending => {
                for i in 0..N {
                    out[i] = input[input.len() - i - 1].clone();
                }
            }
            DistroSpecType::Random => {
                for item in out.iter_mut() {
                    // SAFETY: `prng` is a valid local seed variable.
                    let idx = (unsafe { rand_r(&raw mut prng) } as usize) % N;
                    *item = input[idx].clone();
                }
            }
            // Create a range of values from [0, N) + offset, but shuffle the order of
            // those values in the set.
            DistroSpecType::Shuffle => {
                // Start by filling our shuffle order array with an illegal sentinel
                // value (N will do the job just fine), then foreach i in the range [0,
                // N) pick a random position in the output to put i, and linearly probe
                // until we find the first unused position in order to shuffle.
                let mut order = [N; N];

                for (i, _) in input.iter().enumerate() {
                    // SAFETY: `prng` is a valid local seed variable.
                    let mut pos = (unsafe { rand_r(&raw mut prng) } as usize) % N;
                    while order[pos] != N {
                        pos = (pos + 1) % N;
                    }
                    order[pos] = i;
                }

                // Finally, produce our output from our input, permuting using the
                // shuffle order.
                for i in 0..N {
                    out[i] = input[order[i]].clone();
                }
            }
        }
    }
}

struct ExpectedBaseProfile {
    discipline: SchedDiscipline,
    fair_weight: SchedWeight,
    deadline: SchedDeadlineParams,
}

impl Default for ExpectedBaseProfile {
    fn default() -> Self {
        Self {
            discipline: SchedDiscipline::Fair,
            fair_weight: SchedWeight::from_raw(0),
            deadline: SchedDeadlineParams::default(),
        }
    }
}

#[derive(Default)]
struct ExpectedEffectiveProfile {
    base: ExpectedBaseProfile,
    ipvs: InheritedProfileValues,
}

#[derive(Default)]
struct ThreadEffectiveProfileObserver {
    observed_profile: EffectiveProfile,
}

impl ThreadEffectiveProfileObserver {
    fn observe(&mut self, t: &Thread) {
        self.observed_profile = t.snapshot_effective_profile();
    }

    fn verify_expected_effective_profile(&self, eep: &ExpectedEffectiveProfile) -> bool {
        (subtest!(|eep: &ExpectedEffectiveProfile| {
            let expected_fair = (eep.base.discipline == SchedDiscipline::Fair)
                && (eep.ipvs.uncapped_utilization == SchedUtilization::from_raw(0));
            assert_eq!(expected_fair, self.observed_profile.is_fair());

            if self.observed_profile.is_fair() {
                let expected = SchedWeight::from(eep.base.fair_weight + eep.ipvs.total_weight);
                expect_eq!(expected.raw_value(), self.observed_profile.weight.raw_value());
            } else {
                let mut effective_utilization = eep.ipvs.uncapped_utilization;
                let mut effective_deadline = eep.ipvs.min_deadline;
                let effective_critical = eep.ipvs.critical_count > 0;

                if eep.base.discipline == SchedDiscipline::Deadline {
                    effective_utilization += eep.base.deadline.utilization;
                    effective_deadline =
                        core::cmp::min(effective_deadline, eep.base.deadline.deadline_ns);
                }
                effective_utilization = core::cmp::min(
                    effective_utilization,
                    SchedUtilization::from(ffl::Fixed::from_integer(1)),
                );

                let effective_capacity =
                    SchedDuration::from(effective_deadline * effective_utilization);
                assert_le!(effective_capacity.raw_value(), effective_deadline.raw_value());

                // Rounding can cause differences between expressions that derive capacity
                // from utilization and deadline and expressions that derive utilization
                // from capacity and deadline. For this reason, use the constructor that
                // takes all three values to ensure consistent test comparisons. The two
                // parameter constructor that derives utilization from the given capacity
                // and deadline may produce a slightly different utilization value than the
                // effective utilization value here.
                let expected = SchedDeadlineParams::with_utilization(
                    effective_capacity,
                    effective_deadline,
                    effective_utilization,
                );

                expect_eq!(
                    expected.capacity_ns.raw_value(),
                    self.observed_profile.deadline().capacity_ns.raw_value()
                );
                expect_eq!(
                    expected.deadline_ns.raw_value(),
                    self.observed_profile.deadline().deadline_ns.raw_value()
                );
                expect_eq!(
                    expected.utilization.raw_value(),
                    self.observed_profile.deadline().utilization.raw_value()
                );
                expect_eq!(effective_critical, self.observed_profile.critical);
            }
        }))(eep)
    }
}

struct FairProfile {
    weight: SchedWeight,
    inheritable: InheritableProfile,
}

impl FairProfile {
    fn create(weight: SchedWeight, inheritable: InheritableProfile) -> Option<RefPtr<Profile>> {
        assert!((weight.raw_value() as u64) != 0xFFFF_FFFF_FFFF_0000);
        pin_make_ref_counted!(Profile { kind: ProfileKind::Fair(Self { weight, inheritable }) })
            .ok()
    }

    fn apply(&self, thread: &Thread) {
        thread.set_base_profile(&BaseProfile::new_fair_with_weight(
            self.weight,
            self.inheritable == InheritableProfile::Yes,
        ));
    }

    fn set_expected_base_profile(&self, eep: &mut ExpectedEffectiveProfile) {
        eep.base.discipline = SchedDiscipline::Fair;
        eep.base.fair_weight = self.weight;
        eep.ipvs = InheritedProfileValues::default();
    }

    fn accumulate_expected_pressure(&self, eep: &mut ExpectedEffectiveProfile) {
        if self.inheritable == InheritableProfile::Yes {
            eep.ipvs.total_weight += self.weight;
        }
    }

    fn debug_print(&self, out: &mut dyn Write) {
        let _ = write!(out, "[weight {}]", self.weight.raw_value());
    }

    fn period(&self) -> zx_duration_mono_t {
        0
    }
}

struct DeadlineProfile {
    sched_params: SchedDeadlineParams,
    critical: bool,
}

impl DeadlineProfile {
    fn create(
        capacity: zx_duration_mono_t,
        deadline: zx_duration_mono_t,
    ) -> Option<RefPtr<Profile>> {
        Self::create_with_critical(capacity, deadline, false)
    }

    fn create_with_critical(
        capacity: zx_duration_mono_t,
        deadline: zx_duration_mono_t,
        critical: bool,
    ) -> Option<RefPtr<Profile>> {
        let capacity = SchedDuration::from_raw(capacity);
        let deadline = SchedDuration::from_raw(deadline);
        debug_assert!(capacity <= deadline);
        pin_make_ref_counted!(Profile {
            kind: ProfileKind::Deadline(Self {
                sched_params: SchedDeadlineParams::new(capacity, deadline),
                critical,
            }),
        })
        .ok()
    }

    fn apply(&self, thread: &Thread) {
        thread.set_base_profile(&BaseProfile::new_deadline(self.sched_params, self.critical));
    }

    fn set_expected_base_profile(&self, eep: &mut ExpectedEffectiveProfile) {
        eep.base.discipline = SchedDiscipline::Deadline;
        eep.base.deadline = self.sched_params;
        eep.ipvs = InheritedProfileValues::default();
    }

    fn accumulate_expected_pressure(&self, eep: &mut ExpectedEffectiveProfile) {
        eep.ipvs.uncapped_utilization += self.sched_params.utilization;
        eep.ipvs.min_deadline =
            core::cmp::min(eep.ipvs.min_deadline, self.sched_params.deadline_ns);
        if self.critical {
            eep.ipvs.critical_count += 1;
        }
    }

    fn debug_print(&self, out: &mut dyn Write) {
        let _ = write!(
            out,
            "[capacity {} deadline {} utilization {} critical {}]",
            self.sched_params.capacity_ns.raw_value(),
            self.sched_params.deadline_ns.raw_value(),
            self.sched_params.utilization.raw_value(),
            i32::from(self.critical),
        );
    }

    fn period(&self) -> zx_duration_mono_t {
        self.sched_params.deadline_ns.raw_value()
    }

    fn sched_params(&self) -> &SchedDeadlineParams {
        &self.sched_params
    }
}

enum ProfileKind {
    Fair(FairProfile),
    Deadline(DeadlineProfile),
}

#[ref_counted]
#[pin_data]
#[derive(Recyclable)]
#[repr(C)]
struct Profile {
    kind: ProfileKind,
}

impl Profile {
    fn apply(&self, thread: &Thread) {
        match &self.kind {
            ProfileKind::Fair(fair) => fair.apply(thread),
            ProfileKind::Deadline(deadline) => deadline.apply(thread),
        }
    }

    fn set_expected_base_profile(&self, eep: &mut ExpectedEffectiveProfile) {
        match &self.kind {
            ProfileKind::Fair(fair) => fair.set_expected_base_profile(eep),
            ProfileKind::Deadline(deadline) => deadline.set_expected_base_profile(eep),
        }
    }

    fn accumulate_expected_pressure(&self, eep: &mut ExpectedEffectiveProfile) {
        match &self.kind {
            ProfileKind::Fair(fair) => fair.accumulate_expected_pressure(eep),
            ProfileKind::Deadline(deadline) => deadline.accumulate_expected_pressure(eep),
        }
    }

    fn debug_print(&self, out: &mut dyn Write) {
        match &self.kind {
            ProfileKind::Fair(fair) => fair.debug_print(out),
            ProfileKind::Deadline(deadline) => deadline.debug_print(out),
        }
    }

    fn period(&self) -> zx_duration_mono_t {
        match &self.kind {
            ProfileKind::Fair(fair) => fair.period(),
            ProfileKind::Deadline(deadline) => deadline.period(),
        }
    }

    fn as_deadline(&self) -> Option<&DeadlineProfile> {
        match &self.kind {
            ProfileKind::Fair(_) => None,
            ProfileKind::Deadline(deadline) => Some(deadline),
        }
    }
}

/// Helper wrapper for an owned wait queue which manages grabbing and releasing
/// the locks or disabling preemption at appropriate times for us.  Mostly, this
/// is just about saving some typing.
#[pin_data]
#[derive(Recyclable)]
struct LockedOwnedWaitQueue {
    #[pin]
    base: OwnedWaitQueue,
}

impl Deref for LockedOwnedWaitQueue {
    type Target = OwnedWaitQueue;

    fn deref(&self) -> &Self::Target {
        &self.base
    }
}

// SAFETY: Forwards `ChainLocked` to the embedded `OwnedWaitQueue`.
unsafe impl ChainLocked for LockedOwnedWaitQueue {
    type Class = WaitQueueLockClass;

    fn lock(&self) -> &ClassedChainLock<Self::Class> {
        self.base.lock()
    }
}

impl LockedOwnedWaitQueue {
    fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self { base <- OwnedWaitQueue::init() })
    }

    fn try_new() -> Result<UniquePtr<Self>, kalloc::AllocError> {
        let mut uninit = <Self as UninitRecyclable>::allocate_uninit()?;
        // SAFETY: `uninit.as_mut()` points to valid, aligned, uninitialized heap storage that
        // remains pinned inside `UniquePtr` until `Recyclable::recycle` drops it in place.
        unsafe {
            let _ = uninit.as_mut().write_pin_init(Self::init());
            Ok(UniquePtr::from_raw(uninit.as_ptr().cast::<Self>()))
        }
    }

    fn release_all_threads(&self) {
        let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        self.base.wake_threads(u32::MAX, &mut OwnedWaitQueue::default_wake_hooks());
    }

    fn release_one_thread(&self) {
        let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
        let _ = self.base.wake_thread_and_assign_owner(&mut OwnedWaitQueue::default_wake_hooks());
    }

    fn has_inherited_scheduler_state_storage(&self) -> bool {
        single_chain_lock_guard_owned!(
            guard,
            &self.base,
            IRQ_SAVE_OPTION,
            clt_tag!("LockedOwnedWaitQueue::HasInheritedSchedulerStateStorage (pi_tests)")
        );
        guard.proof().inherited_scheduler_state_storage().is_some()
    }
}

struct FixedStringBuffer<const N: usize> {
    bytes: [u8; N],
    len: usize,
}

impl<const N: usize> FixedStringBuffer<N> {
    const fn new() -> Self {
        Self { bytes: [0; N], len: 0 }
    }

    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.bytes[..self.len]).unwrap_or("")
    }
}

impl<const N: usize> Write for FixedStringBuffer<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let avail = N.saturating_sub(self.len);
        let to_copy = core::cmp::min(avail, s.len());
        self.bytes[self.len..self.len + to_copy].copy_from_slice(&s.as_bytes()[..to_copy]);
        self.len += to_copy;
        Ok(())
    }
}

trait LoopIterValue {
    fn format_iter_val(&self, out: &mut dyn Write);
}

impl LoopIterValue for RefPtr<Profile> {
    fn format_iter_val(&self, out: &mut dyn Write) {
        self.debug_print(out);
    }
}

impl LoopIterValue for &RefPtr<Profile> {
    fn format_iter_val(&self, out: &mut dyn Write) {
        self.debug_print(out);
    }
}

impl LoopIterValue for u32 {
    fn format_iter_val(&self, out: &mut dyn Write) {
        let _ = write!(out, "{}", i64::from(*self));
    }
}

impl LoopIterValue for usize {
    fn format_iter_val(&self, out: &mut dyn Write) {
        let _ = write!(out, "{}", *self as i64);
    }
}

/// LoopIterPrinter
/// A small RAII style class which helps us to print out where a loop iterator
/// is when a test fails and bails out.  Note: loop iterator types must be
/// convertible to int64_t (or `RefPtr<Profile>`).
struct LoopIterPrinter<T: LoopIterValue> {
    field_name: Option<&'static str>,
    iter_val: T,
}

impl<T: LoopIterValue> LoopIterPrinter<T> {
    const fn new(field_name: &'static str, iter_val: T) -> Self {
        Self { field_name: Some(field_name), iter_val }
    }

    fn cancel(&mut self) {
        self.field_name = None;
    }
}

impl<T: LoopIterValue> Drop for LoopIterPrinter<T> {
    fn drop(&mut self) {
        let Some(field_name) = self.field_name else {
            return;
        };

        let mut buffer = FixedStringBuffer::<256>::new();
        let _ = write!(&mut buffer, "Test failed with {} == ", field_name);
        self.iter_val.format_iter_val(&mut buffer);
        kprintln!("{:s}", buffer.as_str());
    }
}

macro_rules! print_loop_iter {
    ($var_name:ident, $printer_name:ident) => {
        let mut $printer_name = LoopIterPrinter::new(stringify!($var_name), $var_name.clone());
    };
}

/// Test threads in the various tests use lambdas in order to store their
/// customized test operations.  In order to allow these lambdas to capture
/// context from their local scope, but not need to use the heap in order to
/// allocate the storage for the scope, we need to know the worst case
/// capture storage requirements across all of these tests.  Armed with this
/// knowledge, we can use an inline function buffer to pre-allocate storage in
/// the `TestThread` object for the worst case lambda we will encounter in the
/// test suite.
///
/// If this grows in the future, this const bound should be updated to
/// match the new worst case storage requirement.
const MAX_OP_LAMBDA_CAPTURE_STORAGE_BYTES: usize = size_of::<*const ()>() * 8;
const MAX_OP_LAMBDA_WORDS: usize = MAX_OP_LAMBDA_CAPTURE_STORAGE_BYTES / size_of::<usize>();

struct InlineOp {
    storage: [MaybeUninit<usize>; MAX_OP_LAMBDA_WORDS],
    call_fn: Option<unsafe fn(*mut u8)>,
    drop_fn: Option<unsafe fn(*mut u8)>,
}

impl Default for InlineOp {
    fn default() -> Self {
        Self { storage: [MaybeUninit::uninit(); MAX_OP_LAMBDA_WORDS], call_fn: None, drop_fn: None }
    }
}

impl InlineOp {
    fn is_some(&self) -> bool {
        self.call_fn.is_some()
    }

    /// Stores `op` inline without heap allocation.
    ///
    /// # Safety
    ///
    /// Any references captured by `op` must remain valid until `self.clear()` is called or `self`
    /// is dropped.
    unsafe fn set<'a, F>(&mut self, op: F)
    where
        F: FnMut() + 'a,
    {
        self.clear();
        assert!(size_of::<F>() <= MAX_OP_LAMBDA_CAPTURE_STORAGE_BYTES);
        assert!(align_of::<F>() <= align_of::<usize>());

        /// Invokes the closure of type `F` stored at `ptr`.
        ///
        /// # Safety
        ///
        /// `ptr` must point to a valid, initialized value of type `F` with exclusive access.
        unsafe fn call_impl<F: FnMut()>(ptr: *mut u8) {
            // SAFETY: `ptr` points to an initialized `F` stored in `InlineOp::storage`.
            unsafe { (*ptr.cast::<F>())() };
        }

        /// Drops the closure of type `F` stored at `ptr` in place.
        ///
        /// # Safety
        ///
        /// `ptr` must point to a valid, initialized value of type `F` that will not be used again.
        unsafe fn drop_impl<F>(ptr: *mut u8) {
            // SAFETY: `ptr` points to an initialized `F` stored in `InlineOp::storage`.
            unsafe { ptr.cast::<F>().drop_in_place() };
        }

        // SAFETY: `self.storage` is suitably sized and aligned for `F` (checked above).
        unsafe {
            self.storage.as_mut_ptr().cast::<F>().write(op);
        }
        self.call_fn = Some(call_impl::<F>);
        self.drop_fn = Some(drop_impl::<F>);
    }

    fn call(&mut self) {
        let call_fn = self.call_fn.expect("InlineOp::call invoked on empty InlineOp");
        // SAFETY: `self.storage` holds the initialized closure matching `call_fn`.
        unsafe { call_fn(self.storage.as_mut_ptr().cast::<u8>()) };
    }

    fn clear(&mut self) {
        self.call_fn = None;
        if let Some(drop_fn) = self.drop_fn.take() {
            // SAFETY: `self.storage` holds the initialized closure matching `drop_fn`, and taking
            // `drop_fn` ensures it is dropped at most once.
            unsafe { drop_fn(self.storage.as_mut_ptr().cast::<u8>()) };
        }
    }
}

impl Drop for InlineOp {
    fn drop(&mut self) {
        self.clear();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
enum TestThreadState {
    Initial = 0,
    Created = 1,
    WaitingToStart = 2,
    Started = 3,
    WaitingForShutdown = 4,
    Shutdown = 5,
}

impl TestThreadState {
    fn from_u32(val: u32) -> Self {
        match val {
            0 => Self::Initial,
            1 => Self::Created,
            2 => Self::WaitingToStart,
            3 => Self::Started,
            4 => Self::WaitingForShutdown,
            5 => Self::Shutdown,
            _ => unreachable!(),
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TestThreadCondition {
    Blocked,
    Started,
    WaitingForShutdown,
}

struct StateSnapshot {
    current_time: SchedTime,
    remaining_time_slice_ns: SchedDuration,
    start_time: SchedTime,
    finish_time: SchedTime,
    time_slice_ns: SchedDuration,
    activation_count: u64,
}

/// The core test thread object.  We use this object to build various graphs of
/// priority inheritance chains, and then evaluate that the effective priorities
/// of the threads involved in the graph are what we expect them to be after
/// various mutations of the graph have taken place.
struct TestThread {
    /// Safety invariant: `thread` and `initial_profile` are only mutated by the test runner thread
    /// while `state` is `Initial` (in `create`) or after the worker thread has been forgotten or
    /// joined (in `reset`). `op` is written by the test runner thread before transitioning `state`
    /// to `WaitingToStart` and resuming `thread`, then invoked and cleared by the worker thread in
    /// `thread_entry` (or cleared by the test runner thread in `reset` after joining `thread`).
    thread: UnsafeCell<Option<ThreadPtr>>,
    state: AtomicU32,
    op: UnsafeCell<InlineOp>,
    initial_profile: UnsafeCell<Option<RefPtr<Profile>>>,
}

impl Default for TestThread {
    fn default() -> Self {
        Self {
            thread: UnsafeCell::new(None),
            state: AtomicU32::new(TestThreadState::Initial as u32),
            op: UnsafeCell::new(InlineOp::default()),
            initial_profile: UnsafeCell::new(None),
        }
    }
}

impl Drop for TestThread {
    fn drop(&mut self) {
        let _ = self.reset(false);
    }
}

impl TestThread {
    /// Reset the barrier at the start of a test in order to prevent threads from
    /// exiting after they have completed their operation.
    fn reset_shutdown_barrier() {
        let _ = Self::allow_shutdown().unsignal();
    }

    /// Clear the barrier and allow shutdown.
    fn clear_shutdown_barrier() {
        Self::allow_shutdown().signal();
    }

    fn allow_shutdown() -> &'static Event {
        static ALLOW_SHUTDOWN: LazyInit<Event> = LazyInit::uninit();
        static INITIALIZED: AtomicBool = AtomicBool::new(false);
        if !INITIALIZED.swap(true, Ordering::SeqCst) {
            // SAFETY: `reset_shutdown_barrier` initializes `ALLOW_SHUTDOWN` on the test runner
            // thread before any worker threads are spawned, and `INITIALIZED.swap` guarantees
            // `init_pin` runs only once.
            unsafe {
                let _ = Pin::static_ref(&ALLOW_SHUTDOWN).init_pin(Event::init_unsignaled());
            }
        }
        &ALLOW_SHUTDOWN
    }

    fn state(&self) -> TestThreadState {
        TestThreadState::from_u32(self.state.load(Ordering::SeqCst))
    }

    fn set_state(&self, new_state: TestThreadState) {
        self.state.store(new_state as u32, Ordering::SeqCst);
    }

    fn initial_profile(&self) -> &RefPtr<Profile> {
        // SAFETY: `initial_profile` is only mutated in `create` and `reset` on the test runner
        // thread.
        unsafe { (*self.initial_profile.get()).as_ref().expect("initial_profile must be set") }
    }

    fn thread_ptr(&self) -> Option<ThreadPtr> {
        // SAFETY: `thread` is set in `create` before the worker thread starts and only cleared in
        // `reset` after the worker thread has been forgotten or joined.
        unsafe { *self.thread.get() }
    }

    fn thread(&self) -> &Thread {
        let ptr = self.thread_ptr();
        debug_assert!(ptr.is_some());
        // SAFETY: `ptr` points to a live `Thread` owned by `self` until `reset` joins or forgets
        // it.
        unsafe { ptr.unwrap().as_ref() }
    }

    fn tstate(&self) -> ThreadStateKind {
        let Some(ptr) = self.thread_ptr() else {
            return ThreadStateKind::Death;
        };
        // SAFETY: `ptr` points to a live `Thread` owned by `self`.
        let thread = unsafe { ptr.as_ref() };
        single_chain_lock_guard_owned!(
            guard,
            thread,
            IRQ_SAVE_OPTION,
            clt_tag!("TestThread::tstate (pi_tests)")
        );
        guard.proof().state()
    }

    fn remaining_time_slice(&self) -> SchedDuration {
        let thread = self.thread();
        single_chain_lock_guard_owned!(
            guard,
            thread,
            IRQ_SAVE_OPTION,
            clt_tag!("TestThread::remaining_time_slice (pi_tests)")
        );
        let proof = guard.proof();
        let time_elapsed = if proof.state() == ThreadStateKind::Running {
            SchedDuration::from(
                SchedTime::from_raw(current_mono_time().0) - proof.last_started_running(),
            )
        } else {
            SchedDuration::from_raw(0)
        };
        SchedDuration::from(proof.remaining_time_slice_ns() - time_elapsed)
    }

    fn activation_count(&self) -> u64 {
        let thread = self.thread();
        single_chain_lock_guard_owned!(
            guard,
            thread,
            IRQ_SAVE_OPTION,
            clt_tag!("TestThread::activation_count (pi_tests)")
        );
        guard.proof().activation_count()
    }

    fn snapshot(&self) -> StateSnapshot {
        let thread = self.thread();
        single_chain_lock_guard_owned!(
            guard,
            thread,
            IRQ_SAVE_OPTION,
            clt_tag!("TestThread::snapshot (pi_tests)")
        );
        let proof = guard.proof();
        let current_time = SchedTime::from_raw(current_mono_time().0);
        let running = proof.state() == ThreadStateKind::Running;
        let time_elapsed = if running {
            SchedDuration::from(current_time - proof.last_started_running())
        } else {
            SchedDuration::from_raw(0)
        };
        let (start_time, finish_time, time_slice_ns, _) = proof.dynamic_parameters();
        StateSnapshot {
            current_time,
            remaining_time_slice_ns: SchedDuration::from(
                proof.remaining_time_slice_ns() - time_elapsed,
            ),
            start_time,
            finish_time,
            time_slice_ns,
            activation_count: proof.activation_count(),
        }
    }

    extern "C" fn thread_entry_trampoline(ctx: *mut c_void) -> i32 {
        // SAFETY: `ctx` is `self as *const TestThread as *mut c_void` passed to
        // `create_with_priority` in `TestThread::create`, and `self` outlives the thread (which is
        // joined in `reset`).
        let this = unsafe { &*(ctx as *const Self) };
        this.thread_entry()
    }

    /// Create a thread, setting its entry point and initial profile in
    /// the process, but do not start it yet.
    fn create(&self, initial_profile: RefPtr<Profile>) -> bool {
        (subtest!(|initial_profile: RefPtr<Profile>| {
            assert_true!(self.thread_ptr().is_none());
            // SAFETY: `self.state()` is checked to be `Initial`, so no worker thread is running.
            assert_true!(unsafe { (*self.initial_profile.get()).is_none() });
            assert_eq!(self.state(), TestThreadState::Initial);

            // SAFETY: No worker thread exists yet; exclusive access on the test runner thread.
            unsafe {
                *self.initial_profile.get() = Some(initial_profile);
            }
            // SAFETY: `thread_entry_trampoline` is a valid entry point and `self` remains valid
            // until the thread is joined or forgotten in `reset`.
            let created = unsafe {
                create_with_priority(
                    c"pi_test_thread".as_ptr(),
                    Self::thread_entry_trampoline,
                    core::ptr::from_ref(self).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            };
            assert_true!(created.is_ok());
            let thread_ptr = created.unwrap();
            // SAFETY: No worker thread has been resumed yet.
            unsafe {
                *self.thread.get() = Some(thread_ptr);
            }
            self.initial_profile().apply(self.thread());

            self.set_state(TestThreadState::Created);
        }))(initial_profile)
    }

    /// Start the thread, have it do nothing but wait to be allowed to exit.
    fn do_stall(&self) -> bool {
        (subtest!(|this: &Self| {
            assert_eq!(this.state(), TestThreadState::Created);
            // SAFETY: The worker thread has not been resumed yet (`state == Created`).
            assert_false!(unsafe { (*this.op.get()).is_some() });

            // SAFETY: The worker thread has not been resumed yet, and the closure captures
            // nothing.
            unsafe {
                (*this.op.get()).set(|| {});
            }

            this.set_state(TestThreadState::WaitingToStart);
            // SAFETY: `this.thread_ptr()` was created in `create` and has not been resumed yet.
            unsafe { this.thread_ptr().unwrap().resume() };

            assert_true!(this.wait_for(TestThreadCondition::Blocked));
        }))(self)
    }

    /// Start the thread and have it block on a standard wait queue.
    fn block_on_wait_queue(&self, wq: &WaitQueue, relative_timeout: DurationMono) -> bool {
        (subtest!(|this: &Self, wq: &WaitQueue, relative_timeout: DurationMono| {
            assert_eq!(this.state(), TestThreadState::Created);
            // SAFETY: The worker thread has not been resumed yet (`state == Created`).
            assert_false!(unsafe { (*this.op.get()).is_some() });

            let op = move || {
                let timeout = if relative_timeout == DurationMono::INFINITE {
                    Deadline::infinite()
                } else {
                    Deadline::after_mono(
                        relative_timeout,
                        crate::kernel::deadline::TimerSlack::none(),
                    )
                };

                ChainLockTransaction::until_done(
                    PREEMPT_DISABLE_AND_IRQ_SAVE_OPTION,
                    clt_tag!("TestThread::BlockOnWaitQueue (pi_tests)"),
                    |token| {
                        // SAFETY: `current_get()` returns a valid pointer to the currently
                        // executing thread, and `as_chain_lock()` is used here solely to acquire
                        // both locks in `acquire_both_or_backoff` before adopting them into
                        // `OwnerGuard`s below.
                        let (current_thread, acquired) = unsafe {
                            let current_thread = &*current_get();
                            let acquired = acquire_both_or_backoff(
                                current_thread.lock().as_chain_lock(),
                                wq.lock().as_chain_lock(),
                                token,
                            );
                            (current_thread, acquired)
                        };
                        if !acquired {
                            return TransactionResult::BACKOFF;
                        }
                        // SAFETY: Both `current_thread` and `wq` locks were acquired above via
                        // `acquire_both_or_backoff`.
                        let (mut current_guard, wq_guard) = unsafe {
                            (
                                OwnerGuard::adopt_unchecked(current_thread, token),
                                OwnerGuard::adopt_unchecked(wq, token),
                            )
                        };

                        ChainLockTransaction::finalize(token);
                        // SAFETY: `current_guard` holds the current running thread's lock,
                        // `wq_guard` holds `wq`'s lock, and the transaction is finalized.
                        let _ = unsafe {
                            WaitQueue::block(
                                wq_guard,
                                &mut current_guard.proof(),
                                &timeout,
                                Interruptible::Yes,
                                token,
                            )
                        };
                        current_guard.release();
                        TransactionResult::DONE
                    },
                );
            };
            // SAFETY: The worker thread has not been resumed yet, and `wq` outlives the worker
            // thread's execution of `op`.
            unsafe {
                (*this.op.get()).set(op);
            }

            this.set_state(TestThreadState::WaitingToStart);
            // SAFETY: `this.thread_ptr()` was created in `create` and has not been resumed yet.
            unsafe { this.thread_ptr().unwrap().resume() };

            assert_true!(this.wait_for(TestThreadCondition::Blocked));
        }))(self, wq, relative_timeout)
    }

    /// Start the thread and have it block on an owned wait queue, declaring the
    /// specified test thread to be the owner of that queue in the process.
    fn block_on_owned_wait_queue(
        &self,
        owned_wq: &OwnedWaitQueue,
        owner: Option<&TestThread>,
        relative_timeout: DurationMono,
    ) -> bool {
        (subtest!(|this: &Self,
                   owned_wq: &OwnedWaitQueue,
                   owner: Option<&TestThread>,
                   relative_timeout: DurationMono| {
            assert_eq!(this.state(), TestThreadState::Created);
            // SAFETY: The worker thread has not been resumed yet (`state == Created`).
            assert_false!(unsafe { (*this.op.get()).is_some() });

            let owner_thrd = owner.map(TestThread::thread);
            let op = move || {
                let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
                // SAFETY: Preemption is disabled by `_eager_resched_disabler`.
                let preempt_disabled = unsafe { PreemptDisabledToken::new() };

                let timeout = if relative_timeout == DurationMono::INFINITE {
                    Deadline::infinite()
                } else {
                    Deadline::after_mono(
                        relative_timeout,
                        crate::kernel::deadline::TimerSlack::none(),
                    )
                };

                // SAFETY: `owned_wq` and `owner_thrd` remain valid for the duration of the test,
                // and preemption is disabled via `preempt_disabled`.
                let _ = unsafe {
                    owned_wq.block_and_assign_owner(
                        &timeout,
                        owner_thrd,
                        ResourceOwnership::Normal,
                        Interruptible::Yes,
                        &preempt_disabled,
                    )
                };
            };

            // SAFETY: The worker thread has not been resumed yet, and `owned_wq` and `owner`
            // remain valid while the worker thread executes `op`.
            unsafe {
                (*this.op.get()).set(op);
            }

            this.set_state(TestThreadState::WaitingToStart);
            // SAFETY: `this.thread_ptr()` was created in `create` and has not been resumed yet.
            unsafe { this.thread_ptr().unwrap().resume() };

            assert_true!(this.wait_for(TestThreadCondition::Blocked));
        }))(self, owned_wq, owner, relative_timeout)
    }

    /// Start the thread and run a custom lambda.
    ///
    /// # Safety
    ///
    /// Any references captured by `op` must remain valid until the thread finishes running `op`
    /// and is reset/joined.
    unsafe fn run_op<'a, F>(&self, op: F) -> bool
    where
        F: FnMut() + 'a,
    {
        let mut op_opt = Some(op);
        (subtest!(|this: &Self| {
            assert_eq!(this.state(), TestThreadState::Created);
            // SAFETY: The worker thread has not been resumed yet (`state == Created`).
            assert_false!(unsafe { (*this.op.get()).is_some() });

            // SAFETY: The worker thread has not been resumed yet, and the caller guarantees that
            // any references captured by `op` outlive the worker thread's execution of `op`.
            unsafe {
                (*this.op.get()).set(op_opt.take().unwrap());
            }

            this.set_state(TestThreadState::WaitingToStart);
            // SAFETY: `this.thread_ptr()` was created in `create` and has not been resumed yet.
            unsafe { this.thread_ptr().unwrap().resume() };

            assert_true!(this.wait_for(TestThreadCondition::Started));
        }))(self)
    }

    /// Reset the thread back to its initial state.  If `explicit_kill` is true,
    /// then do not wait for the thread to exit normally if it has been started.
    /// Simply send it the kill signal.
    fn reset(&self, explicit_kill: bool) -> bool {
        (subtest!(|explicit_kill: bool| {
            // If we are explicitly killing the thread as part of the test, then we
            // should not expect the shutdown barrier to be cleared.
            if !explicit_kill {
                expect_true!(Self::allow_shutdown().is_signaled());
            }

            match self.state() {
                TestThreadState::Initial => {}
                TestThreadState::Created => {
                    // Created but not started?  `forget` seems to be the proper way to
                    // cleanup a thread which was never started.
                    let thread_ptr = self.thread_ptr();
                    assert!(thread_ptr.is_some());
                    // SAFETY: The thread was created in `create` and never resumed, so `forget`
                    // consumes it without starting or joining it.
                    unsafe {
                        thread_ptr.unwrap().forget();
                        *self.thread.get() = None;
                    }
                }
                TestThreadState::WaitingToStart
                | TestThreadState::Started
                | TestThreadState::WaitingForShutdown
                | TestThreadState::Shutdown => {
                    let thread_ptr = self.thread_ptr();
                    assert!(thread_ptr.is_some());
                    let thread_ptr = thread_ptr.unwrap();

                    // If we are explicitly killing the thread, send it the kill signal now.
                    if explicit_kill {
                        // SAFETY: `thread_ptr` is a valid, unjoined thread.
                        unsafe { thread_ptr.kill() };
                    }

                    // The thread should be on its way to termination as we speak.  Attempt to
                    // join it with a relatively short timeout.  If this fails, print a
                    // warning and try again with an infinite timeout.  Why try with a short
                    // timeout and then an infinite timeout?  We might be running in an
                    // emulated or virtualized environment and things may take a lot longer
                    // than they otherwise would.  By timing out quickly and printing a
                    // warning, we can hopefully make it easier for a developer to figure out
                    // what's going on in the case where the second join hangs forever.
                    let timeout = DurationMono(zx_msec(500));
                    let join_deadline =
                        Deadline::after_mono(timeout, crate::kernel::deadline::TimerSlack::none());
                    // SAFETY: `thread_ptr` is joined here. If the first `join` times out, the
                    // thread remains valid for the retry with `InstantMono::INFINITE`.
                    let mut res = unsafe { thread_ptr.join(InstantMono(join_deadline.when().0)) };
                    if res == Err(Status::TIMED_OUT) {
                        kprintln!(
                            "Timed out while joining thread {:p}, retrying with infinite timeout",
                            thread_ptr.as_ptr()
                        );
                        // SAFETY: The first `join` timed out, so `thread_ptr` has not been joined
                        // yet.
                        res = unsafe { thread_ptr.join(InstantMono::INFINITE) };
                    }
                    if let Err(err) = res {
                        panic!(
                            "join of thread {:p} failed with {}",
                            thread_ptr.as_ptr(),
                            err.into_raw()
                        );
                    }
                    // SAFETY: The worker thread has exited and been joined.
                    unsafe {
                        *self.thread.get() = None;
                    }
                }
            }

            self.set_state(TestThreadState::Initial);
            // SAFETY: Any worker thread has been forgotten or joined above, so `self.op` and
            // `self.initial_profile` are exclusively accessed on the test runner thread.
            unsafe {
                (*self.op.get()).clear();
                *self.initial_profile.get() = None;
            }
            assert_true!(self.thread_ptr().is_none());
        }))(explicit_kill)
    }

    fn thread_entry(&self) -> i32 {
        // SAFETY: The test runner thread does not touch `self.op` after setting `WaitingToStart`
        // and resuming this worker thread until after this worker thread is joined in `reset`.
        let has_op = unsafe { (*self.op.get()).is_some() };
        if !has_op || (self.state() != TestThreadState::WaitingToStart) {
            return -1;
        }

        self.set_state(TestThreadState::Started);
        // SAFETY: Exclusively accessed by this worker thread while in `Started`.
        unsafe {
            (*self.op.get()).call();
        }
        self.set_state(TestThreadState::WaitingForShutdown);
        // Do not block on the allow shutdown event if we have received the kill
        // signal.  Zircon events are non-interruptible.
        if (self.thread().signals() & THREAD_SIGNAL_KILL) == 0 {
            let _ = Self::allow_shutdown().wait(&Deadline::infinite());
        }

        self.set_state(TestThreadState::Shutdown);
        // SAFETY: Exclusively accessed by this worker thread before it exits (the test runner
        // thread joins this thread before touching `self.op` in `reset`).
        unsafe {
            (*self.op.get()).clear();
        }

        0
    }

    fn wait_for(&self, condition: TestThreadCondition) -> bool {
        (subtest!(|condition: TestThreadCondition| {
            let timeout = zx_sec(10);
            let poll_interval = DurationMono(zx_usec(100));
            let deadline = current_mono_time().0 + timeout;

            loop {
                match condition {
                    TestThreadCondition::Blocked => {
                        let cur_state = self.tstate();

                        if cur_state == ThreadStateKind::Blocked {
                            break;
                        }

                        if cur_state != ThreadStateKind::Running {
                            assert_eq!(ThreadStateKind::Ready, cur_state);
                        }
                    }
                    TestThreadCondition::Started => {
                        if self.state() == TestThreadState::Started {
                            break;
                        }
                    }
                    TestThreadCondition::WaitingForShutdown => {
                        if self.state() == TestThreadState::WaitingForShutdown {
                            break;
                        }
                    }
                }

                let now = current_mono_time().0;
                assert_lt!(now, deadline);
                let _ = sleep_relative(poll_interval);
            }
        }))(condition)
    }
}

fn pi_test_cycle(inheritable_profiles: InheritableProfile) -> bool {
    (subtest!(|inheritable_profiles: InheritableProfile| {
        let _pboost = AutoProfileBooster::new();

        // Deliberately create a cycle and make sure that we don't hang or otherwise
        // exhibit bad behavior.
        #[pin_data]
        struct Link {
            thread: TestThread,
            #[pin]
            link: LockedOwnedWaitQueue,
            profile: RefCell<Option<RefPtr<Profile>>>,
        }

        const CYCLE_LEN: usize = 4;
        stack_pin_init!(
            let nodes = pin_init_array_from_fn::<_, CYCLE_LEN, _, _>(|_| {
                pin_init!(Link {
                    thread: TestThread::default(),
                    link <- LockedOwnedWaitQueue::init(),
                    profile: RefCell::new(None),
                })
            })
        );
        let nodes: &[Link; CYCLE_LEN] = nodes.into_ref().get_ref();

        // At the end of the tests, success or failure, be sure to clean up.
        let _cleanup = zr::defer(|| {
            TestThread::clear_shutdown_barrier();
            for n in nodes {
                n.link.release_all_threads();
            }
            for n in nodes {
                let _ = n.thread.reset(false);
            }
        });

        // Create each of the profiles and assign them to each of the threads.
        for tndx in 0..nodes.len() as u32 {
            print_loop_iter!(tndx, print_tndx);

            let delta_weight = SchedWeight::from(i64::from(tndx) * TEST_EPSILON_WEIGHT);
            let tgt_weight = SchedWeight::from(TEST_LOWEST_WEIGHT + delta_weight);
            assert_le!(tgt_weight.raw_value(), TEST_HIGHEST_WEIGHT.raw_value());
            let profile = FairProfile::create(tgt_weight, inheritable_profiles);
            assert_true!(profile.is_some());
            let profile = profile.unwrap();
            *nodes[tndx as usize].profile.borrow_mut() = Some(profile.clone());
            assert_true!(nodes[tndx as usize].thread.create(profile));

            print_tndx.cancel();
        }

        // Let each thread run, blocking it on its own link and declaring the next
        // thread in the list to be the owner of the link.  When we hit the last
        // thread, we attempt to form a cycle.
        //
        // As of today, the OwnedWaitQueue code will refuse to create the cycle and
        // will "fix" the problem by allowing the thread to block, but declaring the
        // owner of the OWQ the thread is blocking in to be no one.  Our threads are
        // in ascending priority order, so we should not see any changes to the
        // effective priority of any of the threads.
        //
        // Eventually, however, the Block operation will completely fail instead of
        // allowing the cycle to come into existence.  It will not change the owner,
        // nor will it block the thread in question. Instead, it will return an error.
        // This test will need to be updated when that day arrives.
        for tndx in 0..nodes.len() {
            print_loop_iter!(tndx, print_tndx);

            let owner_thread = &nodes[(tndx + 1) % nodes.len()].thread;
            let link = &nodes[tndx].link;
            assert_true!(nodes[tndx].thread.block_on_owned_wait_queue(
                link,
                Some(owner_thread),
                DurationMono::INFINITE,
            ));

            for validation_ndx in 0..=tndx {
                print_loop_iter!(validation_ndx, print_validation_ndx);

                // The profile of each link in the chain should be the combination of its
                // base profile and all of the threads in the chain which are blocked
                // behind it.
                let mut expected_profile = ExpectedEffectiveProfile::default();
                nodes[validation_ndx]
                    .thread
                    .initial_profile()
                    .set_expected_base_profile(&mut expected_profile);
                for i in 1..=validation_ndx {
                    let thread = &nodes[validation_ndx - i].thread;
                    thread.initial_profile().accumulate_expected_pressure(&mut expected_profile);
                }

                let mut observer = ThreadEffectiveProfileObserver::default();
                observer.observe(nodes[validation_ndx].thread.thread());
                assert_true!(observer.verify_expected_effective_profile(&expected_profile));

                // Every OWQ in the test vector should be owned by the thread after it in
                // the sequence, except for the last OWQ.  We tried to assign it to be
                // owned by the first thread when we tried to create the cycle, but the
                // implementation should have refused and left the queue with no owner.
                let observe_owner = |queue: &OwnedWaitQueue| -> *const Thread {
                    single_chain_lock_guard_owned!(
                        guard,
                        queue,
                        IRQ_SAVE_OPTION,
                        clt_tag!("pi_test_cycle")
                    );
                    guard.proof().queue_owner().map_or(core::ptr::null(), |p| p.as_ptr())
                };

                let expected_owner: *const Thread = if tndx < (nodes.len() - 1) {
                    core::ptr::from_ref(nodes[tndx + 1].thread.thread())
                } else {
                    core::ptr::null()
                };
                let observed_owner = observe_owner(&nodes[tndx].link);
                assert_eq!(expected_owner, observed_owner);

                print_validation_ndx.cancel();
            }

            print_tndx.cancel();
        }
    }))(inheritable_profiles)
}

fn pi_test_deadline_join_split_helper<const NUM_PRESSURE_THREADS: usize, F>(
    active_profile: &RefPtr<Profile>,
    pressure_profiles: &[RefPtr<Profile>; NUM_PRESSURE_THREADS],
    spin_duration: zx_duration_mono_t,
    retry_duration: zx_duration_mono_t,
    mut verify_fn: F,
) -> bool
where
    F: FnMut(&TestThread, &[TestThread; NUM_PRESSURE_THREADS]) -> bool,
{
    (subtest!(|| {
        const RETRY_WARNING_COUNT: usize = 10000;
        let mut retry_count: usize = 0;
        let mut retry_test: bool;

        let owq_count = |owq: &LockedOwnedWaitQueue| -> u32 {
            let owq_base: &OwnedWaitQueue = owq;
            single_chain_lock_guard_owned!(
                guard,
                owq_base,
                IRQ_SAVE_OPTION,
                clt_tag!("pi_tests::bandwidth_inheritance_poll")
            );
            guard.proof().count()
        };

        loop {
            retry_test = false;
            let activation_count_changed = AtomicBool::new(false);

            stack_pin_init!(let owq1 = LockedOwnedWaitQueue::init());
            let owq1: &LockedOwnedWaitQueue = owq1.into_ref().get_ref();
            stack_pin_init!(let owq2 = LockedOwnedWaitQueue::init());
            let owq2: &LockedOwnedWaitQueue = owq2.into_ref().get_ref();
            let active_thread = TestThread::default();
            let pressure_threads: [TestThread; NUM_PRESSURE_THREADS] =
                core::array::from_fn(|_| TestThread::default());

            let print_state = |tag: &str| {
                let active_snapshot = active_thread.snapshot();
                kprintln!(
                    "\n\t{:<10s}\t{:<20s}  {:>14s}  {:>14s}  {:>14s}  {:>14s}  {:>6s}  {:>14s}",
                    tag,
                    "THREAD",
                    "REM_TS",
                    "SLICE",
                    "START",
                    "FINISH",
                    "ACT",
                    "CURRENT"
                );
                kprintln!(
                    "\t{:<10s}\t{:<20s}  {:>14}  {:>14}  {:>14}  {:>14}  {:>6}  {:>14}",
                    "",
                    "active_thread",
                    active_snapshot.remaining_time_slice_ns.raw_value(),
                    active_snapshot.time_slice_ns.raw_value(),
                    active_snapshot.start_time.raw_value(),
                    active_snapshot.finish_time.raw_value(),
                    active_snapshot.activation_count,
                    active_snapshot.current_time.raw_value()
                );
                for (i, pt) in pressure_threads.iter().enumerate() {
                    let pressure_snapshot = pt.snapshot();
                    let mut name = FixedStringBuffer::<32>::new();
                    let _ = write!(&mut name, "pressure_thread{}", i + 1);
                    kprintln!(
                        "\t{:<10s}\t{:<20s}  {:>14}  {:>14}  {:>14}  {:>14}  {:>6}  {:>14}",
                        "",
                        name.as_str(),
                        pressure_snapshot.remaining_time_slice_ns.raw_value(),
                        pressure_snapshot.time_slice_ns.raw_value(),
                        pressure_snapshot.start_time.raw_value(),
                        pressure_snapshot.finish_time.raw_value(),
                        pressure_snapshot.activation_count,
                        pressure_snapshot.current_time.raw_value()
                    );
                }
            };

            let _cleanup = zr::defer(|| {
                TestThread::clear_shutdown_barrier();
                owq1.release_all_threads();
                owq2.release_all_threads();
                for t in &pressure_threads {
                    let _ = t.reset(false);
                }
                let _ = active_thread.reset(false);
            });

            TestThread::reset_shutdown_barrier();

            assert_true!(active_thread.create(active_profile.clone()));
            for i in 0..NUM_PRESSURE_THREADS {
                assert_true!(pressure_threads[i].create(pressure_profiles[i].clone()));
            }

            // All of the threads have been created, bound to their respective profiles,
            // and are waiting to run their assigned operations. Pause the test for a
            // duration sufficient for all of the thread's periods to have expired, such
            // that each thread will receive a new activation. This makes it easier to
            // ensure the initial conditions are deterministic.
            let mut max_period = active_profile.period();
            for p in pressure_profiles {
                max_period = core::cmp::max(max_period, p.period());
            }
            let _ = sleep_relative(DurationMono(max_period));

            // Sample the time after the threads have been created and their initial
            // activations have expired, but before the active thread starts executing.
            // This will be our baseline for measuring elapsed time.
            let start_time: zx_instant_mono_t = current_mono_time().0;

            // Have the active thread block on owq1, then have the pressure threads block
            // on owq1.
            let active_op = || {
                print_state("start active");

                // Wait for all pressure threads to block on owq1.
                while (owq_count(owq1) as usize) != NUM_PRESSURE_THREADS {
                    let _ = sleep_relative(DurationMono(zx_msec(1)));
                }

                print_state("pre-spin");
                let pre_spin_activation_count = active_thread.activation_count();

                // Spin for a measurable duration (i.e. the expected consumed capacity) to
                // ensure that the active thread has accumulated a significant portion of
                // its capacity before splitting from owq1.
                let spin_until = current_mono_time().0 + spin_duration;
                while current_mono_time().0 < spin_until {
                    // Spin to consume time slice.
                }

                let post_spin_activation_count = active_thread.activation_count();
                if post_spin_activation_count != pre_spin_activation_count {
                    activation_count_changed.store(true, Ordering::SeqCst);
                }

                print_state("post-spin");

                // Unblock the pressure threads, returning unused capacity.
                owq1.release_all_threads();

                print_state("post-release");

                // Block on owq2 to freeze state for observation.
                let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
                // SAFETY: Preemption is disabled by `_eager_resched_disabler`, and `owq2` remains
                // valid for the duration of the test.
                unsafe {
                    let preempt_disabled = PreemptDisabledToken::new();
                    let _ = owq2.block_and_assign_owner(
                        &Deadline::infinite(),
                        None,
                        ResourceOwnership::Normal,
                        Interruptible::Yes,
                        &preempt_disabled,
                    );
                }
            };
            // SAFETY: All borrowed locals (`owq1`, `owq2`, `owq_count`, `print_state`,
            // `activation_count_changed`, `active_thread`) are declared before `_cleanup`, which
            // joins `active_thread` before any of those locals go out of scope.
            let started_ok = unsafe { active_thread.run_op(active_op) };
            assert_true!(started_ok);

            // Custom operation for pressure threads: block on owq1, then owq2.
            let pressure_op = || {
                print_state("start pressure");

                let _eager_resched_disabler = AnnotatedAutoEagerReschedDisabler::new();
                // SAFETY: Preemption is disabled by `_eager_resched_disabler`, and `owq1`, `owq2`,
                // and `active_thread` remain valid for the duration of the test.
                unsafe {
                    let preempt_disabled = PreemptDisabledToken::new();
                    let _ = owq1.block_and_assign_owner(
                        &Deadline::infinite(),
                        Some(active_thread.thread()),
                        ResourceOwnership::Normal,
                        Interruptible::Yes,
                        &preempt_disabled,
                    );
                    let _ = owq2.block_and_assign_owner(
                        &Deadline::infinite(),
                        None,
                        ResourceOwnership::Normal,
                        Interruptible::Yes,
                        &preempt_disabled,
                    );
                }
            };

            for t in &pressure_threads {
                // SAFETY: `owq1`, `owq2`, `active_thread`, and `print_state` outlive `_cleanup`,
                // which joins all `pressure_threads` before any captured local is dropped.
                let pressure_started_ok = unsafe { t.run_op(pressure_op) };
                assert_true!(pressure_started_ok);
            }

            // Wait for all threads to block on owq2.
            while (owq_count(owq2) as usize) != NUM_PRESSURE_THREADS + 1 {
                let _ = sleep_relative(DurationMono(zx_msec(5)));
            }

            // Sample the time after all threads block on owq2. This will be our
            // end time for measuring elapsed time.
            let end_time: zx_instant_mono_t = current_mono_time().0;

            print_state("end");

            // If the total elapsed time is too long, or the activation count changed,
            // assume VM time loss occurred and retry.
            if end_time - start_time > retry_duration
                || activation_count_changed.load(Ordering::SeqCst)
            {
                retry_test = true;
            } else {
                expect_true!(verify_fn(&active_thread, &pressure_threads));
            }

            owq2.release_all_threads();
            for t in &pressure_threads {
                assert_true!(t.wait_for(TestThreadCondition::WaitingForShutdown));
            }

            retry_count += 1;
            expect_ne!(
                retry_count,
                RETRY_WARNING_COUNT,
                "Reached retry limit due to suspected time loss"
            );

            if !retry_test {
                break;
            }
        }
    }))()
}

fn pi_test_deadline_join_split_helper_uniform<const NUM_PRESSURE_THREADS: usize, F>(
    active_profile: &RefPtr<Profile>,
    pressure_profile: &RefPtr<Profile>,
    spin_duration: zx_duration_mono_t,
    retry_duration: zx_duration_mono_t,
    verify_fn: F,
) -> bool
where
    F: FnMut(&TestThread, &[TestThread; NUM_PRESSURE_THREADS]) -> bool,
{
    let pressure_profiles: [RefPtr<Profile>; NUM_PRESSURE_THREADS] =
        core::array::from_fn(|_| pressure_profile.clone());
    pi_test_deadline_join_split_helper::<NUM_PRESSURE_THREADS, F>(
        active_profile,
        &pressure_profiles,
        spin_duration,
        retry_duration,
        verify_fn,
    )
}

/// Priority inheritance tests for OwnedWaitQueues
#[unittest::suite(name = "pi")]
mod pi_tests {
    use unittest::{assert_eq, assert_ne};

    const EPSILON_PLUS: SchedWeight =
        SchedWeight::from_raw(TEST_DEFAULT_WEIGHT.raw_value() + TEST_EPSILON_WEIGHT.raw_value());
    const EPSILON_MINUS: SchedWeight =
        SchedWeight::from_raw(TEST_DEFAULT_WEIGHT.raw_value() - TEST_EPSILON_WEIGHT.raw_value());

    /// basic
    #[test]
    fn pi_test_basic() {
        let _pboost = AutoProfileBooster::new();

        #[derive(Clone, Copy, PartialEq, Eq)]
        #[repr(u32)]
        enum ReleaseMethod {
            Wake = 0,
            Timeout = 1,
            Kill = 2,
        }

        impl LoopIterValue for ReleaseMethod {
            fn format_iter_val(&self, out: &mut dyn Write) {
                let _ = write!(out, "{}", *self as i64);
            }
        }

        const REL_METHODS: [ReleaseMethod; 3] =
            [ReleaseMethod::Wake, ReleaseMethod::Timeout, ReleaseMethod::Kill];
        const TIMEOUT_RELEASE_DURATION: DurationMono = DurationMono(zx_msec(10));
        const RETRY_LIMIT: u32 = 100;

        // create the array of profiles we will use during the test, then verify that
        // all of them were successfully allocated before proceeding.
        let profiles = [
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(EPSILON_PLUS, InheritableProfile::Yes),
            FairProfile::create(EPSILON_MINUS, InheritableProfile::Yes),
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::No),
            FairProfile::create(EPSILON_PLUS, InheritableProfile::No),
            FairProfile::create(EPSILON_MINUS, InheritableProfile::No),
            DeadlineProfile::create(zx_msec(2), zx_msec(5)),
            DeadlineProfile::create(zx_usec(200), zx_msec(1)),
        ];

        for profile in &profiles {
            assert_true!(profile.is_some());
        }
        let profiles = profiles.map(Option::unwrap);

        // Test every combination of profiles in a test where one thread waits while
        // another thread blocks behind it, applying profile pressure.  Validate that
        // the receiving thread has the proper effective profile after receiving
        // pressure, and that the effective profile relaxes back to the initial
        // profile after the thread applying pressure ceases to do so for each of the
        // various release methods.
        for blocking_profile in &profiles {
            for pressure_profile in &profiles {
                for rel_method in REL_METHODS {
                    print_loop_iter!(blocking_profile, print_blocking_profile);
                    print_loop_iter!(pressure_profile, print_pressure_profile);
                    print_loop_iter!(rel_method, print_rel_method);

                    let mut retry_count: u32 = 0;
                    let mut retry_test: bool;
                    loop {
                        retry_test = false;

                        stack_pin_init!(let owq = LockedOwnedWaitQueue::init());
                        let owq: &LockedOwnedWaitQueue = owq.into_ref().get_ref();
                        let pressure_thread = TestThread::default();
                        let blocking_thread = TestThread::default();
                        let mut expected_profile = ExpectedEffectiveProfile::default();
                        let mut observer = ThreadEffectiveProfileObserver::default();

                        let _cleanup = zr::defer(|| {
                            TestThread::clear_shutdown_barrier();
                            owq.release_all_threads();
                            let _ = pressure_thread.reset(false);
                            let _ = blocking_thread.reset(false);
                        });

                        // Make sure that our default barriers have been reset to their proper
                        // initial states.
                        TestThread::reset_shutdown_barrier();

                        // Create 2 threads, each with the appropriate profile.
                        assert_true!(blocking_thread.create(blocking_profile.clone()));
                        assert_true!(pressure_thread.create(pressure_profile.clone()));

                        // Start the first thread, wait for it to block, and verify that its
                        // profile is correct (it should not be changed).
                        assert_true!(blocking_thread.do_stall());
                        blocking_profile.set_expected_base_profile(&mut expected_profile);
                        observer.observe(blocking_thread.thread());
                        assert_true!(observer.verify_expected_effective_profile(&expected_profile));

                        // Start the second thread, and have it block on the owned wait queue,
                        // and declare the blocking thread to be the owner of the queue at the
                        // same time.  Then check to be sure that the effective priority of the
                        // blocking thread matches what we expect to see.
                        let relative_timeout = if rel_method == ReleaseMethod::Timeout {
                            TIMEOUT_RELEASE_DURATION
                        } else {
                            DurationMono::INFINITE
                        };
                        assert_true!(pressure_thread.block_on_owned_wait_queue(
                            owq,
                            Some(&blocking_thread),
                            relative_timeout,
                        ));

                        // Observe the effective profile of the blocking thread, then observe
                        // the state of the thread applying pressure.  If this is the Timeout
                        // test, the thread *must* still be blocked on `owq` (not timed out yet)
                        // in order for the test to be considered valid.  If the thread managed
                        // to unblock before we could observe its effective priority, just try
                        // again.
                        observer.observe(blocking_thread.thread());
                        if rel_method == ReleaseMethod::Timeout {
                            retry_test = (pressure_thread.tstate() != ThreadStateKind::Blocked)
                                || (pressure_thread.state() == TestThreadState::WaitingForShutdown);
                        }

                        // Only verify this if we managed to observe the blocked thread's
                        // effective profile while the pressure thread was still applying
                        // pressure.
                        if !retry_test {
                            pressure_profile.accumulate_expected_pressure(&mut expected_profile);
                            assert_true!(
                                observer.verify_expected_effective_profile(&expected_profile)
                            );
                        }

                        // Finally, release the thread from the owned wait queue based on
                        // the release method we are testing.  We will either explicitly
                        // wake it up, let it time out, or kill the thread outright.
                        //
                        // Then, verify that the profile drops back down to what it
                        // was initially.
                        match rel_method {
                            ReleaseMethod::Wake => {
                                owq.release_all_threads();
                            }
                            ReleaseMethod::Timeout => {
                                // Wait until the pressure thread times out and has exited.
                                assert_true!(
                                    pressure_thread
                                        .wait_for(TestThreadCondition::WaitingForShutdown)
                                );
                            }
                            ReleaseMethod::Kill => {
                                let _ = pressure_thread.reset(true);
                            }
                        }

                        blocking_profile.set_expected_base_profile(&mut expected_profile);
                        observer.observe(blocking_thread.thread());
                        assert_true!(observer.verify_expected_effective_profile(&expected_profile));

                        retry_count += 1;
                        if !(retry_test && (retry_count < RETRY_LIMIT)) {
                            break;
                        }
                    }

                    assert_false!(retry_test, "Failed timeout race too many times!");

                    print_blocking_profile.cancel();
                    print_pressure_profile.cancel();
                    print_rel_method.cancel();
                }
            }
        }
    }

    /// critical active
    #[test]
    fn pi_test_critical_active() {
        let _pboost = AutoProfileBooster::new();

        let fair_profile = FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes);
        let critical_profile = DeadlineProfile::create_with_critical(
            zx_msec(2),
            zx_msec(5),
            true, // true for critical
        );

        assert_true!(fair_profile.is_some());
        assert_true!(critical_profile.is_some());
        let fair_profile = fair_profile.unwrap();
        let critical_profile = critical_profile.unwrap();

        stack_pin_init!(let owq = LockedOwnedWaitQueue::init());
        let owq: &LockedOwnedWaitQueue = owq.into_ref().get_ref();
        let active_thread = TestThread::default();
        let pressure_thread = TestThread::default();
        let stop_loop = AtomicBool::new(false);

        let _cleanup = zr::defer(|| {
            stop_loop.store(true, Ordering::SeqCst);
            TestThread::clear_shutdown_barrier();
            owq.release_all_threads();
            let _ = pressure_thread.reset(false);
            let _ = active_thread.reset(false);
        });

        TestThread::reset_shutdown_barrier();

        assert_true!(active_thread.create(fair_profile.clone()));
        assert_true!(pressure_thread.create(critical_profile.clone()));

        // Run the active thread in a soft-yield loop
        let active_op = || {
            while !stop_loop.load(Ordering::SeqCst) {
                r#yield();
            }
        };
        // SAFETY: `stop_loop` outlives `_cleanup`, which joins `active_thread` on scope exit.
        let started_ok = unsafe { active_thread.run_op(active_op) };
        assert_true!(started_ok);

        // Have the pressure thread block on the owq owned by active_thread.
        assert_true!(pressure_thread.block_on_owned_wait_queue(
            owq,
            Some(&active_thread),
            DurationMono::INFINITE,
        ));

        // Verify that active_thread inherited criticality.
        let mut observer = ThreadEffectiveProfileObserver::default();
        observer.observe(active_thread.thread());
        let mut expected_profile = ExpectedEffectiveProfile::default();
        fair_profile.set_expected_base_profile(&mut expected_profile);
        critical_profile.accumulate_expected_pressure(&mut expected_profile);
        assert_true!(observer.verify_expected_effective_profile(&expected_profile));

        // Release the pressure
        owq.release_all_threads();
        assert_true!(pressure_thread.wait_for(TestThreadCondition::WaitingForShutdown));

        // Stop the active loop
        stop_loop.store(true, Ordering::SeqCst);
        assert_true!(active_thread.wait_for(TestThreadCondition::WaitingForShutdown));
    }

    /// changing priority
    #[test]
    fn pi_test_changing_priority() {
        let _pboost = AutoProfileBooster::new();
        stack_pin_init!(let owq = LockedOwnedWaitQueue::init());
        let owq: &LockedOwnedWaitQueue = owq.into_ref().get_ref();
        let pressure_thread = TestThread::default();
        let blocking_thread = TestThread::default();

        let _cleanup = zr::defer(|| {
            TestThread::clear_shutdown_barrier();
            owq.release_all_threads();
            let _ = pressure_thread.reset(false);
            let _ = blocking_thread.reset(false);
        });

        let profiles = [
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(EPSILON_PLUS, InheritableProfile::Yes),
            FairProfile::create(EPSILON_MINUS, InheritableProfile::Yes),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::No),
            FairProfile::create(EPSILON_PLUS, InheritableProfile::No),
            FairProfile::create(EPSILON_MINUS, InheritableProfile::No),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::No),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::No),
            DeadlineProfile::create(zx_msec(2), zx_msec(5)),
            DeadlineProfile::create(zx_usec(200), zx_msec(1)),
        ];

        for profile in &profiles {
            assert_true!(profile.is_some());
        }
        let profiles = profiles.map(Option::unwrap);

        let mut expected_profile = ExpectedEffectiveProfile::default();
        let mut observer = ThreadEffectiveProfileObserver::default();

        // Make sure that our default barriers have been reset to their proper
        // initial states.
        TestThread::reset_shutdown_barrier();

        // Create our threads.
        assert_true!(blocking_thread.create(profiles[0].clone()));
        assert_true!(pressure_thread.create(profiles[0].clone()));

        // Start the first thread, wait for it to block, and verify that its
        // profile is correct (it should not be changed).
        assert_true!(blocking_thread.do_stall());
        blocking_thread.initial_profile().set_expected_base_profile(&mut expected_profile);
        observer.observe(blocking_thread.thread());
        assert_true!(observer.verify_expected_effective_profile(&expected_profile));

        // Block the second thread behind the first.
        assert_true!(pressure_thread.block_on_owned_wait_queue(
            owq,
            Some(&blocking_thread),
            DurationMono::INFINITE,
        ));
        pressure_thread.initial_profile().accumulate_expected_pressure(&mut expected_profile);
        observer.observe(blocking_thread.thread());
        assert_true!(observer.verify_expected_effective_profile(&expected_profile));

        // Changing the pressure thread's profile to a number of different profiles,
        // verifying that the pressure felt by the blocking thread changes
        // appropriately as we do.
        for profile in &profiles {
            print_loop_iter!(profile, print_profile);

            profile.apply(pressure_thread.thread());
            observer.observe(blocking_thread.thread());

            blocking_thread.initial_profile().set_expected_base_profile(&mut expected_profile);
            profile.accumulate_expected_pressure(&mut expected_profile);
            assert_true!(observer.verify_expected_effective_profile(&expected_profile));

            print_profile.cancel();
        }

        // Release the pressure thread, validate that the priority is what we
        // started with and we are done.
        owq.release_all_threads();
        blocking_thread.initial_profile().set_expected_base_profile(&mut expected_profile);
        observer.observe(blocking_thread.thread());
        assert_true!(observer.verify_expected_effective_profile(&expected_profile));
    }

    /// changing priority in WaitQueue
    #[test]
    fn pi_test_changing_priority_in_wait_queue() {
        // A simple smoke test to make sure that we can change a thread's base priority
        // when it is blocked in a normal wait queue (instead of an owned wait queue)
        let _pboost = AutoProfileBooster::new();
        stack_pin_init!(let wq = WaitQueue::init());
        let wq: &WaitQueue = wq.into_ref().get_ref();
        let thread = TestThread::default();

        let _cleanup = zr::defer(|| {
            TestThread::clear_shutdown_barrier();
            wq.wake_all(Ok(()));
            let _ = thread.reset(false);
        });

        let profiles = [
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(EPSILON_PLUS, InheritableProfile::Yes),
            FairProfile::create(EPSILON_MINUS, InheritableProfile::Yes),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::Yes),
            DeadlineProfile::create(zx_msec(2), zx_msec(5)),
            DeadlineProfile::create(zx_usec(200), zx_msec(1)),
        ];

        for profile in &profiles {
            assert_true!(profile.is_some());
        }
        let profiles = profiles.map(Option::unwrap);

        let mut expected_profile = ExpectedEffectiveProfile::default();
        let mut observer = ThreadEffectiveProfileObserver::default();

        // Make sure that our default barriers have been reset to their proper
        // initial states.
        TestThread::reset_shutdown_barrier();

        // Create our thread and have it block in our wait queue.
        assert_true!(thread.create(profiles[0].clone()));
        assert_true!(thread.block_on_wait_queue(wq, DurationMono::INFINITE));
        thread.initial_profile().set_expected_base_profile(&mut expected_profile);
        observer.observe(thread.thread());
        assert_true!(observer.verify_expected_effective_profile(&expected_profile));

        // Changing the thread's base profile while it is blocked in the queue.  Its
        // effective profile should always match the base profile we set.
        for profile in &profiles {
            print_loop_iter!(profile, print_profile);

            profile.apply(thread.thread());
            profile.set_expected_base_profile(&mut expected_profile);
            observer.observe(thread.thread());
            assert_true!(observer.verify_expected_effective_profile(&expected_profile));

            print_profile.cancel();
        }
    }

    /// chains
    #[test]
    fn pi_test_chain() {
        #[pin_data]
        struct Link {
            #[pin]
            queue: LockedOwnedWaitQueue,
            active: Cell<bool>,
        }

        const CHAIN_LEN: usize = 8;
        const LINK_LEN: usize = CHAIN_LEN - 1;

        let profile_deck = [
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::No),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::No),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::No),
            DeadlineProfile::create(zx_msec(2), zx_msec(5)),
            DeadlineProfile::create(zx_usec(200), zx_msec(1)),
        ];

        const PRIORITY_GENERATORS: [DistroSpec; 4] = [
            DistroSpec::new(DistroSpecType::Ascending),
            DistroSpec::new(DistroSpecType::Descending),
            DistroSpec::with_seed(DistroSpecType::Shuffle, 0xbd6f_3bfe_33d5_1c8e),
            DistroSpec::with_seed(DistroSpecType::Shuffle, 0x857c_e1aa_3209_ecc7),
        ];

        const RELEASE_ORDERS: [DistroSpec; 4] = [
            DistroSpec::new(DistroSpecType::Ascending),
            DistroSpec::new(DistroSpecType::Descending),
            DistroSpec::with_seed(DistroSpecType::Shuffle, 0xac8d_4a8e_d016_caf0),
            DistroSpec::with_seed(DistroSpecType::Shuffle, 0xb51e_76ca_5cf2_0875),
        ];

        let threads: [TestThread; CHAIN_LEN] = core::array::from_fn(|_| TestThread::default());
        stack_pin_init!(
            let links = pin_init_array_from_fn::<_, LINK_LEN, _, _>(|_| {
                pin_init!(Link {
                    queue <- LockedOwnedWaitQueue::init(),
                    active: Cell::new(false),
                })
            })
        );
        let links: &[Link; LINK_LEN] = links.into_ref().get_ref();
        let mut release_deck = [0usize; LINK_LEN];

        for profile in &profile_deck {
            assert_true!(profile.is_some());
        }
        let profile_deck = profile_deck.map(Option::unwrap);

        for (i, slot) in release_deck.iter_mut().enumerate() {
            *slot = i;
        }

        let _pboost = AutoProfileBooster::new();
        for pgen_ndx in 0..PRIORITY_GENERATORS.len() as u32 {
            print_loop_iter!(pgen_ndx, print_pgen_ndx);

            // Generate the profile order to use for this pass.
            let mut profiles: [RefPtr<Profile>; CHAIN_LEN] =
                core::array::from_fn(|i| profile_deck[i].clone());
            PRIORITY_GENERATORS[pgen_ndx as usize].apply(&profile_deck, &mut profiles);

            for ro_ndx in 0..RELEASE_ORDERS.len() as u32 {
                print_loop_iter!(ro_ndx, print_ro_ndx);

                // Generate the order in which we will release the links for this
                // pass
                let mut release_ordering = [0usize; LINK_LEN];
                RELEASE_ORDERS[ro_ndx as usize].apply(&release_deck, &mut release_ordering);

                let _cleanup = zr::defer(|| {
                    TestThread::clear_shutdown_barrier();
                    for l in links {
                        l.queue.release_all_threads();
                    }
                    for t in &threads {
                        let _ = t.reset(false);
                    }
                });

                // Lambda used to validate the effective profiles of the threads currently
                // involved in the chain.
                let validate_priorities = subtest!(|| {
                    for tndx in (0..threads.len()).rev() {
                        print_loop_iter!(tndx, print_tndx);

                        // All threads should either be created, started or waiting for
                        // shutdown.  If they are merely created, they have no effective
                        // priority to evaluate at the moment, so just skip them.
                        let t = &threads[tndx];
                        let cur_state = t.state();
                        if cur_state == TestThreadState::Created {
                            print_tndx.cancel();
                            continue;
                        }

                        if cur_state != TestThreadState::WaitingForShutdown {
                            assert_eq!(TestThreadState::Started, cur_state);
                        }

                        // The effective profile of this thread should be its base profile,
                        // plus all of the profile pressure received from the actively linked
                        // thread.
                        let mut expected_profile = ExpectedEffectiveProfile::default();
                        threads[tndx]
                            .initial_profile()
                            .set_expected_base_profile(&mut expected_profile);
                        let mut i = tndx;
                        while (i < links.len()) && links[i].active.get() {
                            assert_lt!(i + 1, threads.len());
                            threads[i + 1]
                                .initial_profile()
                                .accumulate_expected_pressure(&mut expected_profile);
                            i += 1;
                        }

                        let mut observer = ThreadEffectiveProfileObserver::default();
                        observer.observe(threads[tndx].thread());
                        assert_true!(observer.verify_expected_effective_profile(&expected_profile));

                        print_tndx.cancel();
                    }
                });

                // Make sure that our default barriers have been reset to their proper
                // initial states.
                TestThread::reset_shutdown_barrier();

                // Create our threads.
                zr::static_assert!(CHAIN_LEN == 8);
                for tndx in 0..threads.len() as u32 {
                    print_loop_iter!(tndx, print_tndx);
                    assert_true!(threads[tndx as usize].create(profiles[tndx as usize].clone()));
                    print_tndx.cancel();
                }

                // Start the head of the chain, wait for it to block, then verify that its
                // profile is correct (it should not be changed).
                let chain_head = &threads[0];
                assert_true!(chain_head.do_stall());
                assert_true!(validate_priorities());

                // Start each of the threads in the chain one at a time.  Make sure that the
                // pressure of the threads in the chain is properly transmitted each time.
                for tndx in 1..threads.len() as u32 {
                    print_loop_iter!(tndx, print_tndx);

                    let link = &links[(tndx - 1) as usize];
                    assert_true!(threads[tndx as usize].block_on_owned_wait_queue(
                        &link.queue,
                        Some(&threads[(tndx - 1) as usize]),
                        DurationMono::INFINITE,
                    ));
                    link.active.set(true);
                    assert_true!(validate_priorities());

                    print_tndx.cancel();
                }

                // Tear down the chain according to the release ordering for this
                // pass.  Make sure that the priority properly relaxes for each of
                // the threads as we do so.
                for link_ndx in release_ordering {
                    print_loop_iter!(link_ndx, print_link_ndx);

                    assert_lt!(link_ndx, links.len());
                    let link = &links[link_ndx];
                    link.queue.release_all_threads();
                    link.active.set(false);
                    assert_true!(validate_priorities());

                    print_link_ndx.cancel();
                }

                print_ro_ndx.cancel();
            }

            print_pgen_ndx.cancel();
        }
    }

    /// multiple waiters
    #[test]
    fn pi_test_multi_waiter() {
        let _pboost = AutoProfileBooster::new();

        const WAITER_CNT: usize = 8;
        let profile_deck = [
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::No),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::No),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::No),
            DeadlineProfile::create(zx_msec(2), zx_msec(5)),
            DeadlineProfile::create(zx_usec(200), zx_msec(1)),
        ];
        for profile in &profile_deck {
            assert_true!(profile.is_some());
        }
        let profile_deck = profile_deck.map(Option::unwrap);

        const PRIORITY_GENERATORS: [DistroSpec; 4] = [
            DistroSpec::new(DistroSpecType::Ascending),
            DistroSpec::new(DistroSpecType::Descending),
            DistroSpec::with_seed(DistroSpecType::Random, 0x8725_1211_471c_b789),
            DistroSpec::with_seed(DistroSpecType::Shuffle, 0x857c_e1aa_3209_ecc7),
        ];

        stack_pin_init!(let blocking_queue = LockedOwnedWaitQueue::init());
        let blocking_queue: &LockedOwnedWaitQueue = blocking_queue.into_ref().get_ref();
        let blocking_thread = TestThread::default();

        #[derive(Default)]
        struct Waiter {
            thread: TestThread,
            started: Cell<bool>,
            is_waiting: Cell<bool>,
        }

        impl Waiter {
            fn reset(&self) {
                let _ = self.thread.reset(false);
                self.started.set(false);
                self.is_waiting.set(false);
            }
        }

        let waiters: [Waiter; WAITER_CNT] = core::array::from_fn(|_| Waiter::default());

        for bt_profile in &profile_deck {
            print_loop_iter!(bt_profile, print_bt_profile);

            for pgen_ndx in 0..PRIORITY_GENERATORS.len() as u32 {
                print_loop_iter!(pgen_ndx, print_pgen_ndx);

                // At the end of the tests, success or failure, be sure to clean up.
                let _cleanup = zr::defer(|| {
                    TestThread::clear_shutdown_barrier();
                    blocking_queue.release_all_threads();
                    let _ = blocking_thread.reset(false);
                    for w in &waiters {
                        w.reset();
                    }
                });

                // Make sure that our barriers have been reset.
                TestThread::reset_shutdown_barrier();

                // Select the profiles to apply to the waiter threads during this pass.
                let mut profiles: [RefPtr<Profile>; WAITER_CNT] =
                    core::array::from_fn(|i| profile_deck[i].clone());
                PRIORITY_GENERATORS[pgen_ndx as usize].apply(&profile_deck, &mut profiles);

                // Create all of the threads.
                assert_true!(blocking_thread.create(bt_profile.clone()));
                for waiter_ndx in 0..waiters.len() as u32 {
                    print_loop_iter!(waiter_ndx, print_waiter_ndx);

                    zr::static_assert!(WAITER_CNT == 8);
                    assert_true!(
                        waiters[waiter_ndx as usize]
                            .thread
                            .create(profiles[waiter_ndx as usize].clone())
                    );

                    print_waiter_ndx.cancel();
                }

                // Define a small lambda we will use to validate the expected priorities of
                // each of our threads.
                let current_owner: Cell<Option<&TestThread>> = Cell::new(Some(&blocking_thread));
                let validate_priorities = subtest!(|| {
                    let mut expected_profile = ExpectedEffectiveProfile::default();
                    let mut observer = ThreadEffectiveProfileObserver::default();

                    // The expected profile for the current owner of the OWQ should be its
                    // base profile, plus all of the pressure from each of the waiting
                    // threads.
                    let owner = current_owner.get();
                    assert_true!(owner.is_some());
                    let owner = owner.unwrap();
                    owner.initial_profile().set_expected_base_profile(&mut expected_profile);
                    for waiter in &waiters {
                        if waiter.is_waiting.get() {
                            waiter
                                .thread
                                .initial_profile()
                                .accumulate_expected_pressure(&mut expected_profile);
                        }
                    }

                    observer.observe(owner.thread());
                    assert_true!(observer.verify_expected_effective_profile(&expected_profile));

                    // Every waiter thread which has started (waiting or not) should be
                    // running with its initial profile, unless it happens to currently be
                    // the owner of the OWQ.
                    for (waiter_ndx, waiter) in waiters.iter().enumerate() {
                        print_loop_iter!(waiter_ndx, print_waiter_ndx);

                        if waiter.started.get() && !core::ptr::eq(&waiter.thread, owner) {
                            waiter
                                .thread
                                .initial_profile()
                                .set_expected_base_profile(&mut expected_profile);
                            observer.observe(waiter.thread.thread());
                            assert_true!(
                                observer.verify_expected_effective_profile(&expected_profile)
                            );
                        }

                        print_waiter_ndx.cancel();
                    }
                });

                // Start the blocking thread.
                assert_true!(blocking_thread.do_stall());
                assert_true!(validate_priorities());

                // Start each of the threads and have them block on the blocking_queue,
                // declaring blocking_thread to be the owner as they go.  Verify all of
                // the threads' effective profiles as we go.
                for waiter_ndx in 0..waiters.len() as u32 {
                    print_loop_iter!(waiter_ndx, print_waiter_ndx);

                    let w = &waiters[waiter_ndx as usize];
                    assert_true!(w.thread.block_on_owned_wait_queue(
                        blocking_queue,
                        current_owner.get(),
                        DurationMono::INFINITE,
                    ));
                    w.started.set(true);
                    w.is_waiting.set(true);
                    assert_true!(validate_priorities());

                    print_waiter_ndx.cancel();
                }

                // Now wake the threads, one at a time, assigning ownership to the thread
                // which was woken each time.  Note that we should not be assuming which
                // thread is going to be woken.  We will need to request that a thread be
                // woken, then figure out after the fact which one was.
                for tndx in 0..waiters.len() as u32 {
                    print_loop_iter!(tndx, print_tndx);

                    blocking_queue.release_one_thread();

                    let mut new_owner: Option<&TestThread> = None;
                    let deadline = current_mono_time().0 + zx_sec(10);
                    while current_mono_time().0 < deadline {
                        for w in &waiters {
                            // If the waiter's is_waiting flag is set, but the thread has
                            // reached the WaitingForShutdown state, then we know that
                            // this was a thread which was just woken.
                            if w.is_waiting.get()
                                && (w.thread.state() == TestThreadState::WaitingForShutdown)
                            {
                                new_owner = Some(&w.thread);
                                w.is_waiting.set(false);
                                break;
                            }
                        }

                        if new_owner.is_some() {
                            break;
                        }

                        let _ = sleep_relative(DurationMono(zx_usec(100)));
                    }

                    // Sanity checks.  Make sure that the new owner exists, and is not the
                    // same as the old owner.  Also make sure that none of the other threads
                    // have been released but have not been recognized yet.
                    assert_true!(new_owner.is_some());
                    let new_owner_ref = new_owner.unwrap();
                    assert_ne!(
                        core::ptr::from_ref(new_owner_ref),
                        core::ptr::from_ref(current_owner.get().unwrap())
                    );
                    for w in &waiters {
                        if w.is_waiting.get() {
                            assert_eq!(TestThreadState::Started, w.thread.state());
                        } else {
                            assert_eq!(TestThreadState::WaitingForShutdown, w.thread.state());
                        }
                    }
                    current_owner.set(Some(new_owner_ref));

                    // Validate our profiles.
                    assert_true!(validate_priorities());

                    print_tndx.cancel();
                }

                print_pgen_ndx.cancel();
            }
            print_bt_profile.cancel();
        }
    }

    /// multiple owned queues
    #[test]
    fn pi_test_multi_owned_queues() {
        let _pboost = AutoProfileBooster::new();

        const QUEUE_CNT: usize = 8;
        let profile_deck = [
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::Yes),
            FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::No),
            FairProfile::create(TEST_LOWEST_WEIGHT, InheritableProfile::No),
            FairProfile::create(TEST_HIGHEST_WEIGHT, InheritableProfile::No),
            DeadlineProfile::create(zx_msec(2), zx_msec(5)),
            DeadlineProfile::create(zx_usec(200), zx_msec(1)),
        ];
        for profile in &profile_deck {
            assert_true!(profile.is_some());
        }
        let profile_deck = profile_deck.map(Option::unwrap);

        #[pin_data]
        struct Waiter {
            thread: TestThread,
            #[pin]
            queue: LockedOwnedWaitQueue,
            is_started: Cell<bool>,
            is_waiting: Cell<bool>,
        }

        impl Waiter {
            fn reset(&self) {
                self.queue.release_all_threads();
                let _ = self.thread.reset(false);
                self.is_started.set(false);
                self.is_waiting.set(false);
            }
        }

        let blocking_thread = TestThread::default();
        stack_pin_init!(
            let queues = pin_init_array_from_fn::<_, QUEUE_CNT, _, _>(|_| {
                pin_init!(Waiter {
                    thread: TestThread::default(),
                    queue <- LockedOwnedWaitQueue::init(),
                    is_started: Cell::new(false),
                    is_waiting: Cell::new(false),
                })
            })
        );
        let queues: &[Waiter; QUEUE_CNT] = queues.into_ref().get_ref();

        const PRIORITY_GENERATORS: [DistroSpec; 4] = [
            DistroSpec::new(DistroSpecType::Ascending),
            DistroSpec::new(DistroSpecType::Descending),
            DistroSpec::with_seed(DistroSpecType::Random, 0xb89e_3b74_42b9_5a1c),
            DistroSpec::with_seed(DistroSpecType::Shuffle, 0x06ec_82d4_ade8_efba),
        ];

        for bt_profile in &profile_deck {
            print_loop_iter!(bt_profile, print_bt_profile);

            for pgen_ndx in 0..PRIORITY_GENERATORS.len() as u32 {
                print_loop_iter!(pgen_ndx, print_pgen_ndx);

                // At the end of the tests, success or failure, be sure to clean up.
                let _cleanup = zr::defer(|| {
                    TestThread::clear_shutdown_barrier();
                    let _ = blocking_thread.reset(false);
                    for q in queues {
                        q.reset();
                    }
                });

                // Make sure that our barriers have been reset.
                TestThread::reset_shutdown_barrier();

                // Select the profiles to apply to the waiter threads during this pass.
                let mut profiles: [RefPtr<Profile>; QUEUE_CNT] =
                    core::array::from_fn(|i| profile_deck[i].clone());
                PRIORITY_GENERATORS[pgen_ndx as usize].apply(&profile_deck, &mut profiles);

                // Create all of the threads.
                assert_true!(blocking_thread.create(bt_profile.clone()));
                for queue_ndx in 0..queues.len() as u32 {
                    print_loop_iter!(queue_ndx, print_queue_ndx);
                    assert_true!(
                        queues[queue_ndx as usize]
                            .thread
                            .create(profiles[queue_ndx as usize].clone())
                    );
                    print_queue_ndx.cancel();
                }

                // Define a small lambda we will use to validate the expected priorities of
                // each of our threads.
                let validate_priorities = subtest!(|| {
                    let mut expected_profile = ExpectedEffectiveProfile::default();
                    let mut observer = ThreadEffectiveProfileObserver::default();

                    // Each of the started queue threads (waiting or not) should simply have
                    // their base profile.  Verify this.
                    for queue_ndx in 0..queues.len() as u32 {
                        print_loop_iter!(queue_ndx, print_queue_ndx);
                        let q = &queues[queue_ndx as usize];

                        if q.is_started.get() {
                            q.thread
                                .initial_profile()
                                .set_expected_base_profile(&mut expected_profile);
                            observer.observe(q.thread.thread());
                            assert_true!(
                                observer.verify_expected_effective_profile(&expected_profile)
                            );
                        }

                        print_queue_ndx.cancel();
                    }

                    // The effective profile of the blocking_thread should be the
                    // combination of its base profile, in addition to all of the currently
                    // blocked threads whose OWQs are owned by the blocking thread.
                    blocking_thread
                        .initial_profile()
                        .set_expected_base_profile(&mut expected_profile);
                    for q in queues {
                        if q.is_waiting.get() {
                            q.thread
                                .initial_profile()
                                .accumulate_expected_pressure(&mut expected_profile);
                        }
                    }

                    observer.observe(blocking_thread.thread());
                    assert_true!(observer.verify_expected_effective_profile(&expected_profile));
                });

                // Start the blocking thread.
                assert_true!(blocking_thread.do_stall());
                assert_true!(validate_priorities());

                // Start each of the threads and have them block on their associated
                // queue, declaring blocking_thread to be the owner of their queue
                // as they go.  Validate priorities at each step.
                for queue_ndx in 0..queues.len() as u32 {
                    print_loop_iter!(queue_ndx, print_queue_ndx);

                    let q = &queues[queue_ndx as usize];
                    assert_true!(q.thread.block_on_owned_wait_queue(
                        &q.queue,
                        Some(&blocking_thread),
                        DurationMono::INFINITE,
                    ));
                    q.is_started.set(true);
                    q.is_waiting.set(true);
                    assert_true!(validate_priorities());

                    print_queue_ndx.cancel();
                }

                // Now wake the threads, one at a time, verifying priorities as we
                // go.
                for queue_ndx in 0..queues.len() as u32 {
                    print_loop_iter!(queue_ndx, print_queue_ndx);

                    let q = &queues[queue_ndx as usize];
                    q.queue.release_one_thread();
                    q.is_waiting.set(false);
                    assert_true!(validate_priorities());

                    print_queue_ndx.cancel();
                }

                print_pgen_ndx.cancel();
            }
            print_bt_profile.cancel();
        }
    }

    /// cycles (inheritable)
    #[test]
    fn cycles_inheritable() {
        assert_true!(pi_test_cycle(InheritableProfile::Yes));
    }

    /// cycles (non-inheritable)
    #[test]
    fn cycles_non_inheritable() {
        assert_true!(pi_test_cycle(InheritableProfile::No));
    }

    /// b/42182770 regression test
    #[test]
    fn bug_42182770_regression() {
        // Set up and test a situation which mimics the one encountered during bug 42182770.
        // Basically, we want to:
        //
        // 1) Block a thread A which has a fair profile in an owned wait queue W.  Be
        //    sure to test both inheritable and non-inheritable profiles.
        // 2) Observe that the start time, finish time, and time slice of W (the
        //    "dynamic" IPVs) remain zero (the initial defaults).  A has blocked in W,
        //    and it was the first thread to block, but its profile is
        //    non-inheritable, therefore the dynamic parameters remain undefined.
        // 3) Block a thread B, which has an inheritable deadline profile in W.
        // 4) Observe that W's IPVs have taken on B's dynamic scheduler profile values.
        // 5) Unblock B from W.  Observe that W still has IPV storage allocated to it,
        //    but that the static parameters (total weight, uncapped utilization, min
        //    deadline) have returned to their default values.  If extra scheduler
        //    invariant validation is turned on, the dynamic parameters should have
        //    returned to their defaults as well, otherwise they should remain
        //    unchanged.
        // 6) Unblock A from W.  After this operation, W should no longer have any
        //    inherited profile value storage allocated to it as it no longer has any
        //    waiting threads.

        const INHERITABLE_OPTIONS: [InheritableProfile; 2] =
            [InheritableProfile::No, InheritableProfile::Yes];
        for allow_inherit in INHERITABLE_OPTIONS {
            // Create the queue, profiles, and threads we will need to run the test.
            let owq = LockedOwnedWaitQueue::try_new();
            assert_true!(owq.is_ok());
            let owq = owq.unwrap();

            let fair_profile = FairProfile::create(TEST_DEFAULT_WEIGHT, allow_inherit);
            let deadline_profile = DeadlineProfile::create(zx_msec(2), zx_msec(5));
            assert_true!(fair_profile.is_some());
            assert_true!(deadline_profile.is_some());
            let fair_profile = fair_profile.unwrap();
            let deadline_profile = deadline_profile.unwrap();

            let inheritable_weight = if allow_inherit == InheritableProfile::Yes {
                TEST_DEFAULT_WEIGHT
            } else {
                SchedWeight::from_raw(0)
            };

            let fair_thread = TestThread::default();
            let deadline_thread = TestThread::default();
            assert_true!(fair_thread.create(fair_profile));
            assert_true!(deadline_thread.create(deadline_profile.clone()));

            // Make sure that our default barriers have been reset to their proper initial
            // states, and that we will properly release threads from their blocked state
            // if anything goes wrong during the test and we bail out.
            TestThread::reset_shutdown_barrier();
            let _cleanup = zr::defer(|| {
                TestThread::clear_shutdown_barrier();
                owq.release_all_threads();
                let _ = fair_thread.reset(false);
                let _ = deadline_thread.reset(false);
            });

            // Verify that the OWQ has no inherited profile storage allocated to it.  This
            // should always be the case when there are no waiters.
            expect_false!(owq.has_inherited_scheduler_state_storage());

            // Now, block the fair thread on the queue, and verify the IPV state.  There
            // should be storage allocated at this point in time, but because the
            // blocked thread's profile is not inheritable, there should be no inherited
            // utilization in the queue's values.
            assert_true!(fair_thread.block_on_owned_wait_queue(&owq, None, DurationMono::INFINITE));
            {
                let owq_base: &OwnedWaitQueue = &owq;
                single_chain_lock_guard_owned!(
                    guard,
                    owq_base,
                    IRQ_SAVE_OPTION,
                    clt_tag!("pi_tests::bug_42182770_regression #1")
                );
                let proof = guard.proof();
                let iss_opt = proof.inherited_scheduler_state_storage();
                assert_true!(iss_opt.is_some());

                // SAFETY: `guard` holds `owq`'s lock, which synchronizes access to its inherited
                // scheduler state storage.
                let iss: &WaitQueueInheritedSchedulerState = unsafe { iss_opt.unwrap().as_ref() };
                expect_eq!(iss.ipvs.total_weight.raw_value(), inheritable_weight.raw_value());
                expect_eq!(
                    iss.ipvs.uncapped_utilization.raw_value(),
                    SchedUtilization::from_raw(0).raw_value()
                );
                expect_eq!(iss.ipvs.min_deadline.raw_value(), i64::MAX);
                expect_eq!(iss.start_time.raw_value(), SchedTime::from_raw(0).raw_value());
                expect_eq!(iss.finish_time.raw_value(), SchedTime::from_raw(0).raw_value());
                expect_eq!(iss.time_slice_ns.raw_value(), SchedDuration::from_raw(0).raw_value());
            }

            // Next, block the deadline thread.  After this, the queue should have
            // inherited the blocked thread's utilization, as well as its dynamic
            // parameters since it is the only "consequential" thread blocked in the
            // queue.
            assert_true!(deadline_thread.block_on_owned_wait_queue(
                &owq,
                None,
                DurationMono::INFINITE,
            ));
            {
                let owq_base: &OwnedWaitQueue = &owq;
                single_chain_lock_guard_owned!(
                    guard,
                    owq_base,
                    IRQ_SAVE_OPTION,
                    clt_tag!("pi_tests::bug_42182770_regression #2")
                );
                let proof = guard.proof();
                let iss_opt = proof.inherited_scheduler_state_storage();
                assert_true!(iss_opt.is_some());

                let params = deadline_profile.as_deadline().unwrap().sched_params();
                // SAFETY: `guard` holds `owq`'s lock, which synchronizes access to its inherited
                // scheduler state storage.
                let iss: &WaitQueueInheritedSchedulerState = unsafe { iss_opt.unwrap().as_ref() };
                expect_eq!(iss.ipvs.total_weight.raw_value(), inheritable_weight.raw_value());
                expect_eq!(
                    iss.ipvs.uncapped_utilization.raw_value(),
                    params.utilization.raw_value()
                );
                expect_eq!(iss.ipvs.min_deadline.raw_value(), params.deadline_ns.raw_value());
                expect_ne!(iss.start_time.raw_value(), SchedTime::from_raw(0).raw_value());
                expect_ne!(iss.finish_time.raw_value(), SchedTime::from_raw(0).raw_value());
                expect_ne!(iss.time_slice_ns.raw_value(), SchedDuration::from_raw(0).raw_value());
            }

            // Wake the deadline thread from the owned wait queue.  We cannot 100% control
            // which thread will be woken if we simply use WakeOne as the choice of
            // which thread to wake is part of the scheduler's logic, and not something we
            // can guarantee over time.
            //
            // Instead, we can force the thread we want to wake by sending it either a
            // suspend, or kill signal (in this case, we are going to send it the kill
            // signal).
            //
            // Once the thread has woken and exited, observe that the OWQ's IPVs have gone
            // back to no inherited weight or utilization.  If we have extra scheduler
            // invariant validation turned on, we should see the dynamic parameters get
            // reset as well (even though they are not normally reset as they are now
            // considered to be undefined).
            assert_true!(deadline_thread.reset(true));
            {
                let owq_base: &OwnedWaitQueue = &owq;
                single_chain_lock_guard_owned!(
                    guard,
                    owq_base,
                    IRQ_SAVE_OPTION,
                    clt_tag!("pi_tests::bug_42182770_regression #3")
                );
                let proof = guard.proof();
                let iss_opt = proof.inherited_scheduler_state_storage();
                assert_true!(iss_opt.is_some());

                // SAFETY: `guard` holds `owq`'s lock, which synchronizes access to its inherited
                // scheduler state storage.
                let iss: &WaitQueueInheritedSchedulerState = unsafe { iss_opt.unwrap().as_ref() };
                expect_eq!(iss.ipvs.total_weight.raw_value(), inheritable_weight.raw_value());
                expect_eq!(
                    iss.ipvs.uncapped_utilization.raw_value(),
                    SchedUtilization::from_raw(0).raw_value()
                );
                expect_eq!(iss.ipvs.min_deadline.raw_value(), i64::MAX);
                if cfg!(scheduler_extra_invariant_validation) {
                    expect_eq!(iss.start_time.raw_value(), SchedTime::from_raw(0).raw_value());
                    expect_eq!(iss.finish_time.raw_value(), SchedTime::from_raw(0).raw_value());
                    expect_eq!(
                        iss.time_slice_ns.raw_value(),
                        SchedDuration::from_raw(0).raw_value()
                    );
                }
            }

            // Finally, release the fair thread, confirm that the IPV storage no longer
            // exists in the OWQ, and shutdown the test.
            TestThread::clear_shutdown_barrier();
            owq.release_all_threads();
            assert_true!(fair_thread.reset(false));
            assert_false!(owq.has_inherited_scheduler_state_storage());
        }
    }

    /// deadline to fair join and split
    #[test]
    fn pi_test_deadline_to_fair_join_split() {
        // Tests bandwidth distribution when a deadline thread blocks as upstream node
        // behind a fair target thread. This results in a deadline-to-fair PI graph join
        // and a subsequent deadline-from-fair PI graph split.
        let period: zx_duration_mono_t = zx_sec(1);
        let capacity: zx_duration_mono_t = period;
        let spin_duration: zx_duration_mono_t = capacity / 4;
        let max_expected_remaining_capacity: zx_duration_mono_t = capacity - spin_duration;
        let retry_duration: zx_duration_mono_t = period * 3;

        let fair_profile = FairProfile::create(TEST_DEFAULT_WEIGHT, InheritableProfile::Yes);
        let deadline_profile = DeadlineProfile::create(capacity, period);

        assert_true!(fair_profile.is_some());
        assert_true!(deadline_profile.is_some());
        let fair_profile = fair_profile.unwrap();
        let deadline_profile = deadline_profile.unwrap();

        let verify_fn =
            subtest!(|_active_thread: &TestThread, pressure_threads: &[TestThread; 1]| {
                // Verify that pressure_thread's parameters were updated.
                let remaining_time_slice = pressure_threads[0].remaining_time_slice();

                // If the active thread consumed approximately the expected capacity, the
                // remaining time slice should be greater than 0ns and less than the
                // expected remaining capacity (accounting for scheduling overhead).
                expect_gt!(remaining_time_slice.raw_value(), 0);
                expect_lt!(remaining_time_slice.raw_value(), max_expected_remaining_capacity);
            });

        assert_true!(pi_test_deadline_join_split_helper_uniform::<1, _>(
            &fair_profile,
            &deadline_profile,
            spin_duration,
            retry_duration,
            verify_fn,
        ));
    }

    /// deadline to deadline join and split
    #[test]
    fn pi_test_deadline_to_deadline_join_split() {
        // Tests bandwidth distribution when a deadline thread blocks as upstream node
        // behind another deadline target thread. This results in a deadline-to-deadline
        // PI graph join and a subsequent deadline-from-deadline PI graph split.
        let period: zx_duration_mono_t = zx_sec(1);
        let capacity: zx_duration_mono_t = period / 2;
        let spin_duration: zx_duration_mono_t = capacity;
        let max_expected_remaining_capacity: zx_duration_mono_t = (period - spin_duration) / 2;
        let retry_duration: zx_duration_mono_t = period * 3;

        let deadline_profile = DeadlineProfile::create(capacity, period);

        assert_true!(deadline_profile.is_some());
        let deadline_profile = deadline_profile.unwrap();

        let verify_fn =
            subtest!(|active_thread: &TestThread, pressure_threads: &[TestThread; 1]| {
                // Verify that the active and pressure thread's parameters were updated.
                let active_remaining_time_slice = active_thread.remaining_time_slice();
                let pressure_remaining_time_slice = pressure_threads[0].remaining_time_slice();

                // If the active thread consumed approximately the expected capacity, the
                // remaining time slice should be greater than 0ns and less than the
                // expected remaining capacity (accounting for scheduling overhead).
                expect_gt!(active_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    active_remaining_time_slice.raw_value(),
                    max_expected_remaining_capacity
                );
                expect_gt!(pressure_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    pressure_remaining_time_slice.raw_value(),
                    max_expected_remaining_capacity
                );
            });

        assert_true!(pi_test_deadline_join_split_helper_uniform::<1, _>(
            &deadline_profile,
            &deadline_profile,
            spin_duration,
            retry_duration,
            verify_fn,
        ));
    }

    /// deadline to deadline join and split uneven
    #[test]
    fn pi_test_deadline_to_deadline_join_split_uneven() {
        // Tests bandwidth distribution when a deadline thread blocks as upstream node
        // behind another deadline target thread. This results in a deadline-to-deadline
        // PI graph join and a subsequent deadline-from-deadline PI graph split, where the
        // active thread has a smaller capacity than the pressure thread.
        let period: zx_duration_mono_t = zx_sec(1);
        let active_capacity: zx_duration_mono_t = period / 4;
        let pressure_capacity: zx_duration_mono_t = period * 3 / 4;
        let spin_duration: zx_duration_mono_t = period / 2;
        let max_expected_active_remaining_capacity: zx_duration_mono_t =
            (period - spin_duration) / 4;
        let max_expected_pressure_remaining_capacity: zx_duration_mono_t =
            (period - spin_duration) * 3 / 4;
        let retry_duration: zx_duration_mono_t = period * 3;

        let active_profile = DeadlineProfile::create(active_capacity, period);
        let pressure_profile = DeadlineProfile::create(pressure_capacity, period);

        assert_true!(active_profile.is_some());
        assert_true!(pressure_profile.is_some());
        let active_profile = active_profile.unwrap();
        let pressure_profile = pressure_profile.unwrap();

        let verify_fn =
            subtest!(|active_thread: &TestThread, pressure_threads: &[TestThread; 1]| {
                // Verify that the active and pressure thread's parameters were updated.
                let active_remaining_time_slice = active_thread.remaining_time_slice();
                let pressure_remaining_time_slice = pressure_threads[0].remaining_time_slice();

                // If the active thread consumed approximately the expected capacity, the
                // remaining time slice should be greater than 0ns and less than the
                // expected remaining capacity (accounting for scheduling overhead).
                expect_gt!(active_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    active_remaining_time_slice.raw_value(),
                    max_expected_active_remaining_capacity
                );
                expect_gt!(pressure_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    pressure_remaining_time_slice.raw_value(),
                    max_expected_pressure_remaining_capacity
                );
            });

        assert_true!(pi_test_deadline_join_split_helper_uniform::<1, _>(
            &active_profile,
            &pressure_profile,
            spin_duration,
            retry_duration,
            verify_fn,
        ));
    }

    /// deadline to deadline join and split multiple waiters
    #[test]
    fn pi_test_deadline_to_deadline_join_split_multiple_waiters() {
        // Tests bandwidth distribution when multiple deadline threads block as upstream
        // nodes behind a deadline thread. This results in multiple deadline-to-deadline
        // PI graph joins and subsequent deadline-from-deadline PI graph splits, with
        // multiple threads blocking on the same waitqueue.
        let period: zx_duration_mono_t = zx_sec(1);
        let active_capacity: zx_duration_mono_t = period / 2;
        let pressure_capacity: zx_duration_mono_t = period / 4;
        let spin_duration: zx_duration_mono_t = period / 2;
        let max_expected_active_remaining_capacity: zx_duration_mono_t =
            (period - spin_duration) / 2;
        let max_expected_pressure_remaining_capacity: zx_duration_mono_t =
            (period - spin_duration) / 4;
        let retry_duration: zx_duration_mono_t = period * 3;

        let active_profile = DeadlineProfile::create(active_capacity, period);
        let pressure_profile = DeadlineProfile::create(pressure_capacity, period);

        assert_true!(active_profile.is_some());
        assert_true!(pressure_profile.is_some());
        let active_profile = active_profile.unwrap();
        let pressure_profile = pressure_profile.unwrap();

        let verify_fn =
            subtest!(|active_thread: &TestThread, pressure_threads: &[TestThread; 2]| {
                // Verify that the active and pressure thread's parameters were updated.
                let active_remaining_time_slice = active_thread.remaining_time_slice();
                let pressure1_remaining_time_slice = pressure_threads[0].remaining_time_slice();
                let pressure2_remaining_time_slice = pressure_threads[1].remaining_time_slice();

                // If the active thread consumed approximately the expected capacity, the
                // remaining time slice should be greater than 0ns and less than the
                // expected remaining capacity (accounting for scheduling overhead).
                expect_gt!(active_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    active_remaining_time_slice.raw_value(),
                    max_expected_active_remaining_capacity
                );
                expect_gt!(pressure1_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    pressure1_remaining_time_slice.raw_value(),
                    max_expected_pressure_remaining_capacity
                );
                expect_gt!(pressure2_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    pressure2_remaining_time_slice.raw_value(),
                    max_expected_pressure_remaining_capacity
                );
            });

        assert_true!(pi_test_deadline_join_split_helper_uniform::<2, _>(
            &active_profile,
            &pressure_profile,
            spin_duration,
            retry_duration,
            verify_fn,
        ));
    }

    /// deadline to deadline join and split multiple waiters different periods
    #[test]
    fn pi_test_deadline_to_deadline_join_split_multiple_waiters_different_periods() {
        // Tests bandwidth distribution and dynamic deadline parameter updates when
        // multiple deadline threads with different relative deadlines block on the same
        // waitqueue and are woken together (regression test for b/571995085).
        let short_period: zx_duration_mono_t = zx_sec(1);
        let long_period: zx_duration_mono_t = zx_sec(2);
        let active_capacity: zx_duration_mono_t = long_period / 2;
        let short_pressure_capacity: zx_duration_mono_t = short_period / 4;
        let long_pressure_capacity: zx_duration_mono_t = long_period / 4;
        let spin_duration: zx_duration_mono_t = short_period / 2;

        // While joined, the effective period is short_period and total utilization is
        // 50% (active) + 25% (short) + 25% (long) = 100%, so the combined remaining
        // time slice after spinning for spin_duration is at most
        // (short_period - spin_duration), distributed proportionally on split.
        let max_expected_active_remaining_capacity: zx_duration_mono_t =
            (short_period - spin_duration) / 2;
        let max_expected_pressure_remaining_capacity: zx_duration_mono_t =
            (short_period - spin_duration) / 4;
        let retry_duration: zx_duration_mono_t = long_period * 3;

        let active_profile = DeadlineProfile::create(active_capacity, long_period);
        let short_pressure_profile = DeadlineProfile::create(short_pressure_capacity, short_period);
        let long_pressure_profile = DeadlineProfile::create(long_pressure_capacity, long_period);

        assert_true!(active_profile.is_some());
        assert_true!(short_pressure_profile.is_some());
        assert_true!(long_pressure_profile.is_some());
        let active_profile = active_profile.unwrap();
        let short_pressure_profile = short_pressure_profile.unwrap();
        let long_pressure_profile = long_pressure_profile.unwrap();

        let pressure_profiles: [RefPtr<Profile>; 2] =
            [short_pressure_profile, long_pressure_profile];

        let verify_fn =
            subtest!(|active_thread: &TestThread, pressure_threads: &[TestThread; 2]| {
                let active_remaining_time_slice = active_thread.remaining_time_slice();
                let pressure1_remaining_time_slice = pressure_threads[0].remaining_time_slice();
                let pressure2_remaining_time_slice = pressure_threads[1].remaining_time_slice();
                let pressure2_snapshot = pressure_threads[1].snapshot();

                expect_gt!(active_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    active_remaining_time_slice.raw_value(),
                    max_expected_active_remaining_capacity
                );
                expect_gt!(pressure1_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    pressure1_remaining_time_slice.raw_value(),
                    max_expected_pressure_remaining_capacity
                );
                expect_gt!(pressure2_remaining_time_slice.raw_value(), 0);
                expect_lt!(
                    pressure2_remaining_time_slice.raw_value(),
                    max_expected_pressure_remaining_capacity
                );

                // The long waiter should leave the split with its own relative deadline and
                // capacity, not the short waiter's.
                expect_eq!(
                    pressure2_snapshot.finish_time.raw_value()
                        - pressure2_snapshot.start_time.raw_value(),
                    long_period
                );
                expect_eq!(pressure2_snapshot.time_slice_ns.raw_value(), long_pressure_capacity);
            });

        assert_true!(pi_test_deadline_join_split_helper::<2, _>(
            &active_profile,
            &pressure_profiles,
            spin_duration,
            retry_duration,
            verify_fn,
        ));
    }
}
