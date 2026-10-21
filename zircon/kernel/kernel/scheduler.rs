// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#![allow(unused_imports, unused_macros)]
use super::owned_wait_queue::{OwnedWaitQueue, ThreadUnblockList};
use super::percpu::PerCpu;
use super::relaxed_atomic::RelaxedAtomicI64;
use super::scheduler_state::{
    HIGHEST_PRIORITY, IDLE_PRIORITY, LOWEST_PRIORITY, ThreadStateKind as ThreadState,
};
use super::stats::CpuStats;
use super::thread::Thread;
use super::types::{
    AtomicCpuMask, CPU_MASK_ALL, INVALID_CPU, cpu_mask_t as CpuMask, cpu_num_t as CpuNum,
    cpu_num_to_mask, remove_cpu_from_mask,
};
use super::wait_queue::WaitQueueBase;
use crate::counters::define_kcounter;
use crate::kconcurrent::{
    Action, AllowFinalized, ChainLock, ChainLockGuard, ChainLockProof, ChainLockTransaction,
    ChainLockTransactionToken, DONE, IRQ_SAVE_OPTION, NO_IRQ_SAVE_OPTION, OwnerGuard,
    RecordBackoff, SavedState, TransactionResult, assert_transaction_active, clt_tag,
    single_chain_lock_guard,
};
use crate::ktrace_rs;
use crate::power_management::ControlInterface;
use crate::vm::vm_aspace::VmAspace;
use core::mem::MaybeUninit;
use core::pin::Pin;
use fbl::{
    Cursor, DefaultObjectTag, TrackingSize, WavlTree, WavlTreeAugmentedInvariantObserver,
    WavlTreeAugmentedInvariantObserverTraits, WavlTreeContainable, WavlTreeKeyable, WavlTreeNode,
};
use ffl::{FastFixed, Fixed};
use zx_status::Status;

struct ThreadName {
    buf: [u8; zx_types::ZX_MAX_NAME_LEN],
    len: usize,
}

impl ThreadName {
    #[inline]
    fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buf[..self.len]).unwrap_or("")
    }
}

/// Copies the thread's name into a stack-allocated buffer using volatile reads
/// so that concurrent calls to `Thread::set_name` (which does not synchronize
/// with scheduler locks) cannot cause a data race or out-of-bounds read.
///
/// # Safety
/// `thread` must be null or point to a valid `Thread`.
#[inline]
unsafe fn thread_name(thread: *const Thread) -> ThreadName {
    let mut res = ThreadName { buf: [0u8; zx_types::ZX_MAX_NAME_LEN], len: 0 };
    if thread.is_null() {
        return res;
    }
    // SAFETY: `thread` is non-null and valid per the caller's contract.
    let ptr = unsafe { super::thread::name(thread) }.cast::<u8>();
    if ptr.is_null() {
        return res;
    }
    for i in 0..zx_types::ZX_MAX_NAME_LEN {
        // SAFETY: `Thread::name()` points to an inline buffer of `ZX_MAX_NAME_LEN`
        // bytes inside `Thread`. Volatile byte reads avoid UB from concurrent
        // unsynchronized writes in `Thread::set_name`.
        let b = unsafe { ptr.add(i).read_volatile() };
        if b == 0 {
            break;
        }
        res.buf[i] = b;
        res.len = i + 1;
    }
    res
}

// Counts the number of times we set the preemption timer to fire at a point
// that's prior to the time at which the CPU last entered the scheduler.  See
// also the comment where this counter is used.
define_kcounter!(COUNTER_PREEMPT_PAST, "scheduler.preempt_past", Sum);

// Counts the number of times we dequeue a deadline thread whose finish_time is
// earlier than its eligible_time.
define_kcounter!(COUNTER_DEADLINE_PAST, "scheduler.deadline_past", Sum);

// Counts the number of times a thread's effective profile or affinity changed
// while transitioning from READY to RUNNING.
define_kcounter!(COUNTER_UPDATE_IN_TRANSITION, "scheduler.update_in_transition", Sum);

// Counts the number of times we had to run FindTargetCpu again because our
// selected Target became in-active after we chose it.
define_kcounter!(COUNTER_FIND_TARGET_CPU_RETRIES, "scheduler.find_target_cpu.retries", Sum);

// Counts the number of times the fair timeline was snapped forward to make a
// fair thread eligible to run.
define_kcounter!(COUNTER_FAIR_TIMELINE_SNAP_COUNT, "scheduler.fair-timeline-snap.count", Sum);

// Counts the amount of variable time the fair timeline was snapped forward to
// make a fair thread eligible to run.
define_kcounter!(COUNTER_FAIR_TIMELINE_SNAP_TIME, "scheduler.fair-timeline-snap.time", Sum);

// Counts the number of times a task wakes up normally and is placed on the last
// CPU it ran on.
define_kcounter!(COUNTER_WAKEUP_NORMAL_LOCAL, "scheduler.wakeup.normal-local", Sum);

// Counts the number of times a task wakes up normally and is placed on a
// different CPU than the last CPU it ran on.
define_kcounter!(COUNTER_WAKEUP_NORMAL_MIGRATED, "scheduler.wakeup.normal-migrated", Sum);

// Counts the number of times a task wakes up synchronously and is placed on the
// CPU the waker ran on.
define_kcounter!(COUNTER_WAKEUP_SYNCHRONOUS_LOCAL, "scheduler.wakeup.synchronous-local", Sum);

// Counts the number of times a task wakes up synchronously and is placed on a
// different CPU than the CPU the waker ran on.
define_kcounter!(COUNTER_WAKEUP_SYNCHRONOUS_MIGRATED, "scheduler.wakeup.synchronous-migrated", Sum);

#[inline]
fn counter_wakeup_add(is_sync: bool, is_local: bool) {
    match (is_sync, is_local) {
        (false, true) => COUNTER_WAKEUP_NORMAL_LOCAL.add(1),
        (false, false) => COUNTER_WAKEUP_NORMAL_MIGRATED.add(1),
        (true, true) => COUNTER_WAKEUP_SYNCHRONOUS_LOCAL.add(1),
        (true, false) => COUNTER_WAKEUP_SYNCHRONOUS_MIGRATED.add(1),
    }
}

ktrace_rs::declare_interned_string!(PLACEMENT_INSERTION, "Insertion");
ktrace_rs::declare_interned_string!(PLACEMENT_ADJUSTMENT, "Adjustment");
ktrace_rs::declare_interned_string!(PLACEMENT_PREEMPTION, "Preemption");
ktrace_rs::declare_interned_string!(PLACEMENT_MIGRATION, "Migration");
ktrace_rs::declare_interned_string!(PLACEMENT_ASSOCIATION, "Association");

ktrace_rs::declare_interned_string!(PREEMPT_IRQ, "irq");
ktrace_rs::declare_interned_string!(PREEMPT_FLUSH_PENDING, "flush-pending");
ktrace_rs::declare_interned_string!(PREEMPT_RESCHEDULE, "reschedule");

const SCHEDULER_TRACING_LEVEL: u32 = if cfg!(scheduler_tracing_level = "1") {
    1
} else if cfg!(scheduler_tracing_level = "2") {
    2
} else if cfg!(scheduler_tracing_level = "3") {
    3
} else if cfg!(scheduler_tracing_level = "4") {
    4
} else if cfg!(scheduler_tracing_level = "5") {
    5
} else if cfg!(scheduler_tracing_level = "6") {
    6
} else {
    0
};

const SCHEDULER_QUEUE_TRACING_ENABLED: bool = cfg!(scheduler_queue_tracing_enabled);

const LOCAL_KTRACE_LEVEL: u32 = if SCHEDULER_TRACING_LEVEL == 0 && SCHEDULER_QUEUE_TRACING_ENABLED {
    1 // LEVEL_COMMON
} else {
    SCHEDULER_TRACING_LEVEL
};

macro_rules! level_val {
    (STANDARD) => {
        0
    };
    (COMMON) => {
        1
    };
    (BANDWIDTH) => {
        2
    };
    (QUEUE) => {
        3
    };
    (FLOW) => {
        4
    };
    (COUNTER) => {
        5
    };
    (DETAILED) => {
        6
    };
}

macro_rules! local_ktrace_level_enabled {
    ($level:ident) => {
        LOCAL_KTRACE_LEVEL >= level_val!($level)
    };
}

macro_rules! local_ktrace_instant {
    ($level:ident, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        if local_ktrace_level_enabled!($level) {
            ktrace_rs::cpu_instant!("kernel:sched", $label $(, $key => $val)*);
        }
    };
}

macro_rules! local_ktrace_begin_scope {
    ($level:ident, $label:tt $(, $key:tt => $val:expr)* $(,)?) => {
        ktrace_rs::cpu_begin_scope_cond!(
            local_ktrace_level_enabled!($level),
            "kernel:sched",
            $label
            $(, $key => $val)*
        )
    };
}

macro_rules! local_ktrace_flow_begin {
    ($level:ident, $label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        if local_ktrace_level_enabled!($level) {
            ktrace_rs::cpu_flow_begin!("kernel:sched", $label, $flow_id $(, $key => $val)*);
        }
    };
}

macro_rules! local_ktrace_flow_step {
    ($level:ident, $label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        if local_ktrace_level_enabled!($level) {
            ktrace_rs::cpu_flow_step!("kernel:sched", $label, $flow_id $(, $key => $val)*);
        }
    };
}

#[allow(unused_macros)]
macro_rules! local_ktrace_flow_end {
    ($level:ident, $label:tt, $flow_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        if local_ktrace_level_enabled!($level) {
            ktrace_rs::cpu_flow_end!("kernel:sched", $label, $flow_id $(, $key => $val)*);
        }
    };
}

#[allow(unused_macros)]
macro_rules! local_ktrace_counter {
    ($level:ident, $label:tt, $counter_id:expr $(, $key:tt => $val:expr)* $(,)?) => {
        if local_ktrace_level_enabled!($level) {
            ktrace_rs::cpu_counter!("kernel:sched", $label, $counter_id $(, $key => $val)*);
        }
    };
}

macro_rules! local_ktrace_counter_timestamp {
    (
        $level:ident,
        $label:tt,
        $timestamp:expr,
        $counter_id:expr
        $(, $key:tt => $val:expr)* $(,)?
    ) => {
        if local_ktrace_level_enabled!($level) {
            ktrace_rs::cpu_counter_timestamp!(
                "kernel:sched",
                $label,
                $timestamp,
                $counter_id
                $(, $key => $val)*
            );
        }
    };
}

/// Fixed-point task weight.
///
/// The 10-bit fractional component accommodates the exponential curve defining
/// the priority-to-weight relation:
///
///      Weight = 1.1^(Priority - 31)
///
/// This yields roughly a 4-5% bandwidth difference between adjacent priorities.
///
/// Weights should not be negative, however, the value is signed for consistency
/// with `zx_instant_mono_t` (`SchedTime`) and `zx_duration_mono_t` (`SchedDuration`),
/// which are the primary types used in conjunction with `SchedWeight`. This is to
/// make it less likely that expressions involving weights are accidentally
/// promoted to unsigned.
pub type SchedWeight = FastFixed<i64, 10>;

/// Fixed-point utilization factor. Represents the ratio between capacity and
/// period or capacity and relative deadline, depending on which type of
/// utilization is being evaluated.
///
/// The 31-bit fractional component represents the utilization with a precision
/// of ~0.5ns.
pub type SchedUtilization = FastFixed<i64, 31>;

/// Fixed-point types wrapping time and duration types to make time expressions
/// cleaner in the scheduler code.
pub type SchedDuration = FastFixed<zx_types::zx_duration_mono_t, 0>;
pub type SchedTime = FastFixed<zx_types::zx_instant_mono_t, 0>;

/// A smaller duration type that can represent up to ~2.15 seconds. This is
/// longer than any useful deadline scheduling period.
// TODO(eieio): Use this type more extensively for profile parameter values to
// save space.
pub type SchedCompactDuration = FastFixed<i32, 0>;

/// The processing rate of a CPU relative to the fastest CPU in the system.
pub type SchedProcessingRate = FastFixed<i64, 31>;

// Simplify trace event arguments by automatically converting fixed-point values with zero
// fractional bits to the corresponding integral records.
impl<'a> From<FastFixed<i64, 0>> for ktrace_rs::ArgValue<'a> {
    fn from(v: FastFixed<i64, 0>) -> Self {
        ktrace_rs::ArgValue::Int64(v.raw_value())
    }
}

impl<'a> From<FastFixed<u64, 0>> for ktrace_rs::ArgValue<'a> {
    fn from(v: FastFixed<u64, 0>) -> Self {
        ktrace_rs::ArgValue::Uint64(v.raw_value())
    }
}

impl<'a> From<FastFixed<i32, 0>> for ktrace_rs::ArgValue<'a> {
    fn from(v: FastFixed<i32, 0>) -> Self {
        ktrace_rs::ArgValue::Int32(v.raw_value())
    }
}

impl<'a> From<FastFixed<u32, 0>> for ktrace_rs::ArgValue<'a> {
    fn from(v: FastFixed<u32, 0>) -> Self {
        ktrace_rs::ArgValue::Uint32(v.raw_value())
    }
}

// Until the FXT format supports fixed-point argument values, scale utilization
// values by 1000 to improve the interpretability in trace event arguments. Use
// i32 for the more compact encoding, since visualized utilization values do
// not need high precision and will not typically have large integral
// components.
impl<'a> From<FastFixed<i64, 31>> for ktrace_rs::ArgValue<'a> {
    fn from(v: FastFixed<i64, 31>) -> Self {
        let scaled: Fixed<i32, 0> = v.into_fixed().mul_to(Fixed::<i64, 0>::from_raw(1000));
        ktrace_rs::ArgValue::Int32(scaled.raw_value())
    }
}

// Utilities that return fixed-point `SchedDuration` values representing the given
// integer time units in terms of system time units (nanoseconds).
#[inline]
pub const fn sched_ns(nanoseconds: zx_types::zx_duration_mono_t) -> SchedDuration {
    SchedDuration::from_raw(nanoseconds)
}

#[inline]
pub const fn sched_us(microseconds: zx_types::zx_duration_mono_t) -> SchedDuration {
    SchedDuration::from_raw(microseconds * 1_000)
}

#[inline]
pub const fn sched_ms(milliseconds: zx_types::zx_duration_mono_t) -> SchedDuration {
    SchedDuration::from_raw(milliseconds * 1_000_000)
}

#[inline]
pub const fn sched_s(seconds: zx_types::zx_duration_mono_t) -> SchedDuration {
    SchedDuration::from_raw(seconds * 1_000_000_000)
}

/// The maximum deadline utilization permitted for a single thread. This limit
/// is applied when scaling the utilization of a deadline task to the relative
/// performance of a candidate target processor -- placing a task on a
/// processor that would cause the scaled thread utilization to exceed this
/// value is avoided if possible.
pub const K_THREAD_UTILIZATION_MAX: SchedUtilization = SchedUtilization::from_raw(1 << 31);

/// The per-CPU deadline utilization limit to attempt to honor when selecting a
/// CPU to place a task. It is up to userspace to ensure that the total set of
/// deadline tasks can honor this limit. Even if userspace ensures the total
/// set of deadline utilizations is within the total available processor
/// resources, when total utilization is high enough it may not be possible to
/// honor this limit due to the bin packing problem.
pub const K_CPU_UTILIZATION_LIMIT: SchedUtilization = SchedUtilization::from_raw(1 << 31);
pub const ZX_TIME_INFINITE: zx_types::zx_instant_mono_t = zx_types::zx_instant_mono_t::MAX;

/// The default period for fair tasks when period expansion is not in effect.
pub const K_DEFAULT_FAIR_PERIOD: SchedDuration = sched_ms(10);

/// The minimum fair utilization before period expansion is needed.
/// Matches C++ `SchedUtilization{kMinimumFairCapacity} / kDefaultFairPeriod` (truncated).
pub const K_MIN_FAIR_UTILIZATION: SchedUtilization = SchedUtilization::from_raw(214_748_364);

/// The adjustment rates of the exponential moving averages tracking the
/// expected runtimes of each thread.
pub const K_EXPECTED_RUNTIME_ALPHA: FastFixed<i64, 2> = FastFixed::from_raw(1);
pub const K_EXPECTED_RUNTIME_BETA: FastFixed<i64, 0> = FastFixed::from_raw(1);

/// Minimum fair time slice. Set to avoid excessive reschedule overhead.
// TODO(eieio): This needs to be revised and/or scaled for lower OPPs.
pub const K_MINIMUM_FAIR_CAPACITY: SchedDuration = sched_ms(1); // 1ms

/// Table of fixed-point constants converting from kernel priority to fair
/// scheduler weight. Scales the integer value to a fixed-point weight in the
/// interval (0.0, 1.0].
const K_PRIORITY_TO_WEIGHT_TABLE: [SchedWeight; 32] = [
    SchedWeight::from_raw(53),
    SchedWeight::from_raw(58),
    SchedWeight::from_raw(64),
    SchedWeight::from_raw(71),
    SchedWeight::from_raw(78),
    SchedWeight::from_raw(85),
    SchedWeight::from_raw(94),
    SchedWeight::from_raw(103),
    SchedWeight::from_raw(114),
    SchedWeight::from_raw(125),
    SchedWeight::from_raw(138),
    SchedWeight::from_raw(152),
    SchedWeight::from_raw(167),
    SchedWeight::from_raw(184),
    SchedWeight::from_raw(202),
    SchedWeight::from_raw(222),
    SchedWeight::from_raw(245),
    SchedWeight::from_raw(269),
    SchedWeight::from_raw(296),
    SchedWeight::from_raw(326),
    SchedWeight::from_raw(358),
    SchedWeight::from_raw(394),
    SchedWeight::from_raw(434),
    SchedWeight::from_raw(477),
    SchedWeight::from_raw(525),
    SchedWeight::from_raw(578),
    SchedWeight::from_raw(635),
    SchedWeight::from_raw(699),
    SchedWeight::from_raw(769),
    SchedWeight::from_raw(846),
    SchedWeight::from_raw(930),
    SchedWeight::from_raw(1024),
];

/// A mask of all of the currently active schedulers in the system. An
/// "active" scheduler is one that will accept threads to be executed. An
/// "inactive" scheduler will not, and is typically the scheduler for a CPU
/// which is in the process of being taken offline.
///
/// Note that to change the active state of a scheduler for CPU X (e.g., to
/// toggle the state of bit X in the `ACTIVE_SCHEDULERS` mask) that scheduler's
/// `queue_lock` must be held. Therefore, observations of the schedulers'
/// active mask must typically be assumed to be volatile. As soon as the mask
/// is observed, it can immediately change.
static ACTIVE_SCHEDULERS: AtomicCpuMask = AtomicCpuMask::new(0);

/// A mask of all of the currently idle schedulers in the system. An
/// "idle" scheduler is one which last selected the idle/power thread to run,
/// while a "busy" scheduler is one which is running a non-idle thread.
///
/// Like the active mask, the idle mask is volatile and subject to change at
/// any time barring special circumstances.
static IDLE_SCHEDULERS: AtomicCpuMask = AtomicCpuMask::new(0);

/// Peek at the current active mask for all schedulers.
///
/// Note that this is just a lockless atomic load. Barring special
/// circumstances, the set of active schedulers can change at any time.
#[inline]
pub fn peek_active_mask() -> CpuMask {
    ACTIVE_SCHEDULERS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Lock class for `Scheduler::queue_lock_`.
///
/// `Scheduler::queue_lock_` is constructed in C++ and only accessed via a
/// `&KMonitoredSpinlock` facade, so `LockClass::ID` is never used to construct a Rust spinlock.
pub enum SchedulerQueueLockClass {}

impl ksync::LockClass for SchedulerQueueLockClass {
    const ID: *mut core::ffi::c_void = core::ptr::null_mut();
}

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Updates the active power level for `domain_id` in the kernel scheduler.
    ///
    /// # Safety
    ///
    /// `out_mask` must be a valid, non-null, properly aligned writable pointer to a [`CpuMask`]
    /// for the duration of this call.
    fn cpp_scheduler_update_power_level(
        domain_id: u32,
        controller_id: u64,
        interface: ControlInterface,
        arg: u64,
        out_mask: *mut CpuMask,
    ) -> Result<(), Status>;
    fn cpp_scheduler_dump(scheduler: *mut Scheduler, queue_state_only: bool);
    fn cpp_scheduler_get_queue_lock(
        scheduler: *const Scheduler,
    ) -> *const ksync::KMonitoredSpinlock<SchedulerQueueLockClass>;
}

/// Peek at the current active mask state for a given CPU's scheduler.
///
/// Note that this is just a lockless atomic load. Barring special
/// circumstances, the set of active schedulers can change at any time.
#[inline]
pub fn peek_is_active(cpu: CpuNum) -> bool {
    (peek_active_mask() & cpu_num_to_mask(cpu)) != 0
}

/// Accessors for the "idle" state mask; similar to the active state mask.
#[inline]
pub fn peek_idle_mask() -> CpuMask {
    IDLE_SCHEDULERS.load(core::sync::atomic::Ordering::Relaxed)
}

/// Returns true if the given CPU's scheduler is currently marked idle; similar
/// to the active state mask.
#[inline]
pub fn peek_is_idle(cpu: CpuNum) -> bool {
    (peek_idle_mask() & cpu_num_to_mask(cpu)) != 0
}

/// Returns true if new scheduler wakeup accounting is enabled either by boot
/// option or gn arg.
#[inline]
pub fn enable_new_wakeup_accounting() -> bool {
    if cfg!(experimental_force_new_wakeup_accounting) {
        true
    } else {
        boot_options::BootOptions::get().enable_new_wakeup_accounting
    }
}

/// Returns the CPU stats for the given CPU number, forward projected to account for idle or
/// normalized busy time elapsed since the last relevant state change.
pub fn get_projected_cpu_stats(cpu_num: CpuNum) -> CpuStats {
    assert!((cpu_num as usize) < PerCpu::processor_count());
    let mut stats = MaybeUninit::uninit();
    // SAFETY: `cpu_num` is bounds-checked to be in `0..PerCpu::processor_count()`, `queue_lock_`
    // is not held by this (safe, public) entry point, and `stats.as_mut_ptr()` is a valid stack
    // pointer to uninitialized storage that `project_cpu_stats` fully initializes.
    unsafe {
        Scheduler::project_cpu_stats(cpu_num, stats.as_mut_ptr());
        stats.assume_init()
    }
}

/// Updates the active power level for the power domain identified by `domain_id`.
pub fn update_power_level(
    domain_id: u32,
    controller_id: u64,
    interface: ControlInterface,
    arg: u64,
) -> Result<CpuMask, Status> {
    let mut mask = 0;
    // SAFETY: `mask` is a valid writable `CpuMask` on the stack.
    unsafe {
        cpp_scheduler_update_power_level(domain_id, controller_id, interface, arg, &mut mask)
    }?;
    Ok(mask)
}

/// Indicates whether the CPU stats being updated is for a busy or idle time
/// segment.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CpuStatsSegmentType {
    Busy = 0,
    Idle = 1,
}

/// Specifies how to associate a thread with a `Scheduler` instance, update
/// metadata, and whether/where to place the thread in a run queue.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// Selects a place in the queue based on the current insertion time and
    /// thread weight or deadline.
    Insertion = 0,

    /// Selects a place in the queue based on the original insertion time and
    /// the updated (inherited or changed) weight or deadline on the same CPU.
    Adjustment = 1,

    /// Selects a place in the queue based on the original insertion time and
    /// the updated time slice due to being preempted by another thread.
    Preemption = 2,

    /// Selects a place in the queue based on the insertion time in the original
    /// queue adjusted for the new queue.
    Migration = 3,

    /// Updates the metadata to account for a stolen thread that was just taken
    /// from a different queue. This is distinct from `Migration` in that the
    /// thread is not also enqueued, it is run immediately by the new CPU.
    Association = 4,
}

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvaluateStatus {
    Selected = 0,
    Idle = 1,
    StealWork = 2,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RustRescheduleResult {
    pub need_context_switch: bool,
    pub timeslice_expired: bool,
    pub current_is_migrating: bool,
    pub next_thread: *mut Thread,
}

/// Fixed-point type of the `Affine` slope.
pub type Slope = FastFixed<i64, { Affine::K_PRECISION_BITS }>;

/// `Affine` performs simple affine transformations between monotonic and variable timelines for
/// variable bandwidth scheduling.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct Affine {
    pub monotonic_reference_time: SchedTime,
    pub variable_reference_time: SchedTime,
    pub slope: Slope,
}

impl Default for Affine {
    fn default() -> Self {
        Self {
            monotonic_reference_time: SchedTime::from_raw(0),
            variable_reference_time: SchedTime::from_raw(0),
            slope: Self::K_MAX_SLOPE,
        }
    }
}

impl Affine {
    /// Number of fractional bits in `slope`. The C++ `kPrecision` is the
    /// corresponding power of two, `1 << K_PRECISION_BITS`.
    pub const K_PRECISION_BITS: usize = 10;
    pub const K_MIN_SLOPE: Slope = Slope::from_raw(1);
    pub const K_MAX_SLOPE: Slope = Slope::from_raw(1 << Self::K_PRECISION_BITS);

    pub fn monotonic_to_variable(&self, monotonic: SchedTime) -> SchedTime {
        self.multiply(monotonic - self.monotonic_reference_time) + self.variable_reference_time
    }

    pub fn variable_to_monotonic(&self, variable: SchedTime) -> SchedTime {
        self.divide(variable - self.variable_reference_time) + self.monotonic_reference_time
    }

    pub fn monotonic_to_variable_in_place(&self, val: &mut SchedTime) {
        *val = self.monotonic_to_variable(*val);
    }

    pub fn variable_to_monotonic_in_place(&self, val: &mut SchedTime) {
        *val = self.variable_to_monotonic(*val);
    }

    pub fn change_slope_at_monotonic_time(&mut self, monotonic: SchedTime, slope: Slope) {
        debug_assert!(slope >= Self::K_MIN_SLOPE && slope <= Self::K_MAX_SLOPE);
        self.variable_reference_time = self.monotonic_to_variable(monotonic);
        self.monotonic_reference_time = monotonic;
        self.slope = slope;
    }

    pub fn snap(&mut self, monotonic: SchedTime, variable: SchedTime) {
        self.monotonic_reference_time = core::cmp::max(monotonic, self.monotonic_reference_time);
        self.variable_reference_time = core::cmp::max(variable, self.variable_reference_time);
    }

    fn multiply(&self, delta: SchedTime) -> SchedTime {
        if self.slope != Self::K_MAX_SLOPE {
            let shifted = delta.raw_value() / (1 << Self::K_PRECISION_BITS);
            // Saturate like the `ffl` multiplication used by the C++ implementation.
            let scaled = shifted.saturating_mul(self.slope.raw_value());
            return SchedTime::from_raw(scaled);
        }
        delta
    }

    fn divide(&self, delta: SchedTime) -> SchedTime {
        if self.slope != Self::K_MAX_SLOPE {
            let scaled = delta.raw_value() / self.slope.raw_value();
            // Saturate like the `ffl` multiplication used by the C++ implementation.
            let shifted = scaled.saturating_mul(1 << Self::K_PRECISION_BITS);
            return SchedTime::from_raw(shifted);
        }
        delta
    }
}

/// Represents the key deadline scheduler parameters using fixed-point types.
/// This is a fixed-point version of the ABI type `zx_sched_deadline_params_t` that
/// makes expressions in the scheduler logic less verbose.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SchedDeadlineParams {
    pub capacity_ns: SchedDuration,
    pub deadline_ns: SchedDuration,
    pub utilization: SchedUtilization,
}

/// Specifies the type of scheduling algorithm applied to a thread.
#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SchedDiscipline {
    Fair = 0,
    Deadline = 1,
}

#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ForceInheritance(pub bool);

mod base_profile {
    use super::{SchedDeadlineParams, SchedDiscipline, SchedWeight, SchedulerState};

    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FairProfile {
        pub weight: SchedWeight,
    }

    #[repr(C)]
    #[derive(Clone, Copy)]
    pub union BaseProfileUnion {
        pub fair: FairProfile,
        pub deadline: SchedDeadlineParams,
    }

    /// Specifies the base scheduling profile of a thread.
    ///
    /// # Safety Invariant
    ///
    /// - When `self.discipline == SchedDiscipline::Fair`, the active variant of `self.u` is `fair`.
    /// - When `self.discipline == SchedDiscipline::Deadline`, the active variant of `self.u` is
    ///   `deadline`.
    #[repr(C)]
    pub struct BaseProfile {
        discipline: SchedDiscipline,
        pub inheritable: bool,
        pub critical: bool,
        u: BaseProfileUnion,
    }

    // We need to implement Clone and Copy for BaseProfile manually because of the union.
    impl Clone for BaseProfile {
        fn clone(&self) -> Self {
            *self
        }
    }
    impl Copy for BaseProfile {}

    impl BaseProfile {
        pub fn new_fair(priority: i32) -> Self {
            Self::new_fair_with_weight(SchedulerState::convert_priority_to_weight(priority), true)
        }

        pub const fn new_fair_with_weight(weight: SchedWeight, inheritable: bool) -> Self {
            Self {
                discipline: SchedDiscipline::Fair,
                inheritable,
                critical: false,
                u: BaseProfileUnion { fair: FairProfile { weight } },
            }
        }

        pub const fn new_deadline(deadline: SchedDeadlineParams, critical: bool) -> Self {
            Self {
                discipline: SchedDiscipline::Deadline,
                inheritable: true,
                critical,
                u: BaseProfileUnion { deadline },
            }
        }

        #[inline]
        pub const fn discipline(&self) -> SchedDiscipline {
            self.discipline
        }

        #[inline]
        pub fn is_deadline(&self) -> bool {
            self.discipline == SchedDiscipline::Deadline
        }

        #[inline]
        pub fn is_fair(&self) -> bool {
            self.discipline == SchedDiscipline::Fair
        }

        #[inline]
        pub fn fair_weight(&self) -> SchedWeight {
            debug_assert!(self.is_fair());
            // SAFETY: The type invariant guarantees `u.fair` is the active variant when
            // `self.is_fair()`.
            unsafe { self.u.fair.weight }
        }

        #[inline]
        pub fn set_fair_weight(&mut self, weight: SchedWeight) {
            self.discipline = SchedDiscipline::Fair;
            self.u = BaseProfileUnion { fair: FairProfile { weight } };
        }

        #[inline]
        pub fn deadline(&self) -> SchedDeadlineParams {
            debug_assert!(self.is_deadline());
            // SAFETY: The type invariant guarantees `u.deadline` is the active variant when
            // `self.is_deadline()`.
            unsafe { self.u.deadline }
        }

        #[inline]
        pub fn set_deadline(&mut self, deadline: SchedDeadlineParams) {
            self.discipline = SchedDiscipline::Deadline;
            self.u = BaseProfileUnion { deadline };
        }
    }

    // BaseProfile layout assertions
    zr::static_assert!(core::mem::size_of::<BaseProfile>() == 32);
    zr::static_assert!(core::mem::align_of::<BaseProfile>() == 8);
    zr::static_assert!(core::mem::offset_of!(BaseProfile, discipline) == 0);
    zr::static_assert!(core::mem::offset_of!(BaseProfile, inheritable) == 4);
    zr::static_assert!(core::mem::offset_of!(BaseProfile, critical) == 5);
    zr::static_assert!(core::mem::offset_of!(BaseProfile, u) == 8);
}

pub use base_profile::{BaseProfile, BaseProfileUnion, FairProfile};

/// Values stored in the `SchedulerState` of `Thread` instances which track the
/// aggregate profile values inherited from upstream contributors.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InheritedProfileValues {
    /// Inherited from fair threads.
    pub total_weight: SchedWeight,
    /// Inherited from deadline threads.
    pub uncapped_utilization: SchedUtilization,
    pub min_deadline: SchedDuration,
    pub critical_count: i32,
}

impl InheritedProfileValues {
    /// Returns `true` if these values carry either some fair weight, some deadline utilization,
    /// or both.
    #[inline]
    pub fn is_consequential(&self) -> bool {
        self.total_weight != SchedWeight::from_raw(0)
            || self.uncapped_utilization != SchedUtilization::from_raw(0)
    }

    pub fn assert_consistency(&self) {
        debug_assert!(self.total_weight >= SchedWeight::from_raw(0));
        debug_assert!(self.uncapped_utilization >= SchedUtilization::from_raw(0));
        debug_assert!(self.min_deadline > SchedDuration::from_raw(0));
        debug_assert!(
            self.uncapped_utilization == SchedUtilization::from_raw(0)
                || self.min_deadline < SchedDuration::from_raw(i64::MAX)
        );
    }
}

impl Default for InheritedProfileValues {
    #[inline]
    fn default() -> Self {
        Self {
            total_weight: SchedWeight::from_raw(0),
            uncapped_utilization: SchedUtilization::from_raw(0),
            min_deadline: SchedDuration::from_raw(i64::MAX),
            critical_count: 0,
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct EffectiveProfile {
    /// Internal weight values greater than or equal to zero indicate a fair
    /// profile with that weight. The internal deadline parameter values are
    /// arbitrary and reading them is not allowed.
    ///
    /// Negative weight values indicate a deadline profile; see
    /// `K_MAX_DEADLINE_WEIGHT` and `K_MAX_DIAGNOSTIC_WEIGHT`.
    pub weight: SchedWeight,
    pub capacity_ns: SchedCompactDuration,
    pub deadline_ns: SchedCompactDuration,
    pub utilization: SchedUtilization,
    pub critical: bool,
    #[cfg(scheduler_extra_invariant_validation)]
    pub dirty_flags: u32,
}

impl EffectiveProfile {
    pub const DIRTY_FLAG_CLEAN: u32 = 0;
    pub const DIRTY_FLAG_BASE_DIRTY: u32 = 1 << 0;
    pub const DIRTY_FLAG_INHERITED_DIRTY: u32 = 1 << 1;

    pub fn discipline(&self) -> SchedDiscipline {
        if self.weight.raw_value() >= 0 { SchedDiscipline::Fair } else { SchedDiscipline::Deadline }
    }

    /// Returns the discipline of the effective profile at `this` without forming a
    /// reference to it, for use when only the owning scheduler's `queue_lock_` is
    /// held and other fields (such as the dirty flags) may be concurrently written
    /// by a CPU holding the thread's lock.
    ///
    /// # Safety
    /// `this` must point to a valid `EffectiveProfile` whose `weight` is stable
    /// for the duration of the call.
    pub unsafe fn discipline_raw(this: *const Self) -> SchedDiscipline {
        // SAFETY: Reading the `weight` field through the raw pointer per the
        // caller's contract does not create a reference to the whole profile.
        if unsafe { (*this).weight }.raw_value() >= 0 {
            SchedDiscipline::Fair
        } else {
            SchedDiscipline::Deadline
        }
    }

    /// Returns the deadline parameters of the effective profile at `this` without
    /// forming a reference to it, for use when only the owning scheduler's
    /// `queue_lock_` is held and other fields (such as the dirty flags) may be
    /// concurrently written by a CPU holding the thread's lock.
    ///
    /// # Safety
    /// `this` must point to a valid deadline `EffectiveProfile` whose deadline
    /// fields are stable for the duration of the call.
    pub unsafe fn deadline_raw(this: *const Self) -> SchedDeadlineParams {
        // SAFETY: Reading the deadline fields through the raw pointer per the
        // caller's contract does not create a reference to the whole profile.
        unsafe {
            debug_assert!(Self::discipline_raw(this) == SchedDiscipline::Deadline);
            let cap_fixed = Fixed::<i64, 0>::from_fixed((*this).capacity_ns.into_fixed());
            let dead_fixed = Fixed::<i64, 0>::from_fixed((*this).deadline_ns.into_fixed());
            SchedDeadlineParams {
                capacity_ns: SchedDuration::from(cap_fixed),
                deadline_ns: SchedDuration::from(dead_fixed),
                utilization: (*this).utilization,
            }
        }
    }

    pub fn is_fair(&self) -> bool {
        self.discipline() == SchedDiscipline::Fair
    }

    pub fn is_deadline(&self) -> bool {
        !self.is_fair()
    }

    pub fn weight_or(&self, alternative: SchedWeight) -> SchedWeight {
        if self.is_fair() { self.weight } else { alternative }
    }

    pub fn deadline(&self) -> SchedDeadlineParams {
        debug_assert!(self.is_deadline());
        let cap_fixed = Fixed::<i64, 0>::from_fixed(self.capacity_ns.into_fixed());
        let dead_fixed = Fixed::<i64, 0>::from_fixed(self.deadline_ns.into_fixed());
        SchedDeadlineParams {
            capacity_ns: SchedDuration::from(cap_fixed),
            deadline_ns: SchedDuration::from(dead_fixed),
            utilization: self.utilization,
        }
    }

    #[inline]
    pub fn mark_base_profile_changed(&mut self) {
        #[cfg(scheduler_extra_invariant_validation)]
        {
            self.dirty_flags |= Self::DIRTY_FLAG_BASE_DIRTY;
        }
    }

    #[inline]
    pub fn mark_inherited_profile_changed(&mut self) {
        #[cfg(scheduler_extra_invariant_validation)]
        {
            self.dirty_flags |= Self::DIRTY_FLAG_INHERITED_DIRTY;
        }
    }

    #[inline]
    pub fn clean(&mut self) {
        #[cfg(scheduler_extra_invariant_validation)]
        {
            self.dirty_flags = Self::DIRTY_FLAG_CLEAN;
        }
    }

    #[inline]
    pub fn assert_dirty_state(&self, expected: u32) {
        #[cfg(scheduler_extra_invariant_validation)]
        {
            assert!(
                self.dirty_flags == expected,
                "EffectiveProfile::dirty_flags ({:#x}) != expected ({:#x})",
                self.dirty_flags,
                expected
            );
        }
        #[cfg(not(scheduler_extra_invariant_validation))]
        let _ = expected;
    }

    #[inline]
    pub fn assert_dirty(&self) {
        #[cfg(scheduler_extra_invariant_validation)]
        {
            assert!(
                self.dirty_flags != Self::DIRTY_FLAG_CLEAN,
                "EffectiveProfile::dirty_flags is clean when expected dirty"
            );
        }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SubtreeInvariants {
    /// Minimum finish time of all the descendants of this node in the run queue.
    /// Used to perform a partition search in O(log n) time, to find the thread
    /// with the earliest finish time that also has an eligible start time.
    pub min_finish_time: SchedTime,

    /// Minimum weight of all the descendants of this node in the run queue. Used
    /// to determine when period expansion is required to ensure that time slices
    /// are not too small.
    pub min_weight: SchedWeight,
}

/// Per-thread state used by the unified version of `Scheduler`.
///
/// Locking: the queue-related fields (`start_time`, `finish_time`,
/// `time_slice_ns`, `time_slice_used_ns`, `activation_count`, the run queue
/// node, and `subtree_invariants`) are guarded by the owning scheduler's
/// `queue_lock_`, while `base_profile`, `inherited_profile_values`, and the
/// `effective_profile` dirty flags are written under the thread's lock alone.
/// Forming a `&SchedulerState` over a thread in a run queue with only
/// `queue_lock_` held is the accepted idiom in this file for reading the
/// queue-guarded fields and the stable `effective_profile` parameters, as the
/// C++ `MarkHasOwnedThreadAccess` idiom permits: no field read through such a
/// reference can be concurrently written. Use `EffectiveProfile::discipline_raw`
/// where the extra invariant validation fields may be written concurrently.
#[repr(C)]
pub struct SchedulerState {
    /// The start time of the thread's current bandwidth request. This is the
    /// virtual start time for fair tasks and the period start for deadline tasks.
    pub start_time: SchedTime,

    /// The finish time of the thread's current bandwidth request. This is the
    /// virtual finish time for fair tasks and the absolute deadline for deadline
    /// tasks.
    pub finish_time: SchedTime,

    /// Counts how many times this thread has been activated since it was created.
    pub activation_count: u64,

    /// Subtree invariants for the augmented binary search tree implemented by the
    /// run queue WAVLTrees. The WAVLTree observer hooks maintain these per-node
    /// values that describe properties of the node's subtree when the node is in
    /// the tree.
    pub subtree_invariants: SubtreeInvariants,

    /// The scheduling state of the thread.
    pub state: ThreadState,

    pub base_profile: BaseProfile,
    pub inherited_profile_values: InheritedProfileValues,
    pub effective_profile: EffectiveProfile,

    /// The current timeslice allocated to the thread.
    pub time_slice_ns: SchedDuration,

    /// The runtime used in the current period.
    pub time_slice_used_ns: SchedDuration,

    /// The total time in `ThreadState::Running` state. If the thread is currently in
    /// `ThreadState::Running` state, this excludes the time accrued since it last left the
    /// scheduler.
    pub runtime_ns: SchedDuration,

    /// Tracks the exponential moving average of the runtime of the thread.
    pub expected_runtime_ns: SchedDuration,

    /// Tracks runtime accumulated until voluntarily blocking or exhausting the
    /// allocated time slice. Used to exclude involuntary preemption when updating
    /// the expected runtime estimate to improve accuracy.
    pub banked_runtime_ns: SchedDuration,

    /// Tracks the accumulated energy consumption of the thread, as estimated by
    /// the processor energy model. This counter can accumulate ~580 watt years
    /// (e.g. 1W continuously for ~580 years, 10W continuously for ~58 years, ...)
    /// before overflowing.
    pub estimated_energy_consumption_nj: u64,

    /// The time the thread last ran. The exact point in time this value represents
    /// depends on the thread state:
    ///   * `ThreadState::Running`: The time of the last reschedule that selected the thread.
    ///   * `ThreadState::Ready`: The time the thread entered the run queue.
    ///   * Otherwise: The time the thread last ran.
    pub last_started_running: SchedTime,

    /// The current sched_latency flow id for this thread.
    pub flow_id: u64,

    /// The current CPU the thread is `Ready` or `Running` on, `INVALID_CPU` otherwise.
    pub curr_cpu: CpuNum,

    /// The last CPU the thread ran on. `INVALID_CPU` before it first runs.
    pub last_cpu: CpuNum,

    /// The set of CPUs the thread is permitted to run on. The thread is never
    /// assigned to CPUs outside of this set.
    pub hard_affinity: CpuMask,

    /// The set of CPUs the thread should run on if possible. The thread may be
    /// assigned to CPUs outside of this set if necessary.
    pub soft_affinity: CpuMask,
}

/// Internal weight values greater than or equal to zero indicate a fair
/// profile. The internal values of the deadline parameters are invalid and may
/// not be accessed.
pub const K_MIN_FAIR_WEIGHT: SchedWeight = SchedWeight::from_raw(0);

/// Weight values in the range (-10,001, -1] indicate a deadline profile that
/// has not been memoized into a packed diagnostic value. The internal values
/// of the deadline parameters are valid. Directly reading the weight is not
/// allowed.
pub const K_MAX_DEADLINE_WEIGHT: SchedWeight = SchedWeight::from_raw(-1);

/// Weight values less than or equal to -10,001 indicate a deadline profile
/// that has been memoized into a packed diagnostic value. The internal
/// values of the deadline parameters are valid. Directly reading the weight
/// is not allowed.
pub const K_MAX_DIAGNOSTIC_WEIGHT: SchedWeight = SchedWeight::from_raw(-10_001);

impl Default for EffectiveProfile {
    #[inline]
    fn default() -> Self {
        Self {
            weight: SchedWeight::from_raw(0),
            capacity_ns: SchedCompactDuration::from_raw(0),
            deadline_ns: SchedCompactDuration::from_raw(0),
            utilization: SchedUtilization::from_raw(0),
            critical: false,
            #[cfg(scheduler_extra_invariant_validation)]
            dirty_flags: Self::DIRTY_FLAG_CLEAN,
        }
    }
}

impl EffectiveProfile {
    pub fn new(base_profile: &BaseProfile) -> Self {
        let mut ep = Self {
            weight: SchedWeight::from_raw(0),
            capacity_ns: SchedCompactDuration::from_raw(0),
            deadline_ns: SchedCompactDuration::from_raw(0),
            utilization: SchedUtilization::from_raw(0),
            critical: false,
            #[cfg(scheduler_extra_invariant_validation)]
            dirty_flags: Self::DIRTY_FLAG_CLEAN,
        };
        if base_profile.is_fair() {
            let weight = base_profile.fair_weight();
            debug_assert!(weight >= K_MIN_FAIR_WEIGHT);
            ep.weight = weight;
        } else {
            let deadline = base_profile.deadline();
            debug_assert!(deadline.capacity_ns.raw_value() <= i32::MAX as i64);
            debug_assert!(deadline.deadline_ns.raw_value() <= i32::MAX as i64);
            ep.weight = K_MAX_DEADLINE_WEIGHT;
            let cap_fixed = ffl::Fixed::<i32, 0>::from_fixed(deadline.capacity_ns.into_fixed());
            let dead_fixed = ffl::Fixed::<i32, 0>::from_fixed(deadline.deadline_ns.into_fixed());
            ep.capacity_ns = SchedCompactDuration::from(cap_fixed);
            ep.deadline_ns = SchedCompactDuration::from(dead_fixed);
            ep.utilization = deadline.utilization;
            ep.critical = base_profile.critical;
        }
        ep
    }
    pub fn set_deadline(&mut self, deadline_params: SchedDeadlineParams, is_critical: bool) {
        self.weight = K_MAX_DEADLINE_WEIGHT;
        let cap_i32 = Fixed::<i32, 0>::from_fixed(deadline_params.capacity_ns.into_fixed());
        let dead_i32 = Fixed::<i32, 0>::from_fixed(deadline_params.deadline_ns.into_fixed());
        self.capacity_ns = SchedCompactDuration::from(cap_i32);
        self.deadline_ns = SchedCompactDuration::from(dead_i32);
        self.utilization = deadline_params.utilization;
        self.critical = is_critical;
    }
    pub fn set_fair(&mut self, weight: SchedWeight) {
        debug_assert!(weight.raw_value() >= 0);
        self.weight = weight;
    }

    /// Returns a signed 32-bit diagnostic value representing the effective
    /// profile state. Returns either the positive weight or the negative packed
    /// version of the deadline parameters. Memoizes the packed deadline
    /// parameters in a reserved range of the weight to avoid unnecessary
    /// overhead during context switches when parameters remain constant.
    ///
    /// The packed deadline parameter format renders in decimal as -CCCCDDDD,
    /// where CCCC is the capacity and DDDD is the relative deadline (period),
    /// with a precision of 10us. This is useful for visualization in Perfetto,
    /// which only supports 32-bit signed priority values.
    pub fn get_weight_or_packed_deadline_params(&mut self) -> i32 {
        if self.weight >= K_MIN_FAIR_WEIGHT || self.weight <= K_MAX_DIAGNOSTIC_WEIGHT {
            return self.weight.raw_value() as i32;
        }

        let clamped_scaled_capacity = (self.capacity_ns.raw_value() / 10_000).clamp(1, 9_999);
        let clamped_scaled_deadline = (self.deadline_ns.raw_value() / 10_000).clamp(1, 9_999);

        let diagnostic_value = -((clamped_scaled_capacity * 10_000) + clamped_scaled_deadline);
        self.weight = SchedWeight::from_raw(diagnostic_value as i64);

        diagnostic_value
    }
}

/// Returns the weight or memoized packed deadline parameters of the effective profile.
///
/// # Safety
/// The caller must ensure that `ep` points to a valid, initialized `EffectiveProfile`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_effective_profile_get_weight_or_packed_deadline_params(
    ep: *mut EffectiveProfile,
) -> i32 {
    // SAFETY: Caller ensures `ep` is a valid pointer.
    unsafe { (*ep).get_weight_or_packed_deadline_params() }
}

impl SchedulerState {
    /// This value requires the thread's lock to be held, since it may need to
    /// memoize the packed deadline parameters.
    pub fn get_weight_or_packed_deadline_params(&mut self) -> i32 {
        self.effective_profile.get_weight_or_packed_deadline_params()
    }

    /// Converts from kernel priority value in the interval [0, 31] to weight in
    /// the interval (0.0, 1.0]. See the definition of `SchedWeight` for an
    /// explanation of the weight distribution.
    pub fn convert_priority_to_weight(priority: i32) -> SchedWeight {
        debug_assert!((LOWEST_PRIORITY..=HIGHEST_PRIORITY).contains(&priority));
        K_PRIORITY_TO_WEIGHT_TABLE[priority as usize]
    }

    pub fn new(base_profile: BaseProfile) -> Self {
        Self {
            start_time: SchedTime::from_raw(0),
            finish_time: SchedTime::from_raw(0),
            activation_count: 0,
            subtree_invariants: SubtreeInvariants {
                min_finish_time: SchedTime::from_raw(0),
                min_weight: SchedWeight::from_raw(0),
            },
            state: ThreadState::Initial,
            base_profile,
            inherited_profile_values: InheritedProfileValues {
                total_weight: SchedWeight::from_raw(0),
                uncapped_utilization: SchedUtilization::from_raw(0),
                min_deadline: SchedDuration::from_raw(i64::MAX),
                critical_count: 0,
            },
            effective_profile: EffectiveProfile::new(&base_profile),
            time_slice_ns: SchedDuration::from_raw(0),
            time_slice_used_ns: SchedDuration::from_raw(0),
            runtime_ns: SchedDuration::from_raw(0),
            expected_runtime_ns: SchedDuration::from_raw(0),
            banked_runtime_ns: SchedDuration::from_raw(0),
            estimated_energy_consumption_nj: 0,
            last_started_running: SchedTime::from_raw(0),
            flow_id: 0,
            curr_cpu: INVALID_CPU,
            last_cpu: INVALID_CPU,
            hard_affinity: CPU_MASK_ALL,
            soft_affinity: CPU_MASK_ALL,
        }
    }

    /// Recomputes the effective profile from `base_profile` and `inherited_profile_values`.
    /// This should only ever be called while holding the thread's lock.
    pub fn recompute_effective_profile(&mut self) {
        self.effective_profile.assert_dirty();

        let bp = &self.base_profile;
        let ipv = &self.inherited_profile_values;

        ipv.assert_consistency();

        if bp.is_deadline() {
            let bp_deadline = bp.deadline();
            let total_util = ipv.uncapped_utilization + bp_deadline.utilization;
            let new_util = core::cmp::min(total_util, K_THREAD_UTILIZATION_MAX);
            let new_deadline = core::cmp::min(ipv.min_deadline, bp_deadline.deadline_ns);
            let new_capacity = SchedDuration::from(new_util * new_deadline);

            self.effective_profile.set_deadline(
                SchedDeadlineParams {
                    capacity_ns: new_capacity,
                    deadline_ns: new_deadline,
                    utilization: new_util,
                },
                bp.critical || ipv.critical_count > 0,
            );
        } else if ipv.uncapped_utilization > SchedUtilization::from_raw(0) {
            let new_util = core::cmp::min(ipv.uncapped_utilization, K_THREAD_UTILIZATION_MAX);
            let capacity = SchedDuration::from(new_util * ipv.min_deadline);
            self.effective_profile.set_deadline(
                SchedDeadlineParams {
                    capacity_ns: capacity,
                    deadline_ns: ipv.min_deadline,
                    utilization: new_util,
                },
                ipv.critical_count > 0,
            );
        } else {
            // Our thread is fair. We simply end up inheriting the total weight of the
            // threads blocked behind us.
            self.effective_profile.set_fair(bp.fair_weight() + ipv.total_weight);
        }

        self.effective_profile.clean();
    }

    pub fn remaining_time_slice_ns(&self) -> SchedDuration {
        self.time_slice_ns - self.time_slice_used_ns
    }
    pub fn effective_period(&self) -> SchedDuration {
        self.finish_time - self.start_time
    }

    /// Returns the effective mask of CPUs a thread may run on, based on the
    /// thread's affinity masks and CPUs currently active on the system.
    pub fn get_effective_cpu_mask(&self, active_mask: CpuMask) -> CpuMask {
        // The thread may run on any active CPU allowed by both its hard and
        // soft CPU affinity.
        let available_mask = self.hard_affinity & self.soft_affinity & active_mask;

        // Return the mask honoring soft affinity if it is viable, otherwise ignore
        // soft affinity and honor only hard affinity.
        if available_mask != 0 { available_mask } else { self.hard_affinity & active_mask }
    }
}

/// Recomputes the effective profile of the scheduler state.
///
/// # Safety
/// The caller must ensure that `state` points to a valid, initialized `SchedulerState`
/// and that the appropriate locks are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_state_recompute_effective_profile(
    state: *mut SchedulerState,
) {
    // SAFETY: Caller ensures state pointer is valid and locks are held.
    unsafe { (*state).recompute_effective_profile() };
}

/// The disposition is a concise representation of the valid combinations of
/// states of the members of `SchedulerQueueState`. It is used in conjunction with the
/// thread state to determine which operations are valid on a thread and its
/// associated scheduler, if any.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueDisposition {
    /// Corresponds to `active=false`, `in_queue=false`, `stolen_by=INVALID_CPU`.
    Unassociated = 0,
    /// Corresponds to `active=true`, `in_queue=false`, `stolen_by=INVALID_CPU`.
    Associated = 1,
    /// Corresponds to `active=true`, `in_queue=true`, `stolen_by=INVALID_CPU`.
    Enqueued = 2,
    /// Corresponds to `active=false`, `in_queue=false`, `stolen_by=<CPU num>`.
    Stolen = 3,
}

/// The types of affinity the scheduler understands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum AffinityType {
    Hard = 0,
    Soft = 1,
}

/// `SchedulerQueueState` tracks the association with a scheduler and run queue. To
/// accommodate the different ways a thread can be accessed (i.e. starting from a
/// thread vs. starting from a run queue), specific locking rules apply to this
/// member of `Thread` (i.e. `Thread::scheduler_queue_state`).
///
/// There are two locks that apply to interactions with the queue state:
///
///  1. `Thread::lock` (or the thread's lock): Protects most of a thread's data
///     members, including the scheduling state (i.e. `SchedulerState`).
///     `SchedulerState::curr_cpu` indicates which CPU and scheduler
///     the thread is currently associated with.
///
///  2. `Scheduler::queue_lock` (or the queue lock): Protects a scheduler's data
///     members, including the run queue and associated bookkeeping. This lock
///     also protects the thread's scheduler queue state member while the thread
///     is associated with a particular scheduler.
///
/// When both a thread's lock and a scheduler's queue lock must be held at
/// the same time, commonly needed during rescheduling and PI operations,
/// the defined lock order is to acquire the thread's lock before the queue
/// lock. Fortunately, most operations start from a thread and proceed to
/// interact with a scheduler (e.g. blocking, unblocking, PI operations),
/// naturally conforming to the required lock order. However, several
/// operations start from a scheduler (e.g. finding the next thread to run,
/// stealing a thread from another CPU), and require some lock juggling to
/// complete their operations.
///
/// Operations that start from a scheduler typically dequeue a thread from a
/// run queue while holding the queue lock protecting that run queue.
/// Locking the thread to complete the operation involves releasing the
/// currently held queue lock, acquiring the thread's lock, and then
/// acquiring either the same queue lock or a different queue lock.
///
/// For example, selecting the next thread to run during a reschedule
/// involves the following lock juggling sequence:
/// 1. With the local queue lock held, dequeue the next thread to run.
/// 2. Release the queue lock.
/// 3. Acquire the next thread's lock.
/// 4. Re-acquire the local queue lock and continue the reschedule.
///
/// Stealing a thread from another CPU involves a similar lock juggling
/// sequence:
/// 1. With the source queue lock held, dequeue the thread to steal and
///    (mostly) disassociate it with the source scheduler.
/// 2. Release the source queue lock.
/// 3. Acquire the stolen thread's lock.
/// 4. Acquire the local queue lock, associate the thread with the local
///    scheduler, and continue the reschedule.
///
/// In both cases, since the thread is no longer in a queue after step 1,
/// it cannot be selected or stolen by another CPU after the queue lock is
/// released. However, because the thread is not yet locked between steps 2 and
/// 3, an operation starting at the thread and holding the thread's lock could
/// interleave with the locking sequence. If such an interleaved operation
/// involves updating the thread's run queue position and/or the associated
/// scheduler's bookkeeping, care must be taken to avoid double-dequeuing the
/// thread, updating the wrong scheduler's bookkeeping, or rescheduling the
/// wrong CPU.
///
/// `QueueDisposition` is an enumeration of the valid combinations
/// of a thread's scheduler queue states that may be used in conjunction with the
/// thread state (i.e. `SchedulerState::state`) to determine how the thread and the
/// associated scheduler can be updated.
///
/// The following state combinations may be observed by operations that start at
/// a thread and hold both the thread's lock and the queue lock:
///
/// 1. `state=Running`, `disposition=Associated`:
///
///    Observable when a thread is running. Because the currently running
///    thread's lock is held over a reschedule, there are no observable
///    transitional states to deal with in PI and affinity change operations.
///
///    Holding the thread lock delays rescheduling and transitioning from `Running`
///    to any other state. The thread's scheduling state and scheduler
///    bookkeeping may be updated by a PI operation or affinity change. The CPU
///    performing the update is obligated to reschedule the CPU the thread is
///    running on when the update is complete.
///
/// 2. `state=Ready`, `disposition=Enqueued`:
///
///    Observable when a thread is waiting in the run queue and is not in the
///    process of rescheduling.
///
///    Holding the queue lock delays the thread from transitioning to `Running`,
///    being stolen, or migrated by another CPU. The thread's scheduling state,
///    scheduler bookkeeping, and scheduler queue state may be updated by a PI
///    operation or affinity change. The CPU performing the update is obligated
///    to reschedule the CPU the thread is associated with if the change might
///    affect the currently running thread.
///
/// 3. `state=Ready`, `disposition=Associated`:
///
///    Observable when a thread is rescheduling and transitioning from `Ready` to
///    `Running` and the lock juggling of the rescheduling CPU races with an
///    update.
///
///    During the reschedule, when the next thread has been dequeued it is
///    expected to be the next thread to run on the CPU. To complete the
///    transition, the scheduler releases the queue lock (allowing this
///    observation from another CPU), acquires the thread lock, and re-acquires
///    the queue lock.
///
///    Holding the thread lock on another CPU delays the completion of the
///    transition, allowing the effective profile, dynamic parameters, and
///    scheduler bookkeeping or affinity to be updated.
///
///    Note: An update of the thread's effective profile and dynamic parameters
///    can invalidate the thread as the correct/best choice to run. The CPU
///    performing the update is obligated to reschedule the CPU the thread is
///    running on, either unconditionally (current behavior) or conditionally if
///    another thread should run instead (future optimization).
///
///    Likewise, a change to the thread's affinity mask may invalidate the CPU
///    the thread is about to run on as a viable option. The CPU performing the
///    update is obligated to reschedule the CPU to cause it to migrate the
///    thread to a viable target.
///
/// 4. `state=Ready`, `disposition=Stolen`:
///
///    Observable when a thread is being stolen and the lock juggling of
///    the stealing CPU races with an update from another CPU.
///
///    When the thread is being stolen, it is dequeued and removed from the
///    previous scheduler's bookkeeping. To complete the steal, the source
///    queue lock is released (allowing this observation from another CPU),
///    the thread lock is acquired, and the destination queue lock is
///    acquired. Since the stolen thread will become the currently running
///    thread on the stealing CPU, rescheduling is unnecessary and can be
///    omitted in a future optimization.
///
///    Holding the thread lock on another CPU delays the steal, allowing the
///    effective profile and dynamic parameters or affinity mask to be updated.
///    In this state, the thread is not associated with any scheduler
///    bookkeeping, but the current CPU of the thread is stale. However, holding the
///    queue lock of the stale CPU allows the CPU performing the update to make a
///    coherent observation of the `stolen_by` member, which can be used to
///    determine if an affinity mask change has invalidated the stealing CPU as a
///    viable option. The updating CPU is obligated to reschedule the stealing
///    CPU if it became an invalid option due to the update.
///
/// 5. `state=Initial,Blocked*,Sleeping,Suspended,Death`,
///    `disposition=Unassociated`:
///
///    Observable only by the CPU performing a reschedule that transitions the
///    current thread from `Running` to one of the non-runnable states. Since a
///    reschedule of the current thread occurs with the thread lock held, and
///    transitioning to a non-runnable state clears the current CPU for the
///    thread, no other CPU can observe this state while holding the thread's
///    lock and a queue lock.
#[repr(C)]
pub struct SchedulerQueueState {
    /// The WAVLTree node for enqueuing the thread in a run queue.
    pub run_queue_node: WavlTreeNode<Thread>,

    /// The id of the CPU currently in the process of stealing the thread, or
    /// `INVALID_CPU` if the thread is not being stolen.
    pub stolen_by: CpuNum,

    /// Flag indicating whether this thread is associated with a specific CPU's
    /// scheduler and bookkeeping.
    pub active: bool,
}

impl SchedulerQueueState {
    /// Sets the thread state to active (i.e. associated with a specific CPU's
    /// scheduler and bookkeeping).
    ///
    /// Returns true if the thread was not previously active.
    pub fn on_insert(&mut self) -> bool {
        let was_active = self.active;
        self.active = true;
        self.stolen_by = INVALID_CPU;
        !was_active
    }

    /// Sets the thread state to inactive (i.e. not associated with a CPU's
    /// scheduler or bookkeeping). If the thread is being stolen from another CPU's
    /// run queue, `stolen_by` must be the CPU id of the stealing CPU, otherwise it
    /// must be `INVALID_CPU`.
    ///
    /// Returns true if the task was previously active.
    pub fn on_remove(&mut self, stolen_by: CpuNum) -> bool {
        let was_active = self.active;
        self.active = false;
        self.stolen_by = stolen_by;
        was_active
    }

    /// Returns true if the thread is currently enqueued in a run queue.
    pub fn in_queue(&self) -> bool {
        self.run_queue_node.in_container()
    }

    /// Returns the disposition of this scheduler queue state. Asserts on invalid
    /// combinations.
    pub fn disposition(&self) -> QueueDisposition {
        if self.active {
            debug_assert_eq!(self.stolen_by, INVALID_CPU);
            if self.in_queue() { QueueDisposition::Enqueued } else { QueueDisposition::Associated }
        } else {
            debug_assert!(!self.in_queue());
            if self.stolen_by == INVALID_CPU {
                QueueDisposition::Unassociated
            } else {
                QueueDisposition::Stolen
            }
        }
    }
}

#[repr(C)]
#[derive(Clone, Copy)]
pub struct WaitQueueInheritedSchedulerState {
    pub ipvs: InheritedProfileValues,
    pub start_time: SchedTime, // TODO(johngro): Do we need this?
    pub finish_time: SchedTime,
    pub time_slice_ns: SchedDuration,
    pub time_slice_used_ns: SchedDuration,
}

impl WaitQueueInheritedSchedulerState {
    /// Resets this state to its default (newly constructed) values.
    #[inline]
    pub fn reset(&mut self) {
        self.ipvs = InheritedProfileValues::default();
        self.start_time = SchedTime::from_raw(0);
        self.finish_time = SchedTime::from_raw(0);
        self.time_slice_ns = SchedDuration::from_raw(0);
        self.time_slice_used_ns = SchedDuration::from_raw(0);
    }

    /// If we have extra validation enabled, and this queue is no longer
    /// inheriting any deadline pressure (even if there are still waiters),
    /// then reset the dynamic parameters as well.
    ///
    /// The dynamic parameters (start time, finish time, time slice) are
    /// technically undefined when we are not inheriting any utilization.  Fair
    /// thread do not have defined dynamic parameters when they are blocked.
    ///
    /// In a production build with no extra validation checks, it should not be
    /// necessary to ever touch them once they become undefined. Their values
    /// will be overwritten later on if/when they do finally become defined
    /// again.  In a build with extra checks enabled, however, it can be
    /// beneficial to reset them to known default values when they are in the
    /// "undefined" state, in order to make it easier to catch an accidental use
    /// of the parameters when they have no defined meaning.
    #[inline]
    pub fn reset_dynamic_parameters(&mut self) {
        if cfg!(scheduler_extra_invariant_validation) {
            assert!(self.ipvs.uncapped_utilization == SchedUtilization::from_raw(0));
            assert!(self.ipvs.min_deadline == SchedDuration::from_raw(i64::MAX));
            self.start_time = SchedTime::from_raw(0);
            self.finish_time = SchedTime::from_raw(0);
            self.time_slice_ns = SchedDuration::from_raw(0);
            self.time_slice_used_ns = SchedDuration::from_raw(0);
        }
    }

    /// Asserts (when extra validation is enabled) that the dynamic parameters are reset.
    #[inline]
    pub fn assert_dynamic_parameters_are_reset(&self) {
        if cfg!(scheduler_extra_invariant_validation) {
            assert!(self.ipvs.uncapped_utilization == SchedUtilization::from_raw(0));
            assert!(self.ipvs.min_deadline == SchedDuration::from_raw(i64::MAX));
            assert!(self.start_time == SchedTime::from_raw(0));
            assert!(self.finish_time == SchedTime::from_raw(0));
            assert!(self.time_slice_ns == SchedDuration::from_raw(0));
            assert!(self.time_slice_used_ns == SchedDuration::from_raw(0));
        }
    }

    /// Asserts (when extra validation is enabled) that this state is reset.
    #[inline]
    pub fn assert_is_reset(&self) {
        if cfg!(scheduler_extra_invariant_validation) {
            assert!(self.ipvs.total_weight == SchedWeight::from_raw(0));
            self.assert_dynamic_parameters_are_reset();
        }
    }

    /// Get the effective profile of an `OwnedWaitQueue`. The effective profile of an
    /// `OwnedWaitQueue` is the composition of the effective profiles of all the
    /// threads currently waiting in that queue. As such, the effective profile may
    /// change as threads are added to or removed from the queue or when the
    /// effective profile of any thread in the queue changes, either due to base
    /// profile changes or transitive propagation of upstream profile changes from
    /// owned wait queues that they own.
    pub fn get_effective_profile(&self) -> EffectiveProfile {
        let mut ep = EffectiveProfile {
            weight: SchedWeight::from_raw(0),
            capacity_ns: SchedCompactDuration::from_raw(0),
            deadline_ns: SchedCompactDuration::from_raw(0),
            utilization: SchedUtilization::from_raw(0),
            critical: false,
            #[cfg(scheduler_extra_invariant_validation)]
            dirty_flags: EffectiveProfile::DIRTY_FLAG_CLEAN,
        };
        self.ipvs.assert_consistency();
        if self.ipvs.uncapped_utilization > SchedUtilization::from_raw(0) {
            let utilization =
                core::cmp::min(K_THREAD_UTILIZATION_MAX, self.ipvs.uncapped_utilization);
            let capacity = SchedDuration::from(utilization * self.ipvs.min_deadline);
            ep.set_deadline(
                SchedDeadlineParams {
                    capacity_ns: capacity,
                    deadline_ns: self.ipvs.min_deadline,
                    utilization,
                },
                self.ipvs.critical_count > 0,
            );
        } else {
            ep.set_fair(self.ipvs.total_weight);
        }
        ep
    }
}

// SubtreeInvariants layout assertions
zr::static_assert!(core::mem::size_of::<SubtreeInvariants>() == 16);
zr::static_assert!(core::mem::align_of::<SubtreeInvariants>() == 8);
zr::static_assert!(core::mem::offset_of!(SubtreeInvariants, min_finish_time) == 0);
zr::static_assert!(core::mem::offset_of!(SubtreeInvariants, min_weight) == 8);

// InheritedProfileValues layout assertions
zr::static_assert!(core::mem::size_of::<InheritedProfileValues>() == 32);
zr::static_assert!(core::mem::align_of::<InheritedProfileValues>() == 8);
zr::static_assert!(core::mem::offset_of!(InheritedProfileValues, total_weight) == 0);
zr::static_assert!(core::mem::offset_of!(InheritedProfileValues, uncapped_utilization) == 8);
zr::static_assert!(core::mem::offset_of!(InheritedProfileValues, min_deadline) == 16);
zr::static_assert!(core::mem::offset_of!(InheritedProfileValues, critical_count) == 24);

// EffectiveProfile layout assertions
zr::static_assert!(core::mem::size_of::<EffectiveProfile>() == 32);
zr::static_assert!(core::mem::align_of::<EffectiveProfile>() == 8);
zr::static_assert!(core::mem::offset_of!(EffectiveProfile, weight) == 0);
zr::static_assert!(core::mem::offset_of!(EffectiveProfile, capacity_ns) == 8);
zr::static_assert!(core::mem::offset_of!(EffectiveProfile, deadline_ns) == 12);
zr::static_assert!(core::mem::offset_of!(EffectiveProfile, utilization) == 16);
zr::static_assert!(core::mem::offset_of!(EffectiveProfile, critical) == 24);
#[cfg(scheduler_extra_invariant_validation)]
zr::static_assert!(core::mem::offset_of!(EffectiveProfile, dirty_flags) == 28);

// SchedulerState layout assertions
zr::static_assert!(core::mem::size_of::<SchedulerState>() == 224);
zr::static_assert!(core::mem::align_of::<SchedulerState>() == 8);
zr::static_assert!(core::mem::offset_of!(SchedulerState, start_time) == 0);
zr::static_assert!(core::mem::offset_of!(SchedulerState, finish_time) == 8);
zr::static_assert!(core::mem::offset_of!(SchedulerState, activation_count) == 16);
zr::static_assert!(core::mem::offset_of!(SchedulerState, subtree_invariants) == 24);
zr::static_assert!(core::mem::offset_of!(SchedulerState, state) == 40);
zr::static_assert!(core::mem::offset_of!(SchedulerState, base_profile) == 48);
zr::static_assert!(core::mem::offset_of!(SchedulerState, inherited_profile_values) == 80);
zr::static_assert!(core::mem::offset_of!(SchedulerState, effective_profile) == 112);
zr::static_assert!(core::mem::offset_of!(SchedulerState, time_slice_ns) == 144);
zr::static_assert!(core::mem::offset_of!(SchedulerState, time_slice_used_ns) == 152);
zr::static_assert!(core::mem::offset_of!(SchedulerState, runtime_ns) == 160);
zr::static_assert!(core::mem::offset_of!(SchedulerState, expected_runtime_ns) == 168);
zr::static_assert!(core::mem::offset_of!(SchedulerState, banked_runtime_ns) == 176);
zr::static_assert!(core::mem::offset_of!(SchedulerState, estimated_energy_consumption_nj) == 184);
zr::static_assert!(core::mem::offset_of!(SchedulerState, last_started_running) == 192);
zr::static_assert!(core::mem::offset_of!(SchedulerState, flow_id) == 200);
zr::static_assert!(core::mem::offset_of!(SchedulerState, curr_cpu) == 208);
zr::static_assert!(core::mem::offset_of!(SchedulerState, last_cpu) == 212);
zr::static_assert!(core::mem::offset_of!(SchedulerState, hard_affinity) == 216);
zr::static_assert!(core::mem::offset_of!(SchedulerState, soft_affinity) == 220);

// SchedulerQueueState layout assertions
zr::static_assert!(core::mem::size_of::<SchedulerQueueState>() == 40);
zr::static_assert!(core::mem::align_of::<SchedulerQueueState>() == 8);
zr::static_assert!(core::mem::offset_of!(SchedulerQueueState, run_queue_node) == 0);
zr::static_assert!(core::mem::offset_of!(SchedulerQueueState, stolen_by) == 32);
zr::static_assert!(core::mem::offset_of!(SchedulerQueueState, active) == 36);

// WaitQueueInheritedSchedulerState layout assertions
zr::static_assert!(core::mem::size_of::<WaitQueueInheritedSchedulerState>() == 64);
zr::static_assert!(core::mem::align_of::<WaitQueueInheritedSchedulerState>() == 8);
zr::static_assert!(core::mem::offset_of!(WaitQueueInheritedSchedulerState, ipvs) == 0);
zr::static_assert!(core::mem::offset_of!(WaitQueueInheritedSchedulerState, start_time) == 32);
zr::static_assert!(core::mem::offset_of!(WaitQueueInheritedSchedulerState, finish_time) == 40);
zr::static_assert!(core::mem::offset_of!(WaitQueueInheritedSchedulerState, time_slice_ns) == 48);
zr::static_assert!(
    core::mem::offset_of!(WaitQueueInheritedSchedulerState, time_slice_used_ns) == 56
);

// RunQueue layout assertions
zr::static_assert!(core::mem::size_of::<RunQueue>() == 32);
zr::static_assert!(core::mem::align_of::<RunQueue>() == 8);

/// A coherent pair of corresponding time/ticks values in the monotonic and
/// boot timelines.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct SchedMonoTimeAndBootTicks {
    pub mono_time: SchedTime,
    pub boot_ticks: zx_types::zx_instant_boot_ticks_t,
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct CpuSearchSetEntry {
    pub cpu: CpuNum,
    pub cluster: usize,
}

#[repr(C, align(8))]
pub struct PowerLevel(zr::Opaque<[u8; 0]>);

#[repr(C, align(8))]
pub struct SaveStateList(zr::Opaque<[*mut Thread; 1]>);

#[repr(C, align(8))]
pub struct UnblockList(zr::Opaque<[*mut Thread; 1]>);

#[allow(improper_ctypes)]
unsafe extern "C" {
    fn cpp_thread_call_migrate_fn_locked(thread: *mut Thread, stage: i32);
    fn cpp_wait_queue_get_lock(wq: *const WaitQueueBase) -> *const ChainLock;
    fn cpp_scheduler_save_state_list_push_front(sched: *mut Scheduler, thread: *mut Thread);

    fn cpp_thread_get_blocking_wait_queue(thread: *const Thread) -> *mut WaitQueueBase;
    fn cpp_wait_queue_update_blocked_thread_effective_profile(
        wq: *mut WaitQueueBase,
        thread: *mut Thread,
    );
    fn cpp_current_mono_time_and_boot_ticks(out: *mut SchedMonoTimeAndBootTicks);

    fn cpp_thread_get_lock(thread: *mut Thread) -> *mut ChainLock;
    fn cpp_thread_preempts_pending_add(thread: *mut Thread, mask: CpuMask);
    fn cpp_thread_preempts_pending(thread: *const Thread) -> CpuMask;
    fn cpp_thread_preempts_pending_clear(thread: *mut Thread);
    fn cpp_thread_preempt_evaluate_timeslice_extension(thread: *mut Thread) -> bool;
    fn cpp_thread_preempt_eager_resched_disable_count(thread: *const Thread) -> u32;
    fn cpp_thread_preempt_set_pending();
    fn cpp_thread_set_ready(thread: *mut Thread);
    fn cpp_arch_num_spinlocks_held() -> u32;
    fn cpp_unblock_list_pop_back(list: *mut UnblockList) -> *mut Thread;

    fn cpp_thread_get_queue_node(thread: *mut Thread) -> *mut WavlTreeNode<Thread, bool>;
    fn cpp_thread_get_scheduler_state(thread: *mut Thread) -> *mut SchedulerState;
    fn cpp_thread_get_queue_state(thread: *mut Thread) -> *mut SchedulerQueueState;
    fn cpp_thread_get_tid(thread: *const Thread) -> u64;
    fn cpp_owq_get_inherited_state(
        owq: *mut OwnedWaitQueue,
    ) -> *mut WaitQueueInheritedSchedulerState;
    fn cpp_owq_get_inherited_state_const(
        owq: *const OwnedWaitQueue,
    ) -> *const WaitQueueInheritedSchedulerState;
    fn cpp_scheduler_get_normalized_utilization(sched: *const Scheduler) -> SchedUtilization;
    fn cpp_thread_is_idle(thread: *const Thread) -> bool;
    fn cpp_thread_dump_active(thread: *const Thread, output_target: *mut libc::stdio::FILE);

    fn cpp_scheduler_get(cpu: CpuNum) -> *mut Scheduler;
    fn cpp_thread_has_migrate_fn(thread: *const Thread) -> bool;
    fn cpp_thread_migrate_pending(thread: *const Thread) -> bool;

    fn cpp_get_cpu_stats(cpu_num: CpuNum) -> *mut CpuStats;
    fn cpp_scheduler_power_state_active_power_coefficient_nw(sched: *const Scheduler) -> u64;
    fn cpp_scheduler_power_level_control_max_idle_power_coefficient_nw(
        sched: *const Scheduler,
    ) -> u64;

    fn cpp_scheduler_lock_queue(sched: *mut Scheduler);
    fn cpp_scheduler_unlock_queue(sched: *mut Scheduler);
    fn cpp_scheduler_lock_queue_irq_save(sched: *mut Scheduler) -> u64;
    fn cpp_scheduler_unlock_queue_irq_restore(sched: *mut Scheduler, state: u64);

    fn cpp_thread_update_runtime_stats(thread: *mut Thread, state: i32);

    fn cpp_thread_get_disposition(thread: *const Thread) -> i32;
    fn cpp_stall_accumulator_apply_context_switch(
        current_thread: *const Thread,
        next_thread: *const Thread,
    );

    fn cpp_scheduler_get_idle_thread(sched: *const Scheduler) -> *mut Thread;
    fn cpp_scheduler_has_pending_power_work(sched: *const Scheduler) -> bool;

    fn cpp_scheduler_power_level_control_is_enabled(sched: *const Scheduler) -> bool;
    fn cpp_scheduler_power_state_scheduler_control_enabled(sched: *const Scheduler) -> bool;
    fn cpp_scheduler_power_state_is_serving(sched: *const Scheduler) -> bool;
    fn cpp_scheduler_power_state_is_valid_active_power_level(
        sched: *const Scheduler,
        power_level: u8,
    ) -> bool;
    fn cpp_scheduler_power_state_domain_cpus(sched: *const Scheduler) -> CpuMask;
    fn cpp_scheduler_power_state_target_processing_rate(
        sched: *const Scheduler,
    ) -> SchedProcessingRate;
    fn cpp_scheduler_power_state_preceding_target_processing_rate(
        sched: *const Scheduler,
    ) -> SchedProcessingRate;
    fn cpp_scheduler_power_state_has_preceding_target_power_level(sched: *const Scheduler) -> bool;
    fn cpp_scheduler_power_state_find_active_power_level_for_rate(
        sched: *const Scheduler,
        processing_rate: SchedProcessingRate,
    ) -> *const PowerLevel;
    fn cpp_power_level_get_level(power_level: *const PowerLevel) -> u8;
    fn cpp_scheduler_power_state_request_transition(
        sched: *mut Scheduler,
        cpu: CpuNum,
        power_level: u8,
        out_request: *mut PowerLevelUpdateRequest,
    ) -> bool;
    fn cpp_scheduler_power_state_post_update_request(
        sched: *const Scheduler,
        request: *const PowerLevelUpdateRequest,
    ) -> CpuMask;
    fn cpp_scheduler_power_state_update_active_power_level(
        sched: *mut Scheduler,
        power_level: u8,
        out_active_rate: *mut SchedProcessingRate,
    ) -> zx_types::zx_status_t;
    fn cpp_timer_queue_preempt_reset(cpu: CpuNum, deadline: zx_types::zx_instant_mono_t);
    fn cpp_timer_queue_preempt_armed(cpu: CpuNum) -> bool;
    fn cpp_thread_call_context_switch_fn_locked(thread: *mut Thread);
    fn cpp_thread_get_aspace(thread: *mut Thread) -> *mut VmAspace;
    fn cpp_vmm_context_switch(old_aspace: *mut VmAspace, new_aspace: *mut VmAspace);
    fn cpp_arch_context_switch(current_thread: *mut Thread, next_thread: *mut Thread);
    fn cpp_thread_signal_check_restartable_sequence_if_needed(thread: *mut Thread);

    fn cpp_get_cpu_search_set_entries(cpu: CpuNum) -> *const CpuSearchSetEntry;
    fn cpp_get_cpu_search_set_count(cpu: CpuNum) -> usize;
    fn cpp_scheduler_get_power_domain_set_count(sched: *const Scheduler) -> usize;
    fn cpp_scheduler_lookup_power_cost(
        sched: *const Scheduler,
        cpu: CpuNum,
        rate: SchedProcessingRate,
    ) -> u64;
    fn cpp_scheduler_lookup_total_normalized_utilization(
        sched: *const Scheduler,
        cpu: CpuNum,
    ) -> SchedUtilization;
    fn cpp_thread_canary_assert(thread: *const Thread);
    fn cpp_scheduler_power_level_control_update_normalized_utilization(
        sched: *mut Scheduler,
        delta: SchedUtilization,
    ) -> SchedUtilization;
    fn cpp_scheduler_power_level_control_domain_id(sched: *const Scheduler) -> u32;
    fn cpp_scheduler_power_level_control_total_normalized_utilization(
        sched: *const Scheduler,
    ) -> SchedUtilization;

    fn cpp_unblock_list_init(holder: *mut UnblockList);
    fn cpp_unblock_list_destroy(holder: *mut UnblockList);
    fn cpp_unblock_list_push_back(holder: *mut UnblockList, thread: *mut Thread);
    fn cpp_unblock_list_pop_front(holder: *mut UnblockList) -> *mut Thread;
    fn cpp_unblock_list_is_empty(holder: *const UnblockList) -> bool;

    fn cpp_scheduler_save_state_list_is_empty(sched: *mut Scheduler) -> bool;
    fn cpp_scheduler_save_state_list_move(sched: *mut Scheduler, holder: *mut SaveStateList);
    fn cpp_save_state_list_pop(holder: *mut SaveStateList) -> *mut Thread;
    fn cpp_save_state_list_destroy(holder: *mut SaveStateList);

    fn cpp_thread_call_migrate_fn_for_cpu(cpu: CpuNum);
    fn cpp_scheduler_block(current_thread: *mut Thread);
    fn cpp_scheduler_unblock(thread: *mut Thread);
    fn cpp_scheduler_unblock_list(list: *mut core::ffi::c_void);
}

/// Returns a pointer to the `ChainLock` embedded in `thread`.
///
/// # Safety
/// `thread` must point to a valid `Thread`.
#[inline(always)]
unsafe fn thread_get_lock(thread: *const Thread) -> *const ChainLock {
    // SAFETY: The caller guarantees `thread` is valid. `cpp_thread_get_lock` only
    // computes the address of the lock embedded in the thread and does not
    // mutate the thread, so the pointer may be used as a shared pointer.
    unsafe { cpp_thread_get_lock(thread.cast_mut()) }
}

/// Reciprocal of the default fair period to minimize runtime divisions in fair
/// utilization calculations.
pub const K_RECIPROCAL_DEFAULT_FAIR_PERIOD: SchedUtilization = SchedUtilization::from_raw(214);
pub const USER_PROCESSING_RATE_SCALE: u64 = 1000;

/// Number of fractional bits in the userspace CPU performance scale format.
const USER_SCALE_FRACTIONAL_BITS: usize = u32::BITS as usize;

/// The fixed-point format of the userspace CPU performance scale.
type UserScale = Fixed<u64, USER_SCALE_FRACTIONAL_BITS>;

/// Converts a userspace CPU performance scale to a `SchedProcessingRate` value.
///
/// The returned value will always be greater than zero.
#[inline]
pub fn to_sched_processing_rate(
    value: zx_types::zx_cpu_performance_scale_t,
) -> SchedProcessingRate {
    let raw = ((value.integer_part as u64) << USER_SCALE_FRACTIONAL_BITS)
        | (value.fractional_part as u64);
    let user_scale = UserScale::from_raw(raw);
    let performance_scale = SchedProcessingRate::from(Fixed::<i64, 31>::from_fixed(user_scale));
    performance_scale.clamp(
        SchedProcessingRate::from(Fixed::epsilon()),
        SchedProcessingRate::from(Fixed::from_integer(1)),
    )
}

/// Converts a `SchedProcessingRate` value to a userspace CPU performance scale.
#[inline]
pub fn to_user_performance_scale(
    value: SchedProcessingRate,
) -> zx_types::zx_cpu_performance_scale_t {
    let user_scale = UserScale::from_fixed(value.into_fixed());
    let integral = user_scale.integral().raw_value() >> USER_SCALE_FRACTIONAL_BITS;
    let fractional = user_scale.fraction().raw_value();
    zx_types::zx_cpu_performance_scale_t {
        integer_part: integral as u32,
        fractional_part: fractional as u32,
    }
}

/// Represents a requested power level update to be executed at a later stage.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct PowerLevelUpdateRequest {
    /// Domain ID where the request is originating from.
    pub domain_id: u32,
    /// Target ID of the device or domain that should be transitioned.
    pub target_id: u32,
    /// Interface this request should be routed to.
    pub control: u64,
    /// Argument used for transitioning through the control interface.
    pub control_argument: u64,
    /// Options that determine what `target_id` is. This is determined by the
    /// power level being transitioned to.
    pub options: u32,
    pub _pad: u32,
}

/// `PowerLevelControlState` encapsulates the CPU energy model and power level control interface
/// state for energy aware scheduling.
///
/// Instances of `PowerLevelControlState` are protected by the scheduler's `queue_lock`.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct PowerLevelControlState {
    /// The normalized processing rate of this processor at its current active power level.
    pub processing_rate: SchedProcessingRate,
    /// The reciprocal of the normalized processing rate.
    pub processing_rate_reciprocal: SchedProcessingRate,
    /// The potentially pending update to the normalized processing rate. This value is set
    /// whenever the power level controller acknowledges completion of a power level change request.
    pub updated_processing_rate: SchedProcessingRate,
    pub default_processing_rate: SchedProcessingRate,
    /// The maximum processing rate of this processor. Initially set from the CPU topology
    /// data and updated whenever the energy model is set/updated by userspace.
    pub max_processing_rate: SchedProcessingRate,
    /// The min and max effective processing rate for this processor. These
    /// values influence both the actual processing rate of the power domain the
    /// processor belongs to and the effective processing rate of this specific
    /// processor.
    ///
    /// Initially set to 0.0 and updated by userspace to effect performance limits policy.
    pub processing_rate_limit_min: SchedProcessingRate,
    /// Initially set to 1.0 and updated by userspace to effect performance limits policy.
    pub processing_rate_limit_max: SchedProcessingRate,
    /// The power coefficient for the current active power level.
    pub active_power_coefficient_nw: u64,
    pub pending_update_request: PowerLevelUpdateRequest,
    /// True if there is a pending request to change the active power level.
    pub has_pending_update_request: bool,
}

impl PowerLevelControlState {
    /// Returns the given demand clamped to the current rate limits.
    ///
    /// Note that the minimum limit can be greater than the maximum limit, in
    /// which case the maximum limit takes precedence to correctly support using
    /// the maximum for thermal mitigation.
    #[inline]
    pub fn clamp_demand(&self, demand: SchedUtilization) -> SchedUtilization {
        let min_limit =
            core::cmp::min(self.processing_rate_limit_min, self.processing_rate_limit_max);
        demand.clamp(min_limit, self.processing_rate_limit_max)
    }

    /// Returns the maximum processing rate of this processor, clamped to the
    /// current performance limits.
    #[inline]
    pub fn clamped_max_processing_rate(&self) -> SchedProcessingRate {
        core::cmp::min(self.max_processing_rate, self.processing_rate_limit_max)
    }

    /// Returns true if there is a pending update to the processing rate that has not yet been
    /// processed by the scheduler. Checked by `reschedule_common` after accounting has been updated
    /// for the runtime segment covering the previous processing rate.
    #[inline]
    pub fn is_processing_rate_update_pending(&self) -> bool {
        self.processing_rate != self.updated_processing_rate
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BandwidthReservationEntry {
    pub finish_time: SchedTime,
    pub utilization: SchedUtilization,
    pub tid: zx_types::zx_koid_t,
}

impl BandwidthReservationEntry {
    #[inline]
    pub const fn new() -> Self {
        Self {
            finish_time: SchedTime::from_raw(0),
            utilization: SchedUtilization::from_raw(0),
            tid: zx_types::ZX_KOID_INVALID,
        }
    }

    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.tid != zx_types::ZX_KOID_INVALID
    }

    #[inline]
    pub fn reset(&mut self) {
        self.tid = zx_types::ZX_KOID_INVALID;
    }
}

impl Default for BandwidthReservationEntry {
    fn default() -> Self {
        Self::new()
    }
}

/// # Bandwidth Reservation Cache
///
/// Bandwidth Reservation Cache (BRC) is an approximation of the robust Bandwidth
/// Reservation (BR) algorithm that trades off temporary inaccuracy, compared to
/// BR, for increased efficiency and scalability when determining processor
/// execution rates from workload bandwidth demands.
///
/// ## Instantaneous vs. Reserved Demand
///
/// The execution rate R_{p,\omega} of processor p must be sufficient to meet the
/// bandwidth demand U_{p,A} of the set of active tasks A running on the
/// processor:
///
///   U_{p,A} = \sum_{i \in A} U_i \le R_{p,\omega}
///
/// Where the execution rate R_{p,\omega} is in the set of operating points
/// available to processor p:
///
///   R_{p,\omega} \in \{ R_{p,1}, R_{p,2}, ..., R_{p,n} \} : 0 \le R_{p,\omega} \le 1
///
/// While it is correct to raise the execution rate to the minimum viable
/// operating point when instantaneous demand on the processor increases, naively
/// decreasing the execution rate when instantaneous demand decreases can result
/// in missed deadlines. When task i blocks, the scheduler is still obligated to
/// complete the task's remaining work for its pending job J_{i,k} by its finish
/// time f_{i,k}. Reducing the execution rate prevents other tasks from getting
/// sufficiently ahead using the slack time created by the blocking task, which
/// could cause the other tasks to miss their deadlines if the blocking task
/// unblocks before its pending job expires.
///
/// Instead of immediately removing a blocking task's demand from the processor's
/// total demand, the task's demand removal is deferred. The demand is reserved
/// until it is certain that the commitment to the pending job has expired or
/// migrated to a different processor.
///
/// ## Bandwidth Reservation
///
/// The robust BR algorithm splits total processor demand into two demand pools:
///
///   U_{p,A}: The sum of demands for the set A of actively runnable tasks.
///   U_{p,D}: The sum of demands for the set D of recently blocked tasks whose
///   pending jobs have not yet expired or migrated to a different processor.
///
/// The required processing rate is then determined by the sum of both pools:
///
///   U_{p,A} + U_{p,D} \le R_{p,\omega}
///
/// Scheduling and power management operations are modified to manage the demand
/// pools:
///
/// ### On Task Block
///
/// Move the demand U_i of task i from the active pool to the deferred pool:
///
///   U_{p,A} = U_{p,A} - U_i
///   U_{p,D} = U_{p,D} + U_i
///
/// Because total demand is unchanged, the required processing rate is also
/// unchanged.
///
/// ### On Task Wakeup
///
/// If the task i still has a pending job (i.e. f_{i,k} has not expired), move
/// its demand from the deferred pool of the last processor p it ran on to the
/// active pool of the processor q it is currently assigned to, which may be the
/// same processor:
///
///   U_{p,D} = U_{p,D} - U_i
///   U_{q,A} = U_{q,A} + U_i
///
/// If the total demand of processor q increases, a higher processing rate may be
/// required.
///
/// ### On Finish Time Expiration
///
/// When blocked task i's finish time f_{i,k} passes, its reservation is no
/// longer needed and is removed from the deferred pool of the last processor p
/// it ran on:
///
///   U_{p,D} = U_{p,D} - U_i
///
/// Because the total demand decreases, a lower processing rate may be viable.
///
/// ### On Demand Change
///
/// If the demand of task i changes while it has a pending job, due to bandwidth
/// inheritance or a profile change, either the active demand pool or the
/// deferred demand pool of its assigned processor p must be updated:
///
///   \delta U_i = U_i' - U_i
///
///   Runnable: U_{p,A} = U_{p,A} + \delta U_i
///   Blocked:  U_{p,D} = U_{p,D} + \delta U_i
///
/// The total demand change may require a change in processing rate.
///
/// ## Challenges
///
/// The BR algorithm is robust in that it guarantees adequate processor bandwidth
/// is available to meet _viable_ workload demands only for as long as absolutely
/// necessary. However, its implementation can increase complexity and lock
/// contention.
///
/// Finish time expiration can be implemented using per-CPU deferred reservation
/// queues, which track all of the blocked tasks that most recently ran on a CPU
/// having pending finish times that have not expired. The deferred reservation
/// queue is similar to a run queue ordered by finish time, and needs to be
/// updated when each deferred reservation expires or when a task blocks,
/// unblocks, migrates, or changes demand. While the demand queue can reuse the
/// same metadata used for run queues, it complicates potential per-thread space
/// savings and increases the potential for cross-processor contention when
/// removing tasks from the deferral queue during the aforementioned thread
/// operations.
///
/// ## A Simplifying Local Approximation
///
/// The challenges of the BR algorithm can be mitigated using a processor-local
/// cache of demand reservations, hence the name Bandwidth Reservation Cache.
/// Like BR, BRC maintains active and deferred reservations that sum to determine
/// the required processing rate. The key difference with BRC is that deferred
/// demand is tracked locally in a limited size cache of recently blocked tasks
/// that is not updated when (most) cross-processor thread operations occur. This
/// can result in higher apparent bandwidth demand than is strictly necessary,
/// but for a limited amount of time.
///
/// BRC maintains an array of (i, f_{i,k}, U_i) entries tracking up to N deferred
/// bandwidth reservations. Updating the active and deferred bandwidth
/// reservations follows a similar process to BR when tasks block and unblock,
/// but avoids changing deferred bandwidth reservations for remote processors in
/// most cases.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct BandwidthReservationCache<const N: usize> {
    pub entries: [BandwidthReservationEntry; N],
    /// The total utilization of the deferred reservations tracked by the cache.
    pub total_deferred_utilization: SchedUtilization,
}

impl<const N: usize> Default for BandwidthReservationCache<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> BandwidthReservationCache<N> {
    #[inline]
    pub const fn new() -> Self {
        const { assert!(N > 0) };
        Self {
            entries: [BandwidthReservationEntry::new(); N],
            total_deferred_utilization: SchedUtilization::from_raw(0),
        }
    }

    #[inline]
    pub const fn total_deferred_utilization(&self) -> SchedUtilization {
        self.total_deferred_utilization
    }

    /// Adds the given thread, finish time, and utilization to the cache. If the
    /// given `tid` is already in the cache, the entry is updated and the previous
    /// utilization is returned.
    ///
    /// Returns the utilization of the reservation that was replaced or evicted
    /// from the cache, if any. This quantity should be removed from the total
    /// effective utilization bookkeeping.
    pub fn add(
        &mut self,
        tid: zx_types::zx_koid_t,
        finish_time: SchedTime,
        utilization: SchedUtilization,
    ) -> SchedUtilization {
        const { assert!(N > 0) };
        debug_assert!(tid != zx_types::ZX_KOID_INVALID);
        debug_assert!(utilization >= SchedUtilization::from_raw(0));

        // If there is an existing entry for the tid replace it, otherwise find the
        // first empty element.
        let mut empty_index: Option<usize> = None;
        for (i, entry) in self.entries.iter_mut().enumerate() {
            if entry.tid == tid {
                let utilization_to_remove = entry.utilization;
                self.total_deferred_utilization += utilization - utilization_to_remove;
                debug_assert!(self.total_deferred_utilization >= SchedUtilization::from_raw(0));

                entry.finish_time = finish_time;
                entry.utilization = utilization;
                return utilization_to_remove;
            }
            if empty_index.is_none() && !entry.is_valid() {
                empty_index = Some(i);
            }
        }

        // If there is an empty element, add the new entry there.
        if let Some(idx) = empty_index {
            self.entries[idx] = BandwidthReservationEntry { finish_time, utilization, tid };
            self.total_deferred_utilization += utilization;
            debug_assert!(self.total_deferred_utilization >= SchedUtilization::from_raw(0));
            return SchedUtilization::from_raw(0);
        }

        // Replace the max finish time when the cache is full. Since N > 0 there will
        // always be at least one element with the max finish time.
        let mut max_idx = 0;
        for i in 1..N {
            if self.entries[i].finish_time > self.entries[max_idx].finish_time {
                max_idx = i;
            }
        }

        if self.entries[max_idx].finish_time > finish_time {
            let utilization_to_remove = self.entries[max_idx].utilization;
            self.total_deferred_utilization += utilization - utilization_to_remove;
            debug_assert!(self.total_deferred_utilization >= SchedUtilization::from_raw(0));

            self.entries[max_idx] = BandwidthReservationEntry { finish_time, utilization, tid };
            return utilization_to_remove;
        }

        utilization
    }

    /// Removes the given `tid` from the cache, if present.
    ///
    /// Returns the utilization of the reservation that was removed from the cache,
    /// if any.
    pub fn remove(&mut self, tid: zx_types::zx_koid_t) -> SchedUtilization {
        for entry in self.entries.iter_mut() {
            if entry.tid == tid {
                let utilization_to_remove = entry.utilization;
                self.total_deferred_utilization -= utilization_to_remove;
                debug_assert!(self.total_deferred_utilization >= SchedUtilization::from_raw(0));

                entry.reset();
                return utilization_to_remove;
            }
        }
        SchedUtilization::from_raw(0)
    }

    /// Removes any expired entries with finish times that are less than or equal
    /// to the given time.
    ///
    /// Returns the total utilization of the reservations that were pruned from the
    /// cache, if any.
    pub fn prune(&mut self, now: SchedTime) -> SchedUtilization {
        let mut utilization_to_remove = SchedUtilization::from_raw(0);
        for entry in self.entries.iter_mut() {
            if entry.is_valid() && entry.finish_time <= now {
                utilization_to_remove += entry.utilization;
                self.total_deferred_utilization -= entry.utilization;
                debug_assert!(self.total_deferred_utilization >= SchedUtilization::from_raw(0));

                entry.reset();
            }
        }
        utilization_to_remove
    }

    /// Clears all entries in the cache.
    ///
    /// Returns the total utilization of reservations that were cleared from the
    /// cache, if any.
    pub fn clear(&mut self) -> SchedUtilization {
        let utilization_to_remove = self.total_deferred_utilization;
        self.total_deferred_utilization = SchedUtilization::from_raw(0);
        for entry in self.entries.iter_mut() {
            entry.reset();
        }
        utilization_to_remove
    }

    /// Returns the minimum of the given time and the minimum finish time of any
    /// active entry in the cache.
    pub fn clamp_to_next_finish_time(&self, mut time: SchedTime) -> SchedTime {
        // Find the minimum valid finish time.
        for entry in self.entries.iter() {
            if entry.is_valid() && entry.finish_time < time {
                time = entry.finish_time;
            }
        }
        time
    }
}

#[repr(C)]
pub struct RustSchedulerState {
    /// The run queue of fair scheduled threads ready to run, but not currently running.
    pub fair_run_queue: RunQueue,

    /// The run queue of deadline scheduled threads ready to run, but not currently running.
    pub deadline_run_queue: RunQueue,

    /// The run queue of critical deadline scheduled threads ready to run, but
    /// not currently running.
    pub critical_deadline_run_queue: RunQueue,

    /// Count of the threads running on this CPU, including threads in the run
    /// queue and the currently running thread. Does not include the idle thread.
    pub runnable_task_count: i32,
    _pad1: [u8; 4],

    /// Total weights of threads running on this CPU, including threads in the
    /// run queue and the currently running thread. Does not include the idle
    /// thread.
    pub weight_total: SchedWeight,

    /// The value of `weight_total` when the current thread was scheduled.
    /// Provides a reference for determining whether the total weights changed
    /// since the last reschedule.
    pub scheduled_weight_total: SchedWeight,

    /// Pointer to the thread actively running on this CPU.
    pub active_thread: *mut Thread,

    pub fair_period: SchedDuration,
    pub reciprocal_fair_period: SchedUtilization,

    /// The system time since the last update to the global virtual time.
    pub last_update_time_ns: SchedTime,

    /// The system time that the current time slice started.
    pub start_of_current_time_slice_ns: SchedTime,

    /// The system time that the current thread should be preempted. Initialized to
    /// `ZX_TIME_INFINITE` to pass the assertion `now < target_preemption_time_ns` (or
    /// else the current time slice is expired) on the first entry into the
    /// scheduler.
    pub target_preemption_time_ns: SchedTime,

    /// The sum of the expected runtimes of all active threads on this CPU. This
    /// value is an estimate of the average queuing time for this CPU, given the
    /// current set of active threads.
    pub total_expected_runtime_ns: SchedDuration,

    /// The CPU this scheduler instance is associated with.
    /// NOTE: This member is not initialized to prevent clobbering the value set
    /// by `sched_early_init()`, which is called before the global ctors that
    /// initialize the rest of the members of this class.
    // TODO(eieio): Figure out a better long-term solution to determine which
    // CPU is associated with each instance of this class. This is needed by
    // non-static methods that are called from arbitrary CPUs, namely `insert()`.
    pub this_cpu: CpuNum,
    _pad2: [u8; 4],

    /// The index of the logical cluster this CPU belongs to. CPUs with the same
    /// logical cluster index have the best chance of good cache affinity with
    /// respect to load distribution decisions.
    pub cluster: usize,

    /// Flag indicating that this `Scheduler`'s CPU is in the process of becoming
    /// inactive. When true, threads who have a migration function which must be
    /// run on this CPU are permitted to join this CPU's ready queue, even if the
    /// CPU has already been flagged as inactive. The thread who is in charge of
    /// deactivating the CPU will make sure to run the migration function of the
    /// thread on this CPU before finishing the deactivation process.
    pub cpu_deactivating: core::sync::atomic::AtomicBool,
    _pad3: [u8; 7],

    // Values exported for lock-free access across CPUs. These are mirrors of the
    // members of the same name without the `exported_` prefix. This avoids
    // unnecessary atomic loads when updating the values using arithmetic
    // operations on the local CPU. These values are atomically readonly to other
    // CPUs.
    // TODO(eieio): Look at cache line alignment for these members to optimize
    // cache performance.
    /// The lock-free value of the estimated queue time for the CPU this
    /// scheduler is associated with.
    pub exported_queue_time_ns: RelaxedAtomicI64,
    /// The lock-free value of the estimated deadline utilization for the
    /// CPU this scheduler is associated with.
    pub exported_deadline_utilization: RelaxedAtomicI64,
    /// The lock-free value of the estimated deadline utilization for the
    /// CPU, clamped to the current rate limits, for the CPU this scheduler is
    /// associated with.
    pub exported_clamped_deadline_utilization: RelaxedAtomicI64,
    /// The lock-free value of the estimated fair utilization for the
    /// CPU this scheduler is associated with.
    pub exported_fair_utilization: RelaxedAtomicI64,
    /// The lock-free value of the estimated fair utilization for the
    /// CPU, clamped to the current rate limits, for the CPU this scheduler is
    /// associated with.
    pub exported_clamped_fair_utilization: RelaxedAtomicI64,
    /// The lock-free value of the total estimated utilization (deadline + fair)
    /// for the CPU, clamped to the current rate limits, for the CPU this scheduler
    /// is associated with.
    pub exported_clamped_total_utilization: RelaxedAtomicI64,
    /// The lock-free value of the processing rate for the CPU this
    /// scheduler is associated with.
    pub exported_processing_rate: RelaxedAtomicI64,
    /// The lock-free value of the max processing rate for the CPU this
    /// scheduler is associated with.
    pub exported_max_processing_rate: RelaxedAtomicI64,

    /// The thread which ran just before this thread was scheduled. Used by
    /// `lock_handoff_internal` to release the previous thread's lock after a
    /// context switch operation has fully completed.
    pub previous_thread: core::sync::atomic::AtomicPtr<Thread>,

    /// The total critical deadline utilization of threads associated with this CPU.
    pub critical_deadline_utilization: SchedUtilization,

    pub active_thread_weight: SchedWeight,

    /// The affine transform for converting between monotonic and variable
    /// timelines for the fair run queue.
    pub fair_affine_transform: Affine,

    /// Accumulates the time spent in an idle state since the last reschedule.
    pub processor_idle_time_ns: RelaxedAtomicI64,

    /// Track bandwidth reservations for up to 8 recently blocked threads.
    pub bandwidth_reservation_cache: BandwidthReservationCache<8>,

    /// Encapsulates the CPU energy model and power level control interface state
    /// for energy aware scheduling.
    pub power_level_control: PowerLevelControlState,
}

const CPP_SCHEDULER_TAIL_BYTES: usize =
    core::mem::size_of::<percpu_bindings::Scheduler>() - core::mem::size_of::<RustSchedulerState>();

/// Implements fair and deadline scheduling algorithms and manages the associated
/// per-CPU state.
#[repr(C)]
pub struct Scheduler {
    pub state: core::cell::UnsafeCell<RustSchedulerState>,
    _cpp_fields: core::cell::UnsafeCell<core::mem::MaybeUninit<[u8; CPP_SCHEDULER_TAIL_BYTES]>>,
}

// Affine layout assertions
zr::static_assert!(core::mem::size_of::<Affine>() == 24);
zr::static_assert!(core::mem::align_of::<Affine>() == 8);
zr::static_assert!(core::mem::offset_of!(Affine, monotonic_reference_time) == 0);
zr::static_assert!(core::mem::offset_of!(Affine, variable_reference_time) == 8);
zr::static_assert!(core::mem::offset_of!(Affine, slope) == 16);

// PowerLevelUpdateRequest layout assertions
zr::static_assert!(core::mem::size_of::<PowerLevelUpdateRequest>() == 32);
zr::static_assert!(core::mem::align_of::<PowerLevelUpdateRequest>() == 8);
zr::static_assert!(core::mem::offset_of!(PowerLevelUpdateRequest, domain_id) == 0);
zr::static_assert!(core::mem::offset_of!(PowerLevelUpdateRequest, target_id) == 4);
zr::static_assert!(core::mem::offset_of!(PowerLevelUpdateRequest, control) == 8);
zr::static_assert!(core::mem::offset_of!(PowerLevelUpdateRequest, control_argument) == 16);
zr::static_assert!(core::mem::offset_of!(PowerLevelUpdateRequest, options) == 24);

// PowerLevelControlState layout assertions
zr::static_assert!(core::mem::size_of::<PowerLevelControlState>() == 104);
zr::static_assert!(core::mem::align_of::<PowerLevelControlState>() == 8);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, processing_rate) == 0);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, processing_rate_reciprocal) == 8);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, updated_processing_rate) == 16);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, default_processing_rate) == 24);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, max_processing_rate) == 32);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, processing_rate_limit_min) == 40);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, processing_rate_limit_max) == 48);
zr::static_assert!(
    core::mem::offset_of!(PowerLevelControlState, active_power_coefficient_nw) == 56
);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, pending_update_request) == 64);
zr::static_assert!(core::mem::offset_of!(PowerLevelControlState, has_pending_update_request) == 96);

// BandwidthReservationCache layout assertions
zr::static_assert!(core::mem::size_of::<BandwidthReservationEntry>() == 24);
zr::static_assert!(core::mem::align_of::<BandwidthReservationEntry>() == 8);
zr::static_assert!(core::mem::size_of::<BandwidthReservationCache<8>>() == 200);
zr::static_assert!(core::mem::align_of::<BandwidthReservationCache<8>>() == 8);

// RustSchedulerState layout assertions
zr::static_assert!(core::mem::size_of::<RustSchedulerState>() == 624);
zr::static_assert!(core::mem::align_of::<RustSchedulerState>() == 8);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, fair_run_queue) == 0);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, deadline_run_queue) == 32);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, critical_deadline_run_queue) == 64);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, runnable_task_count) == 96);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, weight_total) == 104);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, scheduled_weight_total) == 112);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, active_thread) == 120);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, fair_period) == 128);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, reciprocal_fair_period) == 136);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, last_update_time_ns) == 144);
zr::static_assert!(
    core::mem::offset_of!(RustSchedulerState, start_of_current_time_slice_ns) == 152
);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, target_preemption_time_ns) == 160);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, total_expected_runtime_ns) == 168);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, this_cpu) == 176);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, cluster) == 184);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, cpu_deactivating) == 192);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, exported_queue_time_ns) == 200);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, exported_deadline_utilization) == 208);
zr::static_assert!(
    core::mem::offset_of!(RustSchedulerState, exported_clamped_deadline_utilization) == 216
);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, exported_fair_utilization) == 224);
zr::static_assert!(
    core::mem::offset_of!(RustSchedulerState, exported_clamped_fair_utilization) == 232
);
zr::static_assert!(
    core::mem::offset_of!(RustSchedulerState, exported_clamped_total_utilization) == 240
);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, exported_processing_rate) == 248);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, exported_max_processing_rate) == 256);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, previous_thread) == 264);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, critical_deadline_utilization) == 272);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, active_thread_weight) == 280);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, fair_affine_transform) == 288);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, processor_idle_time_ns) == 312);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, bandwidth_reservation_cache) == 320);
zr::static_assert!(core::mem::offset_of!(RustSchedulerState, power_level_control) == 520);

zr::static_assert!(core::mem::offset_of!(Scheduler, state) == 0);
zr::static_assert!(
    core::mem::size_of::<Scheduler>() == core::mem::size_of::<percpu_bindings::Scheduler>()
);
zr::static_assert!(
    core::mem::align_of::<Scheduler>() == core::mem::align_of::<percpu_bindings::Scheduler>()
);

/// Returns a tracing string ref for the given placement value.
fn placement_to_str(placement: Placement) -> &'static ktrace_rs::InternedString {
    match placement {
        Placement::Insertion => PLACEMENT_INSERTION,
        Placement::Adjustment => PLACEMENT_ADJUSTMENT,
        Placement::Preemption => PLACEMENT_PREEMPTION,
        Placement::Migration => PLACEMENT_MIGRATION,
        Placement::Association => PLACEMENT_ASSOCIATION,
    }
}

impl Scheduler {
    /// Returns a new flow id when flow tracing is enabled, zero otherwise.
    fn next_flow_id(&self) -> u64 {
        if local_ktrace_level_enabled!(FLOW) { flow_id::generate() } else { 0 }
    }

    /// Asserts that a given thread belongs to `self` `Scheduler` instance.
    /// Requires holding the scheduler's `queue_lock`.
    ///
    /// # Safety
    /// The caller must ensure that `thread` is a valid pointer and `queue_lock_` is held.
    pub unsafe fn assert_in_scheduler(&self, thread: *const Thread) {
        // Evaluate nothing in release builds.
        if cfg!(debug_assertions) {
            // SAFETY: `thread` is valid and `queue_lock_` is held, which synchronizes
            // access to `curr_cpu`, `stolen_by`, and `active_thread`.
            let (curr_cpu, stolen_by, active_thread) = unsafe {
                (
                    (*cpp_thread_get_scheduler_state(thread.cast_mut())).curr_cpu,
                    (*cpp_thread_get_queue_state(thread.cast_mut())).stolen_by,
                    (*self.state.get()).active_thread,
                )
            };
            // SAFETY: `this_cpu` is initialized during early boot.
            let this_cpu = unsafe { self.this_cpu() };
            // TODO(johngro): Come back here and remove the `thread == active_thread`
            // clause once other system-wide invariants have been cleaned up. See
            // https://fxbug.dev/340551498 for details.
            debug_assert!(
                curr_cpu == this_cpu || stolen_by == this_cpu || thread == active_thread,
                "Thread {} expected to be a member of scheduler {}, but curr_cpu says {}",
                // SAFETY: `thread` is valid.
                unsafe { cpp_thread_get_tid(thread) },
                this_cpu,
                curr_cpu
            );
        }
    }

    /// It would be nice to be able to make a stronger assertion here (that the
    /// thread was not just in a run queue, but the specific run queue we expect it
    /// to be in), but unfortunately, there is no inexpensive way of doing that.
    ///
    /// Thankfully, most of the time that we need to invoke an assertion like this,
    /// it is pretty obvious from the context that the assertion is true. It might
    /// be reasonable to (someday) consider removing even this weak runtime check,
    /// and just leaving this as a static analysis workaround, and nothing more.
    ///
    /// # Safety
    /// The caller must ensure that `thread` is a valid pointer and `queue_lock_` is held.
    pub unsafe fn assert_in_run_queue(&self, thread: *const Thread) {
        // Evaluate nothing in release builds.
        if cfg!(debug_assertions) {
            // SAFETY: `thread` is valid and `queue_lock_` is held, which synchronizes
            // access to `curr_cpu` and the run queue node state.
            let (curr_cpu, in_queue) = unsafe {
                (
                    (*cpp_thread_get_scheduler_state(thread.cast_mut())).curr_cpu,
                    (*cpp_thread_get_queue_state(thread.cast_mut())).in_queue(),
                )
            };
            // SAFETY: `this_cpu` is initialized during early boot.
            let this_cpu = unsafe { self.this_cpu() };
            debug_assert!(
                curr_cpu == this_cpu && in_queue,
                "Thread {} expected to be in run queue of scheduler {}, curr_cpu={}, in_queue={}",
                // SAFETY: `thread` is valid.
                unsafe { cpp_thread_get_tid(thread) },
                this_cpu,
                curr_cpu,
                in_queue
            );
        }
    }

    /// Unconditionally perform an expensive check of as many scheduler invariants
    /// as we can. Usually, this is not called directly; the `validate_invariants`
    /// version (which is gated on a default-off build option) is preferred.
    ///
    /// # Safety
    /// The caller must ensure that `queue_lock_` is held and that a
    /// `ChainLockTransaction` is active on the current CPU (the C++ equivalent of
    /// holding the `chainlock_transaction_token` capability).
    pub unsafe fn validate_invariants_unconditional(&self) {
        let observe_fair_thread = |t: *const Thread| {
            // We are holding the scheduler's queue lock, and observing threads which
            // are either active, or members of our queues, so it is OK for us to
            // read a thread's effective profile discipline, even though we don't
            // explicitly hold the thread's lock. Other fields of the effective
            // profile (the dirty flags) may be written by a CPU holding only the
            // thread's lock, so no reference to the profile is formed here.
            //
            // SAFETY: `t` is a valid thread which is active in, or queued on, this
            // scheduler, and `queue_lock_` is held, which keeps its effective
            // profile weight stable.
            let discipline = unsafe {
                EffectiveProfile::discipline_raw(
                    &raw const (*cpp_thread_get_scheduler_state(t.cast_mut())).effective_profile,
                )
            };
            assert!(
                discipline == SchedDiscipline::Fair,
                "Fair thread {} has non-fair effective profile",
                // SAFETY: `t` is valid.
                unsafe { cpp_thread_get_tid(t) }
            );

            // If we have dirty tracking enabled for effective profiles (only enabled in
            // builds with extra scheduler state validation enabled) we can try to make
            // some extra checks.
            //
            // 1) If we know that the current base profile is clean, we can assert that
            //    the base profile is fair.
            // 2) If we know that the current IPVs are clean, we can assert that the IPV
            //    utilization is zero.
            //
            // In order to safely observe these things, however, we need to be holding
            // the thread's lock. Threads can change their base profile or IPVs (and
            // the flags which indicate if they are dirty or nor) while only holding
            // their lock. They do not need to be holding the scheduler's queue lock if
            // the thread happens to be running or ready.
            //
            // So, _try_ to get the thread's lock if we don't already hold it, and if we
            // succeed, go ahead a perform our checks. If we fail to get the lock for
            // any reason, just skip the checks this time.
            #[cfg(scheduler_extra_invariant_validation)]
            {
                // Requires `t`'s lock to be held by the current CPU.
                let extra_checks = |t: *const Thread| {
                    // SAFETY: `t` is valid and its lock is held by the current CPU (it
                    // was either already held, or acquired below), so the base profile,
                    // inherited profile values, and dirty flags cannot change while this
                    // reference is live. The reference is formed only after the lock is
                    // held.
                    let ss = unsafe { &*cpp_thread_get_scheduler_state(t.cast_mut()) };
                    let ep = &ss.effective_profile;
                    let bp = &ss.base_profile;
                    let ipv = &ss.inherited_profile_values;

                    if (ep.dirty_flags & EffectiveProfile::DIRTY_FLAG_BASE_DIRTY) == 0 {
                        assert!(
                            bp.is_fair(),
                            "Fair thread {} has clean, but non-fair, base profile",
                            // SAFETY: `t` is valid.
                            unsafe { cpp_thread_get_tid(t) }
                        );
                    }
                    if (ep.dirty_flags & EffectiveProfile::DIRTY_FLAG_INHERITED_DIRTY) == 0 {
                        assert!(
                            ipv.uncapped_utilization == SchedUtilization::from_raw(0),
                            "Fair thread {} has clean IPV, but non-zero inherited utilization ({})",
                            // SAFETY: `t` is valid.
                            unsafe { cpp_thread_get_tid(t) },
                            ipv.uncapped_utilization.raw_value()
                        );
                    }
                };

                // SAFETY: The caller guarantees that a `ChainLockTransaction` is active
                // on the current CPU.
                let token = unsafe { ChainLockTransaction::assert_active() };
                // SAFETY: `t` is valid, and cannot be destroyed while it is active in,
                // or queued on, this scheduler, whose `queue_lock_` we hold, so its
                // embedded lock outlives this reference.
                let lock = unsafe { &*thread_get_lock(t) };
                if lock.is_held(&token) {
                    extra_checks(t);
                } else if lock.try_acquire_with_options(
                    AllowFinalized::Yes,
                    RecordBackoff::Yes,
                    &token,
                ) {
                    extra_checks(t);
                    lock.release(&token);
                }
            }
        };

        let observe_deadline_thread = |t: *const Thread| {
            // See above for the locking rules for accessing the pieces of effective
            // profile.
            //
            // SAFETY: See `observe_fair_thread`.
            let discipline = unsafe {
                EffectiveProfile::discipline_raw(
                    &raw const (*cpp_thread_get_scheduler_state(t.cast_mut())).effective_profile,
                )
            };
            assert!(
                discipline == SchedDiscipline::Deadline,
                "Deadline thread {} has non-deadline effective profile",
                // SAFETY: `t` is valid.
                unsafe { cpp_thread_get_tid(t) }
            );

            #[cfg(scheduler_extra_invariant_validation)]
            {
                // Requires `t`'s lock to be held by the current CPU.
                let extra_checks = |t: *const Thread| {
                    // SAFETY: See `observe_fair_thread`.
                    let ss = unsafe { &*cpp_thread_get_scheduler_state(t.cast_mut()) };
                    let ep = &ss.effective_profile;
                    let bp = &ss.base_profile;
                    let ipv = &ss.inherited_profile_values;

                    if ep.dirty_flags == EffectiveProfile::DIRTY_FLAG_CLEAN {
                        assert!(
                            bp.is_deadline()
                                || (ipv.uncapped_utilization > SchedUtilization::from_raw(0)),
                            "Deadline thread {} has a clean effective profile, but neither a \
                             deadline base profile ({}), nor a non-zero inherited utilization ({})",
                            // SAFETY: `t` is valid.
                            unsafe { cpp_thread_get_tid(t) },
                            if bp.is_fair() { "Fair" } else { "Deadline" },
                            ipv.uncapped_utilization.raw_value()
                        );
                    }
                };

                // SAFETY: See `observe_fair_thread`.
                let token = unsafe { ChainLockTransaction::assert_active() };
                // SAFETY: See `observe_fair_thread`.
                let lock = unsafe { &*thread_get_lock(t) };
                if lock.is_held(&token) {
                    extra_checks(t);
                } else if lock.try_acquire_with_options(
                    AllowFinalized::Yes,
                    RecordBackoff::Yes,
                    &token,
                ) {
                    extra_checks(t);
                    lock.release(&token);
                }
            }
        };

        // SAFETY: `queue_lock_` is held, so the run queues are stable while we
        // iterate them and the observers only read thread state.
        for t in unsafe { &*self.fair_run_queue() }.iter() {
            observe_fair_thread(t);
        }

        // SAFETY: As above.
        for t in unsafe { &*self.critical_deadline_queue() }.iter() {
            observe_deadline_thread(t);
        }

        // SAFETY: As above.
        for t in unsafe { &*self.deadline_queue() }.iter() {
            observe_deadline_thread(t);
        }

        // SAFETY: `queue_lock_` is held, synchronizing access to `active_thread`.
        let active_thread = unsafe { (*self.state.get()).active_thread };
        assert!(!active_thread.is_null());
        // SAFETY: `active_thread` is non-null and is active on this scheduler, so
        // reading its effective profile discipline under `queue_lock_` is safe (see
        // above).
        let active_discipline = unsafe {
            EffectiveProfile::discipline_raw(
                &raw const (*cpp_thread_get_scheduler_state(active_thread)).effective_profile,
            )
        };
        if active_discipline == SchedDiscipline::Fair {
            observe_fair_thread(active_thread);
        } else {
            assert!(active_discipline == SchedDiscipline::Deadline);
            observe_deadline_thread(active_thread);
        }
    }

    /// Performs an expensive check of scheduler invariants when the
    /// `scheduler_extra_invariant_validation` build option is enabled, and
    /// nothing otherwise.
    ///
    /// # Safety
    /// See `validate_invariants_unconditional`.
    #[inline]
    pub unsafe fn validate_invariants(&self) {
        if cfg!(scheduler_extra_invariant_validation) {
            // SAFETY: Forwarded from the caller.
            unsafe { self.validate_invariants_unconditional() };
        }
    }

    /// Updates the thread's start/finish times and capacity if the current
    /// activation has expired.
    ///
    /// # Safety
    /// `thread` must be valid, and the caller must hold both the scheduler's
    /// `queue_lock_` and `thread`'s lock exclusively, which together guard the
    /// `SchedulerState` fields read and written here.
    unsafe fn update_activation(&self, thread: *mut Thread, now: SchedTime) {
        // SAFETY: `thread` is valid and both its lock and `queue_lock_` are held
        // exclusively, so no other reference to its `SchedulerState` is live for
        // the duration of this borrow.
        let state = unsafe { &mut *cpp_thread_get_scheduler_state(thread) };
        debug_assert_eq!(state.state, ThreadState::Ready);
        // SAFETY: `thread` is valid.
        debug_assert!(!unsafe { cpp_thread_is_idle(thread) });

        let update_trace = local_ktrace_begin_scope!(
            DETAILED,
            "update",
            "remaining timeslice before" => state.remaining_time_slice_ns(),
        );

        let ep = &state.effective_profile;

        let is_fair = ep.is_fair();
        // Determine how much time is left before the finish time. This might be less
        // than the remaining time slice or negative if the thread blocked.
        let time_until_finish_time_ns = state.finish_time - now;

        if time_until_finish_time_ns <= 0 || state.remaining_time_slice_ns() <= 0 {
            // Start a new activation and update the period and capacity with the latest
            // fair or deadline bandwidth parameters. Although the fair capacity is
            // determined dynamically during reschedules by calculate_timeslice, use a
            // non-zero initial value here to ensure that multiple successive calls to
            // update_activation do not incorrectly start new activations with start
            // times further into the future.
            //
            // For example, such a succession can occur when a fair thread wakes after
            // its current activation has expired, is re-activated as it enters the run
            // queue, and is later stolen and potentially re-activated again. Both wake
            // and steal operations can occur after a thread's activation has expired
            // and must start a new activation. Using a non-zero value for the new
            // activation's capacity ensures that the second call to update_activation
            // during stealing does not enter this conditional block unless enough time
            // has passed in the run queue for the finish time to expire.
            let period_ns = if is_fair {
                // SAFETY: `queue_lock_` is held.
                unsafe { self.fair_period() }
            } else {
                ep.deadline().deadline_ns
            };

            let capacity_ns = if is_fair { period_ns } else { ep.deadline().capacity_ns };

            let period_finish_ns = state.start_time + period_ns;

            state.start_time = if now >= period_finish_ns { now } else { period_finish_ns };
            state.finish_time = state.start_time + period_ns;
            state.time_slice_ns = capacity_ns;
            state.time_slice_used_ns = SchedDuration::from_raw(0);
            state.activation_count += 1;
        }

        debug_assert!(state.remaining_time_slice_ns() > 0);

        ktrace_rs::end_scope!(
            update_trace,
            "remaining time slice after" => state.remaining_time_slice_ns(),
            "time until finish time" => time_until_finish_time_ns,
            "start time" => state.start_time,
            "finish time" => state.finish_time,
            "effective period" => state.finish_time - state.start_time,
            "eligible" => state.start_time <= now,
        );
    }

    /// Returns the number of the CPU this scheduler instance is associated with.
    ///
    /// # Safety
    /// `this_cpu` must have been initialized by `sched_early_init`.
    unsafe fn this_cpu(&self) -> CpuNum {
        // SAFETY: `this_cpu` is initialized once during early boot before any
        // scheduler operations and is immutable thereafter.
        unsafe { (*self.state.get()).this_cpu }
    }

    /// Returns the system time that the current time slice started.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn start_of_current_time_slice_ns(&self) -> SchedTime {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `start_of_current_time_slice_ns`.
        unsafe { (*self.state.get()).start_of_current_time_slice_ns }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn set_start_of_current_time_slice_ns(&self, t: SchedTime) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `start_of_current_time_slice_ns`.
        unsafe {
            (*self.state.get()).start_of_current_time_slice_ns = t;
        }
    }

    /// Total weights of threads running on this CPU, including threads in the
    /// run queue and the currently running thread. Does not include the idle
    /// thread.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    pub unsafe fn weight_total(&self) -> SchedWeight {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `weight_total`.
        unsafe { (*self.state.get()).weight_total }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    pub unsafe fn set_weight_total(&self, w: SchedWeight) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `weight_total`.
        unsafe { (*self.state.get()).weight_total = w };
    }

    /// The value of `weight_total` when the current thread was scheduled.
    /// Provides a reference for determining whether the total weights changed
    /// since the last reschedule.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    pub unsafe fn scheduled_weight_total(&self) -> SchedWeight {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `scheduled_weight_total`.
        unsafe { (*self.state.get()).scheduled_weight_total }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    pub unsafe fn set_scheduled_weight_total(&self, w: SchedWeight) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `scheduled_weight_total`.
        unsafe { (*self.state.get()).scheduled_weight_total = w };
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn set_active_thread(&self, thread: *mut Thread) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `active_thread`.
        unsafe {
            (*self.state.get()).active_thread = thread;
        }
    }

    /// Returns the system time that the current thread should be preempted.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn target_preemption_time_ns(&self) -> SchedTime {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `target_preemption_time_ns`.
        unsafe { (*self.state.get()).target_preemption_time_ns }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn set_target_preemption_time_ns(&self, t: SchedTime) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `target_preemption_time_ns`.
        unsafe {
            (*self.state.get()).target_preemption_time_ns = t;
        }
    }

    /// # Safety
    /// The caller must ensure that interrupts are disabled, `queue_lock_` is not
    /// already held by the current CPU, and lock ordering invariants are respected.
    unsafe fn lock_queue(&self) {
        // SAFETY: `self` points to a valid `Scheduler` instance, interrupts are
        // disabled, and `queue_lock_` is not already held.
        unsafe { cpp_scheduler_lock_queue(self as *const Scheduler as *mut Scheduler) }
    }

    /// # Safety
    /// The caller must ensure that `queue_lock_` is currently held by the current CPU.
    unsafe fn unlock_queue(&self) {
        // SAFETY: `self` points to a valid `Scheduler` instance with `queue_lock_` held.
        unsafe { cpp_scheduler_unlock_queue(self as *const Scheduler as *mut Scheduler) }
    }

    /// Updates a segment of the CPU stats idle or busy timelines.
    ///
    /// # Critical: Maintain Coherency of Idle and Busy States
    ///
    /// Accurate CPU statistics depend on the atomic coherency of the following:
    /// 1. Time Samples: Current time and interval since last update.
    /// 2. Idle State: CPU flags (`set_idle`, `peek_is_idle`, `peek_idle_mask`).
    /// 3. Stats Members: `cpu_stats` (`last_updated_instant`, `idle_time`,
    ///    `normalized_busy_time`).
    /// 4. Rate: The current rate of the CPU (`processing_rate`).
    ///
    /// ATOMICITY REQUIREMENT:
    /// All reads and writes of these variables must occur within a SINGLE
    /// acquisition of the queue lock. This ensures that external queries (e.g.,
    /// `ZX_INFO_CPU_STATS`) do not see inconsistent values or "negative progress"
    /// during projection.
    ///
    /// STATE TRANSITIONS, WORK STEALING, AND PROCESSING RATE:
    /// If the CPU transitions between idle and busy, or changes rate, stats must be
    /// updated carefully:
    /// 1. Initial Update: Always performed under the first lock acquisition to
    ///    account for time elapsed since the last reschedule.
    /// 2. Transition Update: If the CPU's idle state changes, upon re-acquiring the
    ///    lock, stats MUST be updated again using FRESH time samples.
    /// 3. Processing Rate Update: If the CPU's processing rate changes, upon
    ///    re-acquiring the lock, stats MUST be updated again using FRESH time
    ///    samples.
    ///
    /// Failure to re-sample time after re-acquiring the lock will result in
    /// "negative progress," as other CPUs may have already projected these values
    /// forward while the lock was released.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`, and `this_cpu` must be initialized.
    unsafe fn update_cpu_stats_segment(
        &self,
        now: SchedTime,
        stats_segment_type: CpuStatsSegmentType,
        segment_processing_rate: SchedProcessingRate,
    ) {
        // SAFETY: `this_cpu` is initialized and `queue_lock_` is held, synchronizing
        // writes to `last_updated_instant`, `idle_time`, and `normalized_busy_time`.
        let (last_updated_atomic, idle_time_atomic, busy_time_atomic) = unsafe {
            let stats_ptr = cpp_get_cpu_stats(self.this_cpu());
            let last_updated_atomic = &*core::ptr::addr_of!((*stats_ptr).last_updated_instant)
                .cast::<core::sync::atomic::AtomicI64>();
            let idle_time_atomic = &*core::ptr::addr_of!((*stats_ptr).idle_time)
                .cast::<core::sync::atomic::AtomicI64>();
            let busy_time_atomic = &*core::ptr::addr_of!((*stats_ptr).normalized_busy_time)
                .cast::<core::sync::atomic::AtomicI64>();
            (last_updated_atomic, idle_time_atomic, busy_time_atomic)
        };

        let last_updated_raw = last_updated_atomic.load(core::sync::atomic::Ordering::Relaxed);
        let segment_runtime = now - SchedTime::from_raw(last_updated_raw);
        debug_assert!(
            segment_runtime >= SchedDuration::from_raw(0),
            "now={} last_updated_instant={} segment_runtime={}",
            now.raw_value(),
            last_updated_raw,
            segment_runtime.raw_value()
        );

        last_updated_atomic.store(now.raw_value(), core::sync::atomic::Ordering::Relaxed);

        if stats_segment_type == CpuStatsSegmentType::Idle {
            idle_time_atomic
                .fetch_add(segment_runtime.raw_value(), core::sync::atomic::Ordering::Relaxed);
        } else {
            let scaled = SchedDuration::from(segment_runtime * segment_processing_rate);
            busy_time_atomic.fetch_add(scaled.raw_value(), core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Computes the estimated energy consumed since the last reschedule and
    /// updates the current thread and CPU energy accumulators.
    ///
    /// # Safety
    /// `current_thread` must point to a valid `Thread`, and the caller must hold
    /// both `queue_lock_` and `current_thread`'s lock exclusively.
    unsafe fn update_estimated_energy_consumption(
        &self,
        current_thread: *mut Thread,
        _mono_time: SchedTime,
        boot_ticks: zx_types::zx_instant_boot_ticks_t,
        actual_runtime_ns: SchedDuration,
    ) {
        // Time in a low-power idle state should only accrue when running the idle
        // thread. Always consume the processor idle time, even if an energy model is
        // not set, to avoid accumulating excessive idle time and triggering the
        // assert when an energy model is finally set.
        // SAFETY: `self.state` and `this_cpu` are valid and `current_thread` is valid.
        let (idle_processor_time_ns, cpu) = unsafe {
            (
                SchedDuration::from_raw((*self.state.get()).processor_idle_time_ns.swap(0)),
                self.this_cpu(),
            )
        };
        debug_assert!(
            idle_processor_time_ns <= actual_runtime_ns,
            "idle_processor_time_ns={} actual_runtime_ns={} cpu={} current_thread={}",
            idle_processor_time_ns.raw_value(),
            actual_runtime_ns.raw_value(),
            cpu,
            // SAFETY: `current_thread` is a valid pointer per caller contract.
            unsafe { thread_name(current_thread) }.as_str(),
        );

        // SAFETY: `self` is valid and `queue_lock_` is held.
        if unsafe { cpp_scheduler_power_level_control_is_enabled(self) } {
            let trace = local_ktrace_begin_scope!(DETAILED, "update_energy");

            // Subtract any time the processor spent in the low-power idle state from the
            // runtime to ensure that active vs. idle power consumption is attributed
            // correctly. Processors can accumulate both active or idle power consumption,
            // but threads, including the idle power thread, accumulate only active power
            // consumption.
            let active_processor_time_ns = actual_runtime_ns - idle_processor_time_ns;

            // This method can be called from another CPU during PI adjustments. Make sure
            // to update the stats for the CPU the thread is associated with.
            // SAFETY: `cpu` is a valid CPU number.
            let stats_ptr = unsafe { cpp_get_cpu_stats(cpu) };

            // Division is performed in the FastFixed domain, which uses 64-bit
            // intermediates: the kernel has no 128-bit division support.
            type FractionalSeconds = FastFixed<u64, 20>;
            type Energy = Fixed<u64, 0>;

            // The energy consumption over the active interval is computed as:
            //
            // E_active = P_active * dt_active
            //
            // P_active is the power coefficient of the current active power level.
            let active_interval_s: FractionalSeconds =
                FastFixed::from(active_processor_time_ns / sched_s(1));
            // SAFETY: `queue_lock_` is held.
            let active_power_nw =
                unsafe { (*self.state.get()).power_level_control.active_power_coefficient_nw };

            let active_energy_consumption_nj: Energy =
                Fixed::<u64, 0>::from_raw(active_power_nw).mul_to(active_interval_s.into_fixed());

            // SAFETY: `queue_lock_` and `current_thread`'s lock are held exclusively,
            // synchronizing updates to `stats_ptr` and `current_thread`'s `SchedulerState`.
            unsafe {
                (*stats_ptr).active_energy_consumption_nj +=
                    active_energy_consumption_nj.raw_value();
                let state = &mut *cpp_thread_get_scheduler_state(current_thread);
                state.estimated_energy_consumption_nj += active_energy_consumption_nj.raw_value();
                // If the thread is dying, report its active energy consumption after the
                // final update to ensure an accurate estimate based on its full runtime.
                if state.state == ThreadState::Death {
                    ktrace_rs::instant!(
                        "kernel:power",
                        "Thread Energy",
                        "estimated energy consumption (nJ)" =>
                            state.estimated_energy_consumption_nj
                    );
                }
            }

            // TODO(https://fxbug.dev/377583571): Select the correct power coefficient
            // when deeper idle states are implemented. For now the max idle power
            // coefficient corresponds to the most general arch idle state (e.g. WFI,
            // halt).
            if idle_processor_time_ns > SchedDuration::from_raw(0) {
                let idle_interval_s: FractionalSeconds =
                    FastFixed::from(idle_processor_time_ns / sched_s(1));
                // SAFETY: `self` is valid and `queue_lock_` is held.
                let idle_power_nw = unsafe {
                    cpp_scheduler_power_level_control_max_idle_power_coefficient_nw(self)
                };

                let idle_energy_consumption_nj: Energy =
                    Fixed::<u64, 0>::from_raw(idle_power_nw).mul_to(idle_interval_s.into_fixed());
                // SAFETY: `queue_lock_` is held, synchronizing access to `stats_ptr`.
                unsafe {
                    (*stats_ptr).idle_energy_consumption_nj +=
                        idle_energy_consumption_nj.raw_value();
                }
            }

            // SAFETY: `queue_lock_` is held, synchronizing access to `stats_ptr`.
            let total_nj = unsafe {
                (*stats_ptr).active_energy_consumption_nj + (*stats_ptr).idle_energy_consumption_nj
            };
            local_ktrace_counter_timestamp!(
                BANDWIDTH,
                "Energy (nJ)",
                ktrace_rs::InstantBootTicks(boot_ticks),
                cpu as u64,
                "CPU" => total_nj
            );

            ktrace_rs::end_scope!(
                trace,
                "active_processor_time_ns" => active_processor_time_ns.raw_value(),
                "idle_processor_time_ns" => idle_processor_time_ns.raw_value(),
            );
        }
    }
    /// Adjusts the given fair thread's bandwidth and dynamic parameters for
    /// changes in bandwidth demand since it was last scheduled.
    ///
    /// # Safety
    /// `thread` must be valid, and the caller must hold both the scheduler's
    /// `queue_lock_` and `thread`'s lock exclusively, which together guard the
    /// `SchedulerState` fields read and written here.
    unsafe fn adjust_fair_bandwidth(&self, thread: *mut Thread, now: SchedTime) {
        let trace = local_ktrace_begin_scope!(DETAILED, "adjust_bandwidth");

        // Calculate the new time slice before borrowing the thread's state mutably,
        // as `calculate_timeslice` borrows that state itself.
        // SAFETY: Forwarded from the caller.
        let time_slice_ns = unsafe { self.calculate_timeslice(thread) };

        // SAFETY: `thread` is valid and both its lock and `queue_lock_` are held
        // exclusively, so no other reference to its `SchedulerState` is live for
        // the duration of this borrow.
        let state = unsafe { &mut *cpp_thread_get_scheduler_state(thread) };
        debug_assert!(state.effective_profile.is_fair());
        debug_assert!(state.remaining_time_slice_ns() > 0 && state.finish_time > now);

        // Calculate the new remaining time slice from the current remaining time
        // slice and the changes to period and utilization, such that lag is
        // conserved, keeping the same finish time.
        let time_until_finish_time_ns = state.finish_time - now;

        let old_effective_period = state.effective_period();
        let old_utilization = SchedUtilization::from(state.time_slice_ns / old_effective_period);
        // SAFETY: `queue_lock_` is held.
        let reciprocal_fair_period = unsafe { self.reciprocal_fair_period() };

        let new_utilization = SchedUtilization::from(time_slice_ns * reciprocal_fair_period);
        let utilization_delta = new_utilization - old_utilization;

        let old_remaining_time_slice_ns = state.remaining_time_slice_ns();
        let new_remaining_time_slice =
            old_remaining_time_slice_ns + utilization_delta * time_until_finish_time_ns;

        // Update the dynamic parameters, clamping the remaining time slice to the end
        // of the period.
        state.time_slice_ns = time_slice_ns;
        state.time_slice_used_ns =
            time_slice_ns - core::cmp::min(new_remaining_time_slice, time_until_finish_time_ns);

        // SAFETY: `queue_lock_` is held.
        let fair_period = unsafe { self.fair_period() };
        let period_delta = fair_period - old_effective_period;
        state.start_time = state.finish_time - fair_period;

        ktrace_rs::end_scope!(
            trace,
            "r_i" => old_remaining_time_slice_ns,
            "r_i'" => new_remaining_time_slice,
            "r_{i,e}" => state.remaining_time_slice_ns(),
            "dU_i" => utilization_delta,
            "dT_i" => period_delta,
            "d_i" => time_until_finish_time_ns,
            "eligible" => state.start_time <= now,
        );
    }

    /// Updates the total expected runtime estimator with the given delta and exports
    /// the atomic shadow variable for cross-CPU readers. The exported value is scaled
    /// by the relative performance factor of the CPU to account for performance
    /// differences in the estimate.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the scheduler's queue lock is held before calling
    /// this method.
    unsafe fn update_total_expected_runtime(&self, delta_ns: SchedDuration) {
        let state = self.state.get();
        // SAFETY: The queue lock is held by the caller, allowing safe mutable access to the state.
        let total = unsafe {
            (*state).total_expected_runtime_ns += delta_ns;
            (*state).total_expected_runtime_ns
        };
        debug_assert!(total.raw_value() >= 0);
        // SAFETY: The queue lock is held by the caller as required by `scale_up`.
        let scaled_ns = unsafe { self.scale_up(total) };
        // SAFETY: The queue lock is held by the caller, synchronizing access to
        // `exported_queue_time_ns` and demand tracing.
        unsafe {
            (*state).exported_queue_time_ns.store(scaled_ns.raw_value());
            self.trace_estimated_runtime(scaled_ns.raw_value());
            if self.power_level_control_is_enabled() {
                self.trace_fair_demand();
            }
        }
    }

    fn arch_yield() {
        core::hint::spin_loop()
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn set_active_thread_weight(&self, w: SchedWeight) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `active_thread_weight`.
        unsafe {
            (*self.state.get()).active_thread_weight = w;
        }
    }

    /// Count of the threads running on this CPU, including threads in the run
    /// queue and the currently running thread. Does not include the idle thread.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn runnable_task_count(&self) -> i32 {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive mutable access to the run queues and task count.
        unsafe { (*self.state.get()).runnable_task_count }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn set_runnable_task_count(&self, count: i32) {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive mutable access to the run queues and task count.
        unsafe { (*self.state.get()).runnable_task_count = count };
    }

    /// The run queue of critical deadline scheduled threads ready to run, but
    /// not currently running.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn critical_deadline_queue(&self) -> *const RunQueue {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive access to the run queues.
        unsafe { &raw const (*self.state.get()).critical_deadline_run_queue }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn critical_deadline_queue_mut(&self) -> *mut RunQueue {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive access to the run queues.
        unsafe { &raw mut (*self.state.get()).critical_deadline_run_queue }
    }

    /// The run queue of deadline scheduled threads ready to run, but not currently running.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn deadline_queue(&self) -> *const RunQueue {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive access to the run queues.
        unsafe { &raw const (*self.state.get()).deadline_run_queue }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn deadline_queue_mut(&self) -> *mut RunQueue {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive access to the run queues.
        unsafe { &raw mut (*self.state.get()).deadline_run_queue }
    }

    /// The run queue of fair scheduled threads ready to run, but not currently running.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn fair_run_queue(&self) -> *const RunQueue {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive access to the run queues.
        unsafe { &raw const (*self.state.get()).fair_run_queue }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn fair_run_queue_mut(&self) -> *mut RunQueue {
        // SAFETY: The caller must hold the scheduler's queue lock, ensuring
        // exclusive access to the run queues.
        unsafe { &raw mut (*self.state.get()).fair_run_queue }
    }

    /// Returns the total critical deadline utilization of threads associated with this CPU.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn critical_deadline_utilization(&self) -> SchedUtilization {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `critical_deadline_utilization`.
        unsafe { (*self.state.get()).critical_deadline_utilization }
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn set_critical_deadline_utilization(&self, u: SchedUtilization) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `critical_deadline_utilization`.
        unsafe { (*self.state.get()).critical_deadline_utilization = u };
    }

    /// Returns the normalized processing rate of the CPU this scheduler instance is
    /// associated with at its current active power level.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    #[inline]
    unsafe fn processing_rate(&self) -> SchedProcessingRate {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `power_level_control`.
        unsafe { (*self.state.get()).power_level_control.processing_rate }
    }

    /// Returns the reciprocal of the normalized processing rate of the CPU this
    /// scheduler instance is associated with.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    #[inline]
    unsafe fn processing_rate_reciprocal(&self) -> SchedProcessingRate {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `power_level_control`.
        unsafe { (*self.state.get()).power_level_control.processing_rate_reciprocal }
    }

    fn round_1000(v: FastFixed<i64, 31>) -> u64 {
        FastFixed::<i64, 0>::from(v * FastFixed::<i64, 0>::from_raw(1000)).raw_value() as u64
    }

    /// Updates trace counters which track the total number of runnable threads for a CPU.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn trace_total_runnable_threads(&self) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `runnable_task_count`.
        let (this_cpu, runnable_task_count) = unsafe {
            let state = self.state.get();
            ((*state).this_cpu, (*state).runnable_task_count)
        };
        local_ktrace_counter!(
            COUNTER,
            "Queue Length",
            this_cpu as u64,
            "CPU" => runnable_task_count
        );
    }

    /// # Safety
    /// `this_cpu` must have been initialized by `sched_early_init`.
    unsafe fn trace_estimated_runtime(&self, value: i64) {
        // SAFETY: `this_cpu` is initialized during early boot.
        let cpu = unsafe { self.this_cpu() };
        local_ktrace_counter!(
            COUNTER,
            "Estimated Runtime",
            cpu as u64,
            "CPU" => value
        );
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn trace_fair_demand(&self) {
        // SAFETY: The caller holds `queue_lock_`. We read scalar fields first so no
        // Rust reference is live across `cpp_scheduler_get_normalized_utilization`.
        let (this_cpu, total_expected_runtime_ns) = unsafe {
            let state = self.state.get();
            ((*state).this_cpu, (*state).total_expected_runtime_ns)
        };
        let fair_demand =
            SchedUtilization::from(K_RECIPROCAL_DEFAULT_FAIR_PERIOD * total_expected_runtime_ns);
        // SAFETY: `self` is a valid `Scheduler` and `queue_lock_` is held.
        let deadline_util = unsafe { cpp_scheduler_get_normalized_utilization(self) };
        let total_demand = deadline_util + fair_demand;

        // SAFETY: The caller holds `queue_lock_`, and no FFI calls occur while
        // accessing `power_level_control` or the `exported_*` atomics.
        unsafe {
            let state = self.state.get();
            let plc = &(*state).power_level_control;
            let clamped_fair = plc.clamp_demand(fair_demand);
            let clamped_total = plc.clamp_demand(total_demand);
            (*state).exported_fair_utilization.store(fair_demand.raw_value());
            (*state).exported_clamped_fair_utilization.store(clamped_fair.raw_value());
            (*state).exported_clamped_total_utilization.store(clamped_total.raw_value());
        }

        let latched_ts = core::cell::Cell::new(None);
        let get_ts = || {
            if let Some(ts) = latched_ts.get() {
                ts
            } else {
                let ts = ktrace_rs::KTrace::timestamp();
                latched_ts.set(Some(ts));
                ts
            }
        };
        ktrace_rs::cpu_counter_timestamp!(
            "kernel:sched-demand",
            "Demand:Variable",
            get_ts(),
            this_cpu as u64,
            "CPU" => Self::round_1000(fair_demand)
        );
        ktrace_rs::cpu_counter_timestamp!(
            "kernel:sched-demand",
            "Demand:Total",
            get_ts(),
            this_cpu as u64,
            "CPU" => Self::round_1000(total_demand)
        );
    }

    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn trace_deadline_utilization(&self, utilization: SchedUtilization) {
        // SAFETY: The caller holds `queue_lock_`, and no FFI calls occur while
        // reading `total_expected_runtime_ns` or `power_level_control` and storing
        // to `exported_clamped_total_utilization`.
        let (this_cpu, total_demand) = unsafe {
            let state = self.state.get();
            let fair_demand = K_RECIPROCAL_DEFAULT_FAIR_PERIOD * (*state).total_expected_runtime_ns;
            let total_demand = utilization + fair_demand;
            let clamped_total = (*state).power_level_control.clamp_demand(total_demand);
            (*state).exported_clamped_total_utilization.store(clamped_total.raw_value());
            ((*state).this_cpu, total_demand)
        };

        let latched_ts = core::cell::Cell::new(None);
        let get_ts = || {
            if let Some(ts) = latched_ts.get() {
                ts
            } else {
                let ts = ktrace_rs::KTrace::timestamp();
                latched_ts.set(Some(ts));
                ts
            }
        };
        ktrace_rs::cpu_counter_timestamp!(
            "kernel:sched-demand",
            "Demand:Constant",
            get_ts(),
            this_cpu as u64,
            "CPU" => Self::round_1000(utilization)
        );
        ktrace_rs::cpu_counter_timestamp!(
            "kernel:sched-demand",
            "Demand:Total",
            get_ts(),
            this_cpu as u64,
            "CPU" => Self::round_1000(total_demand)
        );

        // SAFETY: `self` is a valid `Scheduler` and `queue_lock_` is held.
        // `cpp_scheduler_power_level_control_domain_id` is only called when
        // `cpp_scheduler_power_level_control_is_enabled` returns true.
        if unsafe { cpp_scheduler_power_level_control_is_enabled(self) } {
            let domain_id = unsafe { cpp_scheduler_power_level_control_domain_id(self) };
            let domain_util =
                unsafe { cpp_scheduler_power_level_control_total_normalized_utilization(self) };
            local_ktrace_counter_timestamp!(
                BANDWIDTH,
                "Demand:Constant",
                get_ts(),
                domain_id as u64,
                "Domain" => Self::round_1000(domain_util)
            );
        }
    }

    /// # Safety
    /// `this_cpu` must have been initialized by `sched_early_init`.
    unsafe fn trace_processing_rate(
        &self,
        rate: SchedProcessingRate,
        boot_ticks: zx_types::zx_instant_boot_ticks_t,
    ) {
        // SAFETY: `this_cpu` is initialized during early boot.
        let cpu = unsafe { self.this_cpu() };
        ktrace_rs::cpu_counter_timestamp!(
            "kernel:power",
            "Rate",
            ktrace_rs::InstantBootTicks(boot_ticks),
            cpu as u64,
            "CPU" => Self::round_1000(rate)
        );
    }
    /// Removes the given `tid` from this scheduler's bandwidth reservation cache.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    unsafe fn bandwidth_reservation_cache_remove(&self, tid: u64) -> SchedUtilization {
        // SAFETY: `queue_lock_` is held by the caller, serializing mutable access
        // to `bandwidth_reservation_cache`.
        unsafe { (*self.state.get()).bandwidth_reservation_cache.remove(tid) }
    }

    /// Adds or updates `tid` in this scheduler's bandwidth reservation cache.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    unsafe fn bandwidth_reservation_cache_add(
        &self,
        tid: u64,
        finish_time: SchedTime,
        utilization: SchedUtilization,
    ) -> SchedUtilization {
        // SAFETY: `queue_lock_` is held by the caller, serializing mutable access
        // to `bandwidth_reservation_cache`.
        unsafe {
            (*self.state.get()).bandwidth_reservation_cache.add(tid, finish_time, utilization)
        }
    }

    /// Returns true if power control is enabled.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_` (or call during
    /// single-threaded early boot before the scheduler is active).
    unsafe fn power_level_control_is_enabled(&self) -> bool {
        // SAFETY: `self` is a valid `Scheduler` and `queue_lock_` is held by the caller.
        unsafe { cpp_scheduler_power_level_control_is_enabled(self) }
    }

    /// Returns the target processing rate of this processor (the in-flight
    /// desired rate if a transition is pending, or the current processing rate).
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    #[inline]
    unsafe fn target_processing_rate(&self) -> SchedProcessingRate {
        // SAFETY: `queue_lock_` is held by the caller.
        if unsafe { self.power_level_control_is_enabled() } {
            // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
            unsafe { cpp_scheduler_power_state_target_processing_rate(self) }
        } else {
            // SAFETY: `queue_lock_` is held by the caller.
            unsafe { (*self.state.get()).power_level_control.processing_rate }
        }
    }

    /// Returns the processing rate of the active power level immediately
    /// preceding the target power level (the lower bound of the target power level).
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    #[inline]
    unsafe fn preceding_target_processing_rate(&self) -> SchedProcessingRate {
        // SAFETY: `queue_lock_` is held by the caller.
        if unsafe { self.power_level_control_is_enabled() } {
            // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
            unsafe { cpp_scheduler_power_state_preceding_target_processing_rate(self) }
        } else {
            SchedProcessingRate::from_raw(0)
        }
    }

    /// Returns true if the targeted active power level has an active power level
    /// preceding it (i.e. is not the lowest active power level).
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    #[inline]
    unsafe fn has_preceding_target_power_level(&self) -> bool {
        // SAFETY: `queue_lock_` is held by the caller.
        if unsafe { self.power_level_control_is_enabled() } {
            // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
            unsafe { cpp_scheduler_power_state_has_preceding_target_power_level(self) }
        } else {
            false
        }
    }

    /// Returns the total normalized utilization demand (deadline + fair) of this
    /// processor, clamped to the current performance limits.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    #[inline]
    unsafe fn clamped_total_demand(&self) -> SchedUtilization {
        // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
        let norm_deadline = unsafe { cpp_scheduler_get_normalized_utilization(self) };
        // SAFETY: `queue_lock_` is held by the caller, guarding `total_expected_runtime_ns`
        // and `power_level_control`.
        unsafe {
            let state = self.state.get();
            let fair_demand = K_RECIPROCAL_DEFAULT_FAIR_PERIOD * (*state).total_expected_runtime_ns;
            (*state).power_level_control.clamp_demand(norm_deadline + fair_demand)
        }
    }

    /// Returns true if the clamped demand is above the processing rate of the
    /// target active power level.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    #[inline]
    unsafe fn processing_rate_should_increase(&self) -> bool {
        // SAFETY: `queue_lock_` is held by the caller.
        unsafe { self.clamped_total_demand() > self.target_processing_rate() }
    }

    /// Returns true if the clamped demand is at or below the processing rate of
    /// the preceding target active power level.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    #[inline]
    unsafe fn processing_rate_should_decrease(&self) -> bool {
        // SAFETY: `queue_lock_` is held by the caller.
        let clamped_demand = unsafe { self.clamped_total_demand() };
        // SAFETY: `queue_lock_` is held by the caller.
        unsafe {
            self.has_preceding_target_power_level()
                && clamped_demand <= self.preceding_target_processing_rate()
        }
    }

    /// Returns true if the clamped demand is outside the processing rate range
    /// of the target active power level.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    #[inline]
    unsafe fn processing_rate_should_change(&self) -> bool {
        // SAFETY: `queue_lock_` is held by the caller.
        unsafe { self.processing_rate_should_decrease() || self.processing_rate_should_increase() }
    }

    /// Pends a power level request that will later be flushed by
    /// `send_pending_power_level_request`.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    unsafe fn pend_power_level_request(&self, power_level: u8) -> bool {
        // SAFETY: `this_cpu_` is initialized and immutable after early boot.
        let cpu = unsafe { self.this_cpu() };
        let _trace = local_ktrace_begin_scope!(
            QUEUE,
            "sched_req_power_level",
            "cpu" => cpu,
            "power_level" => power_level as u32,
        );

        // If there is already a pending request, it can be updated without issuing a reschedule,
        // since this update raced with dispatch of the previous request and won.
        let state = self.state.get();
        // SAFETY: `queue_lock_` is held by the caller, guarding `power_level_control`
        // and `power_state_`.
        let had_pending_request =
            unsafe { (*state).power_level_control.has_pending_update_request };
        let mut request = PowerLevelUpdateRequest::default();
        // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
        if unsafe {
            cpp_scheduler_power_state_request_transition(
                self as *const Scheduler as *mut Scheduler,
                cpu,
                power_level,
                &mut request,
            )
        } {
            // SAFETY: `queue_lock_` is held by the caller.
            unsafe {
                (*state).power_level_control.pending_update_request = request;
                (*state).power_level_control.has_pending_update_request = true;
            }
        }
        // SAFETY: `queue_lock_` is held by the caller.
        !had_pending_request && unsafe { (*state).power_level_control.has_pending_update_request }
    }

    /// Pends a power level request compatible with the given rate that will
    /// later be flushed by `send_pending_power_level_request`.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    unsafe fn pend_power_level_request_for_rate(
        &self,
        processing_rate: SchedProcessingRate,
    ) -> bool {
        // SAFETY: `queue_lock_` is held by the caller.
        if unsafe { self.power_level_control_is_enabled() } {
            // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
            let power_level = unsafe {
                cpp_scheduler_power_state_find_active_power_level_for_rate(self, processing_rate)
            };
            if !power_level.is_null() {
                // SAFETY: `power_level` is non-null and points to a valid `PowerLevel`
                // in the domain's model while `queue_lock_` is held.
                let level = unsafe { cpp_power_level_get_level(power_level) };
                // SAFETY: `queue_lock_` is held by the caller.
                return unsafe { self.pend_power_level_request(level) };
            }
        }
        false
    }

    /// Returns the lock-free value of the total estimated utilization (deadline + fair),
    /// clamped to the current rate limits, for the given `cpu`.
    ///
    /// # Safety
    ///
    /// `cpu` must be a valid CPU number (`cpu < PerCpu::processor_count()`) with an
    /// initialized `Scheduler`.
    #[inline]
    pub unsafe fn get_cpu_clamped_total_utilization(cpu: CpuNum) -> SchedUtilization {
        // SAFETY: `cpu` is a valid CPU number with an initialized `Scheduler`.
        let sched = unsafe { &*cpp_scheduler_get(cpu) };
        // SAFETY: `exported_clamped_total_utilization` is an atomic shadow variable
        // safe for lock-free concurrent reads across CPUs.
        SchedUtilization::from_raw(unsafe {
            (*sched.state.get()).exported_clamped_total_utilization.load()
        })
    }

    /// Re-evaluates the power level requirements for processors in the same
    /// domain and pends a power level request if the processing rate should be
    /// adjusted.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    unsafe fn reevaluate_current_power_level(&self) {
        let mut max_clamped_demand = SchedUtilization::from_raw(0);
        // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
        let mut domain_cpus = if unsafe { self.power_level_control_is_enabled() } {
            unsafe { cpp_scheduler_power_state_domain_cpus(self) }
        } else {
            0
        };
        while domain_cpus != 0 {
            let cpu_num = remove_cpu_from_mask(&mut domain_cpus);
            // SAFETY: `cpu_num` comes from `domain_cpus`, so it is a valid CPU number.
            max_clamped_demand = core::cmp::max(max_clamped_demand, unsafe {
                Self::get_cpu_clamped_total_utilization(cpu_num)
            });
        }

        // SAFETY: `queue_lock_` is held by the caller.
        let preceding_rate = unsafe { self.preceding_target_processing_rate() };
        // SAFETY: `queue_lock_` is held by the caller.
        let target_rate = unsafe { self.target_processing_rate() };

        local_ktrace_instant!(
            QUEUE,
            "ReevaluateCurrentPowerLevel",
            "max_clamped_utilization" => max_clamped_demand,
            "preceding_processing_rate" => preceding_rate,
            "processing_rate" => target_rate,
        );

        // SAFETY: `queue_lock_` is held by the caller.
        if (unsafe { self.has_preceding_target_power_level() }
            && max_clamped_demand <= preceding_rate)
            || max_clamped_demand > target_rate
        {
            // SAFETY: `queue_lock_` is held by the caller.
            unsafe {
                self.pend_power_level_request_for_rate(max_clamped_demand);
            }
        }
    }

    /// Sends a pending power level request immediately using the registered
    /// power level controller.
    ///
    /// Because this operation needs to update bookkeeping on other CPUs that are
    /// currently protected by their respective queue locks, the local CPU's
    /// queue lock must be temporarily released, which could result in an
    /// incomplete critical section if local invariants are changed by a remote
    /// processor. Care must be taken to ensure the caller's invariants are
    /// either maintained or re-validated across this operation.
    ///
    /// Returns the mask of CPUs that should be rescheduled immediately.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_` with interrupts disabled,
    /// and any caller invariants must tolerate `queue_lock_` being temporarily released
    /// and re-acquired.
    unsafe fn send_pending_power_level_request(&self) -> CpuMask {
        let state = self.state.get();
        // SAFETY: `queue_lock_` is held by the caller.
        if !unsafe { (*state).power_level_control.has_pending_update_request } {
            return 0;
        }
        // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
        if !unsafe { cpp_scheduler_power_state_is_serving(self) } {
            return 0;
        }
        // Move the pending request into a local copy, leaving the pending request
        // member cleared before releasing the queue lock.
        // SAFETY: `queue_lock_` is held by the caller.
        let request = unsafe {
            (*state).power_level_control.has_pending_update_request = false;
            (*state).power_level_control.pending_update_request
        };

        let trace = local_ktrace_begin_scope!(
            QUEUE,
            "sched_opp_request_fast",
            "domain_id" => request.domain_id,
            "control_argument" => request.control_argument,
        );

        let sched_mut = self as *const Scheduler as *mut Scheduler;
        // SAFETY: `queue_lock_` is currently held by the caller; we temporarily release
        // it around `post_update_request` (which may acquire other CPUs' `queue_lock_`s)
        // and immediately re-acquire it with interrupts remaining disabled.
        let cpus_to_reschedule_mask = unsafe {
            cpp_scheduler_unlock_queue(sched_mut);
            let mask = cpp_scheduler_power_state_post_update_request(self, &request);
            cpp_scheduler_lock_queue(sched_mut);
            mask
        };

        ktrace_rs::end_scope!(
            trace,
            "cpus_to_reschedule_mask" => cpus_to_reschedule_mask,
        );

        cpus_to_reschedule_mask
    }

    /// Increases the processing rate when the required utilization increases
    /// beyond the current rate, accounting for current limits.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    unsafe fn power_level_control_update_rate(&self) {
        // SAFETY: `queue_lock_` is held by the caller.
        if unsafe {
            self.power_level_control_is_enabled()
                && cpp_scheduler_power_state_scheduler_control_enabled(self)
                && self.processing_rate_should_increase()
        } {
            // SAFETY: `queue_lock_` is held by the caller.
            unsafe {
                let demand = self.clamped_total_demand();
                self.pend_power_level_request_for_rate(demand);
            }
        }
    }

    /// Evaluates changing the processing rate when the required utilization
    /// moves outside the bounds of the current power level. All of the
    /// processors in the same domain must be re-evaluated to determine whether
    /// an actual rate change should occur.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    unsafe fn power_level_control_reevaluate_if_needed(&self) {
        // SAFETY: `queue_lock_` is held by the caller.
        if unsafe {
            cpp_scheduler_power_state_scheduler_control_enabled(self)
                && !(*self.state.get()).power_level_control.has_pending_update_request
                && self.processing_rate_should_change()
        } {
            // SAFETY: `queue_lock_` is held by the caller.
            unsafe {
                self.reevaluate_current_power_level();
            }
        }
    }

    /// Updates the scheduler's power level control state after the power domain set
    /// associated with this scheduler has been updated.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    pub unsafe fn on_power_domain_set_updated(
        &self,
        active_rate: SchedProcessingRate,
        domain_max_rate: SchedProcessingRate,
        has_domain: bool,
    ) {
        // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
        let active_power_coefficient_nw =
            unsafe { cpp_scheduler_power_state_active_power_coefficient_nw(self) };
        // SAFETY: `queue_lock_` is held by the caller, serializing mutable access
        // to `power_level_control` and `exported_max_processing_rate`.
        unsafe {
            let state = self.state.get();
            let plc = &mut (*state).power_level_control;
            // Set the active power coefficient from the power state config.
            plc.active_power_coefficient_nw = active_power_coefficient_nw;
            // Clear the request to ensure that a DPC racing with a domain change cannot latch a
            // pending request intended for the previous domain and the new domain ref pointer
            // together.
            plc.pending_update_request = PowerLevelUpdateRequest::default();
            plc.has_pending_update_request = false;
            // Reset the processing rate using either the current rate for the domain or the
            // default.
            if active_rate > SchedProcessingRate::from_raw(0) {
                plc.updated_processing_rate = active_rate;
            } else {
                plc.updated_processing_rate = plc.default_processing_rate;
            }
            // Set the max processing rate for admission control comparisons.
            plc.max_processing_rate =
                if has_domain { domain_max_rate } else { plc.default_processing_rate };
            // Update the cross-processor shadow value of the max processing rate to match the
            // current power domain value.
            (*state)
                .exported_max_processing_rate
                .store(plc.clamped_max_processing_rate().raw_value());
        }
    }

    /// Called by the power level controller server (e.g. the userspace component servicing the
    /// kernel power level control interface) for the domain associated with this scheduler to
    /// acknowledge that a power level request has been completed.
    ///
    /// # Safety
    ///
    /// `self` must be a valid `Scheduler` and its `queue_lock_` must not already be held
    /// by the current CPU.
    pub unsafe fn update_active_power_level(&self, power_level: u8) -> zx_types::zx_status_t {
        let sched_mut = self as *const Scheduler as *mut Scheduler;
        // SAFETY: `self` is valid and `queue_lock_` is not already held by the current CPU.
        let _guard = unsafe { QueueLockIrqSaveGuard::acquire(sched_mut) };

        // SAFETY: `this_cpu_` is initialized and immutable after early boot.
        let cpu = unsafe { self.this_cpu() };
        let trace = local_ktrace_begin_scope!(
            COMMON,
            "UpdateActivePowerLevel",
            "cpu" => cpu,
            "power_level" => power_level as u32,
        );

        let mut active_rate = SchedProcessingRate::from_raw(0);
        // SAFETY: `sched_mut` is valid and `_guard` holds `queue_lock_`.
        let status = unsafe {
            cpp_scheduler_power_state_update_active_power_level(
                sched_mut,
                power_level,
                &mut active_rate,
            )
        };
        // SAFETY: `_guard` holds `queue_lock_`, serializing access to `power_level_control`.
        let (processing_rate, updated_processing_rate) = unsafe {
            let state = self.state.get();
            if status == zx_types::ZX_OK {
                (*state).power_level_control.updated_processing_rate = active_rate;
            }
            (
                Self::round_1000((*state).power_level_control.processing_rate) as u32,
                Self::round_1000((*state).power_level_control.updated_processing_rate) as u32,
            )
        };
        ktrace_rs::end_scope!(
            trace,
            "processing_rate" => processing_rate,
            "updated_processing_rate" => updated_processing_rate,
        );

        status
    }

    /// Sets the processing rate on systems that don't provide an energy model and manage OPPs
    /// exclusively from userspace. User requests are ignored if an energy model is set.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    pub unsafe fn user_set_processing_rate(&self, processing_rate: SchedProcessingRate) {
        debug_assert!(processing_rate > SchedProcessingRate::from_raw(0));
        // SAFETY: `queue_lock_` is held by the caller.
        if !unsafe { self.power_level_control_is_enabled() } {
            // SAFETY: `queue_lock_` is held by the caller.
            unsafe {
                (*self.state.get()).power_level_control.updated_processing_rate = processing_rate;
            }
        }
    }

    /// Sets the processing rate limits for the processor. Returns true if the
    /// current active power level needs to be updated in response to the new
    /// limits.
    ///
    /// The specified `min` is allowed to be greater than `max` for future
    /// latency vs. power tradeoff management, which will be documented when
    /// implemented. Until that time, `max` takes precedence in the case where
    /// `min > max`.
    ///
    /// # Safety
    ///
    /// The caller must hold this scheduler's `queue_lock_`.
    pub unsafe fn user_set_processing_rate_limits(
        &self,
        min: SchedProcessingRate,
        max: SchedProcessingRate,
    ) -> bool {
        let zero = SchedProcessingRate::from_raw(0);
        let one = SchedProcessingRate::from_raw(1 << 31);
        let state = self.state.get();
        // SAFETY: `queue_lock_` is held by the caller, serializing access to
        // `power_level_control` and `exported_max_processing_rate`.
        unsafe {
            (*state).power_level_control.processing_rate_limit_min = min.clamp(zero, one);
            (*state).power_level_control.processing_rate_limit_max = max.clamp(zero, one);

            let clamped_max = (*state).power_level_control.clamped_max_processing_rate();
            (*state).exported_max_processing_rate.store(clamped_max.raw_value());
        }

        // SAFETY: `self` is valid and `queue_lock_` is held by the caller.
        let norm_deadline = unsafe { cpp_scheduler_get_normalized_utilization(self) };
        // SAFETY: `queue_lock_` is held by the caller.
        unsafe {
            let fair_demand = SchedUtilization::from(
                K_RECIPROCAL_DEFAULT_FAIR_PERIOD * (*state).total_expected_runtime_ns,
            );
            let total_demand = norm_deadline + fair_demand;

            let clamped_deadline = (*state).power_level_control.clamp_demand(norm_deadline);
            let clamped_fair = (*state).power_level_control.clamp_demand(fair_demand);
            let clamped_total = (*state).power_level_control.clamp_demand(total_demand);

            (*state).exported_clamped_deadline_utilization.store(clamped_deadline.raw_value());
            (*state).exported_clamped_fair_utilization.store(clamped_fair.raw_value());
            (*state).exported_clamped_total_utilization.store(clamped_total.raw_value());

            self.processing_rate_should_change()
        }
    }

    /// Sets the initial values of the CPU performance scales for this `Scheduler`
    /// instance from topology data during early boot.
    ///
    /// # Safety
    ///
    /// Must be called during early boot before the scheduler is active and before
    /// concurrent access to `self` is possible.
    pub unsafe fn initialize_processing_rate(&self, scale: SchedProcessingRate) {
        // Since this happens early in boot, before the scheduler is actually running,
        // acquiring the queue lock is unnecessary.
        debug_assert!(scale > SchedProcessingRate::from_raw(0));
        // SAFETY: Called during single-threaded early boot initialization.
        debug_assert!(!unsafe { self.power_level_control_is_enabled() });
        // SAFETY: Called during early boot before concurrent access to `self.state`.
        unsafe {
            let state = self.state.get();
            let plc = &mut (*state).power_level_control;
            plc.processing_rate = scale;
            plc.max_processing_rate = scale;
            plc.default_processing_rate = scale;
            plc.updated_processing_rate = scale;
            plc.processing_rate_reciprocal =
                SchedProcessingRate::from(SchedDuration::from_raw(1) / scale);
            (*state).exported_processing_rate.store(scale.raw_value());
            (*state)
                .exported_max_processing_rate
                .store(plc.clamped_max_processing_rate().raw_value());
        }
    }

    /// Gets the performance scales of up to `info.len()` CPUs. Returns the last
    /// values requested by userspace, even if they have not yet taken effect.
    ///
    /// Requires `info.len() <= num CPUs`.
    ///
    /// # Safety
    ///
    /// `info.len()` must not exceed `PerCpu::processor_count()`, and no `queue_lock_`
    /// may already be held by the current CPU.
    pub unsafe fn get_performance_scales(info: &mut [zx_types::zx_cpu_performance_info_t]) {
        let processor_count = PerCpu::processor_count();
        debug_assert!(info.len() <= processor_count);
        for (i, entry) in info.iter_mut().enumerate() {
            let cpu = i as CpuNum;
            // SAFETY: `i < info.len() <= processor_count`, so `cpu` is a valid CPU
            // with an initialized `Scheduler`, and `queue_lock_` is not held.
            let sched_ptr = unsafe { cpp_scheduler_get(cpu) };
            // SAFETY: `sched_ptr` is valid and `queue_lock_` is not already held.
            let _guard = unsafe { QueueLockIrqSaveGuard::acquire(sched_ptr) };
            entry.logical_cpu_number = cpu;
            // SAFETY: `sched_ptr` is valid and `_guard` holds its `queue_lock_`.
            entry.performance_scale = to_user_performance_scale(unsafe {
                (*(*sched_ptr).state.get()).power_level_control.updated_processing_rate
            });
        }
    }

    /// Gets the default performance scales of up to `info.len()` CPUs. Returns the
    /// initial values determined by the system topology, or 1.0 when no topology
    /// is available.
    ///
    /// Requires `info.len() <= num CPUs`.
    ///
    /// # Safety
    ///
    /// `info.len()` must not exceed `PerCpu::processor_count()`, and no `queue_lock_`
    /// may already be held by the current CPU.
    pub unsafe fn get_default_performance_scales(info: &mut [zx_types::zx_cpu_performance_info_t]) {
        let processor_count = PerCpu::processor_count();
        debug_assert!(info.len() <= processor_count);
        for (i, entry) in info.iter_mut().enumerate() {
            let cpu = i as CpuNum;
            // SAFETY: `i < info.len() <= processor_count`, so `cpu` is a valid CPU
            // with an initialized `Scheduler`, and `queue_lock_` is not held.
            let sched_ptr = unsafe { cpp_scheduler_get(cpu) };
            // SAFETY: `sched_ptr` is valid and `queue_lock_` is not already held.
            let _guard = unsafe { QueueLockIrqSaveGuard::acquire(sched_ptr) };
            entry.logical_cpu_number = cpu;
            // SAFETY: `sched_ptr` is valid and `_guard` holds its `queue_lock_`.
            entry.performance_scale = to_user_performance_scale(unsafe {
                (*(*sched_ptr).state.get()).power_level_control.max_processing_rate
            });
        }
    }

    /// Gets the processing limits of up to `limits.len()` CPUs.
    ///
    /// Requires `limits.len() <= num CPUs`.
    ///
    /// # Safety
    ///
    /// `limits.len()` must not exceed `PerCpu::processor_count()`, and no `queue_lock_`
    /// may already be held by the current CPU.
    pub unsafe fn get_processing_limits(limits: &mut [zx_types::zx_cpu_perf_limit_t]) {
        let processor_count = PerCpu::processor_count();
        debug_assert!(limits.len() <= processor_count);
        for (i, entry) in limits.iter_mut().enumerate() {
            let cpu = i as CpuNum;
            // SAFETY: `i < limits.len() <= processor_count`, so `cpu` is a valid CPU
            // with an initialized `Scheduler`, and `queue_lock_` is not held.
            let sched_ptr = unsafe { cpp_scheduler_get(cpu) };
            // SAFETY: `sched_ptr` is valid and `queue_lock_` is not already held.
            let _guard = unsafe { QueueLockIrqSaveGuard::acquire(sched_ptr) };

            entry.logical_cpu_number = cpu;
            entry.limit_type = zx_types::ZX_CPU_PERF_LIMIT_TYPE_RATE;
            // SAFETY: `sched_ptr` is valid and `_guard` holds its `queue_lock_`.
            unsafe {
                let plc = &(*(*sched_ptr).state.get()).power_level_control;
                entry.min = Self::round_1000(plc.processing_rate_limit_min);
                entry.max = Self::round_1000(plc.processing_rate_limit_max);
            }
        }
    }
    /// Updates the total deadline utilization estimator with the given delta and
    /// traces/exports the atomic shadow variables for cross-CPU readers.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the scheduler's queue lock is held before calling
    /// this method to guarantee consistent access to the power level control properties.
    unsafe fn update_total_deadline_utilization(&self, delta: SchedUtilization) {
        // Avoid unnecessary trace counter and atomic variable updates.
        if delta.raw_value() != 0 {
            // SAFETY: FFI call to C++ power state to update the normalized utilization.
            // The queue lock is held by the caller as verified by the safety contract.
            let utilization = unsafe {
                cpp_scheduler_power_level_control_update_normalized_utilization(
                    self as *const Scheduler as *mut Scheduler,
                    delta,
                )
            };
            debug_assert!(utilization.raw_value() >= 0);
            // SAFETY: The queue lock is held by the caller, allowing safe mutable access to the
            // state.
            unsafe {
                let state = self.state.get();
                (*state).exported_deadline_utilization.store(utilization.raw_value());
                let clamped = (*state).power_level_control.clamp_demand(utilization);
                (*state).exported_clamped_deadline_utilization.store(clamped.raw_value());
                self.trace_deadline_utilization(utilization);
            }
        }
    }

    /// Updates the current processing rate, reciprocal, and active power coefficient to the
    /// updated values if a rate update is pending, and exports the atomic shadow variable for
    /// cross-CPU readers. Called by `reschedule_common` after accounting is performed for the
    /// time spent at the previous rate.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the scheduler's queue lock is held before calling
    /// this method to guarantee consistent access to the power level control properties.
    unsafe fn update_processing_rate(&self, boot_ticks: zx_types::zx_instant_boot_ticks_t) -> bool {
        let state = self.state.get();
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `power_level_control`
        // and exported processing rate state.
        unsafe {
            if (*state).power_level_control.is_processing_rate_update_pending() {
                let rate = (*state).power_level_control.updated_processing_rate;
                (*state).power_level_control.processing_rate = rate;
                (*state).power_level_control.processing_rate_reciprocal =
                    SchedProcessingRate::from(SchedDuration::from_raw(1) / rate);
                let coeff = cpp_scheduler_power_state_active_power_coefficient_nw(self);
                (*state).power_level_control.active_power_coefficient_nw = coeff;
                if !self.power_level_control_is_enabled() {
                    (*state).power_level_control.max_processing_rate = rate;
                    let clamped_max = (*state).power_level_control.clamped_max_processing_rate();
                    (*state).exported_max_processing_rate.store(clamped_max.raw_value());
                }
                (*state).exported_processing_rate.store(rate.raw_value());
                self.trace_processing_rate(rate, boot_ticks);
                true
            } else {
                false
            }
        }
    }

    /// Converts a time from the monotonic timeline to the variable timeline.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn monotonic_to_variable(&self, monotonic: SchedTime) -> SchedTime {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `fair_affine_transform`.
        unsafe { (*self.state.get()).fair_affine_transform.monotonic_to_variable(monotonic) }
    }

    /// Converts a time from the variable timeline to the monotonic timeline.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn variable_to_monotonic(&self, variable: SchedTime) -> SchedTime {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `fair_affine_transform`.
        unsafe { (*self.state.get()).fair_affine_transform.variable_to_monotonic(variable) }
    }

    /// Converts a time from the monotonic timeline to the variable timeline in place.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn monotonic_to_variable_in_place(&self, ref_time: &mut SchedTime) {
        // SAFETY: The caller holds `queue_lock_`.
        *ref_time = unsafe { self.monotonic_to_variable(*ref_time) };
    }

    /// Converts a time from the variable timeline to the monotonic timeline in place.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn variable_to_monotonic_in_place(&self, ref_time: &mut SchedTime) {
        // SAFETY: The caller holds `queue_lock_`.
        *ref_time = unsafe { self.variable_to_monotonic(*ref_time) };
    }

    /// The current fair scheduling period.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn fair_period(&self) -> SchedDuration {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to `fair_period`.
        unsafe { (*self.state.get()).fair_period }
    }

    /// The cached reciprocal of the current fair scheduling period.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn reciprocal_fair_period(&self) -> SchedUtilization {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `reciprocal_fair_period`.
        unsafe { (*self.state.get()).reciprocal_fair_period }
    }

    /// Sets the fair period and updates its cached reciprocal.
    ///
    /// # Safety
    /// The caller must ensure that the scheduler's queue lock is held.
    unsafe fn set_fair_period(&self, period: SchedDuration) {
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to
        // `fair_period` and `reciprocal_fair_period`.
        unsafe {
            let state = self.state.get();
            (*state).fair_period = period;
            (*state).reciprocal_fair_period =
                SchedUtilization::from(SchedDuration::from_raw(1) / period);
        }
    }

    /// Evaluates the fair bandwidth demand and updates the period scale, preventing fair bandwidth
    /// time slices from becoming too small, and causing thrashing, by ensuring that threads with
    /// the minimum weight in the queue get at least the minimum capacity.
    ///
    /// To guarantee that a fair bandwidth thread receives at least a minimum capacity, the
    /// following relationship must hold:
    ///
    ///   w_min / w_total >= C_min / T_default
    ///
    /// Where:
    ///
    ///   w_min is the minimum weight of threads in the run queue and the active thread.
    ///   w_total is the total weight of the run queue.
    ///   C_min is the minimum capacity.
    ///   T_default is the default fair task period.
    ///
    /// If this relationship does not hold, a scale factor is applied to expand the fair task period
    /// and ensure that tasks with the minimum weight receive the minimum capacity:
    ///
    ///   s = C_min / T_default * w_total / w_min
    ///
    /// # Safety
    /// The caller must hold this scheduler's `queue_lock_`. On the C++ side
    /// `rust_state_` is `TA_GUARDED(queue_lock_)`, so holding the lock gives this
    /// call exclusive access to the fair run queue and `fair_affine_transform`
    /// for its duration.
    unsafe fn update_fair_bandwidth_period(&self, now: SchedTime) {
        // SAFETY: The caller holds `queue_lock_`, which guards `active_thread_weight`.
        let active_thread_weight = unsafe { (*self.state.get()).active_thread_weight };

        // Returns the minimum weight of the run queue, including the active thread.
        let get_min_weight = || {
            // SAFETY: The caller holds `queue_lock_`, so the fair run queue cannot
            // change while this shared reference is live.
            let fair_run_queue = unsafe { &*self.fair_run_queue() };
            if fair_run_queue.is_empty() {
                return active_thread_weight;
            }
            // Note that `root_cursor()` is only meaningful for a non-empty tree: the root
            // pointer of an empty `WavlTree` is null rather than the sentinel.
            let root = fair_run_queue.root_cursor().get().expect("non-empty run queue has a root");
            // SAFETY: `root` is a live thread enqueued on this scheduler, and `queue_lock_`
            // is held, which guards its `subtree_invariants`.
            unsafe {
                self.assert_in_run_queue(root);
                core::cmp::min(
                    active_thread_weight,
                    (*cpp_thread_get_scheduler_state(core::ptr::from_ref(root).cast_mut()))
                        .subtree_invariants
                        .min_weight,
                )
            }
        };

        let min_weight = get_min_weight();
        // SAFETY: Forwarded from the caller.
        let weight_total = unsafe { self.weight_total() };

        // `fair_affine_transform` is guarded by `queue_lock_`, which the caller holds.
        // Access it through a raw pointer so that no reference is live across the
        // calls to `set_fair_period`.
        let affine = unsafe { &raw mut (*self.state.get()).fair_affine_transform };

        if min_weight < weight_total * K_MIN_FAIR_UTILIZATION {
            let min_utilization = SchedUtilization::from(min_weight / weight_total);
            let slope = Slope::from(min_utilization / K_MIN_FAIR_UTILIZATION);
            let slope = core::cmp::max(slope, Affine::K_MIN_SLOPE);

            // SAFETY: `queue_lock_` is held (see above).
            if unsafe { (*affine).slope } != slope {
                // SAFETY: As above.
                unsafe { (*affine).change_slope_at_monotonic_time(now, slope) };
                let fair_period = SchedDuration::from(K_DEFAULT_FAIR_PERIOD / slope);
                // SAFETY: Forwarded from the caller.
                unsafe { self.set_fair_period(fair_period) };
            }
        } else if unsafe { (*affine).slope } != Affine::K_MAX_SLOPE {
            // SAFETY: `queue_lock_` is held (see above).
            unsafe { (*affine).change_slope_at_monotonic_time(now, Affine::K_MAX_SLOPE) };
            // SAFETY: Forwarded from the caller.
            unsafe { self.set_fair_period(K_DEFAULT_FAIR_PERIOD) };
        }
    }

    /// Returns true if the given thread is critical deadline scheduled.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread`, and the caller must hold either
    /// `queue_lock_` (if `thread` is associated with this scheduler) or at least
    /// shared access to `thread`'s lock.
    unsafe fn is_critical_deadline_thread(&self, thread: *const Thread) -> bool {
        // SAFETY: The caller holds `queue_lock_` or `thread`'s lock, synchronizing
        // access to `effective_profile.critical`. We project through a raw pointer
        // rather than forming `&SchedulerState` because `dirty_flags` may be written
        // under `thread`'s lock alone.
        unsafe {
            let state = cpp_thread_get_scheduler_state(thread.cast_mut());
            (*state).effective_profile.critical
        }
    }
    /// Scales a duration value by the reciprocal of the processing rate.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the scheduler's queue lock is held before calling
    /// this method to guarantee consistent access to the power level control properties.
    unsafe fn scale_up(&self, value: SchedDuration) -> SchedDuration {
        // SAFETY: Forwarded from the caller.
        let reciprocal = unsafe { self.processing_rate_reciprocal() };
        SchedDuration::from(value * reciprocal)
    }

    /// Scales a duration value by the processing rate.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the scheduler's queue lock is held before calling
    /// this method to guarantee consistent access to the power level control properties.
    unsafe fn scale_down(&self, value: SchedDuration) -> SchedDuration {
        // SAFETY: Forwarded from the caller.
        let rate = unsafe { self.processing_rate() };
        SchedDuration::from(value * rate)
    }

    /// Calculates the timeslice of the thread based on the current run queue
    /// state.
    ///
    /// # Safety
    /// `thread` must be valid, and the caller must hold the scheduler's
    /// `queue_lock_` and at least shared access to `thread`'s lock, which guard
    /// the run queue state and `thread`'s effective profile, respectively.
    unsafe fn calculate_timeslice(&self, thread: *const Thread) -> SchedDuration {
        let trace = local_ktrace_begin_scope!(DETAILED, "calculate_timeslice");
        // SAFETY: `thread` is valid and its lock is held at least shared, so its
        // `SchedulerState` is valid for shared access.
        let state = unsafe { &*cpp_thread_get_scheduler_state(thread.cast_mut()) };
        let ep = &state.effective_profile;
        debug_assert!(ep.is_fair());

        // SAFETY: `queue_lock_` is held.
        let weight_total = unsafe { self.weight_total() };
        let fair_utilization = SchedUtilization::from(ep.weight / weight_total);

        // SAFETY: `queue_lock_` is held.
        let fair_period = unsafe { self.fair_period() };
        let time_slice_ns = SchedDuration::from(fair_period * fair_utilization);

        ktrace_rs::end_scope!(
            trace,
            "fair utilization" => fair_utilization,
            "time slice" => time_slice_ns,
            "weight" => ep.weight.raw_value(),
            "total weight" => weight_total.raw_value(),
        );

        time_slice_ns
    }

    /// Returns the time that the next deadline task will become eligible or infinite
    /// if there are no ready deadline tasks.
    ///
    /// # Safety
    /// The caller must hold the scheduler's `queue_lock_`, which guards the
    /// deadline run queues and the queued threads' `SchedulerState`.
    unsafe fn get_next_eligible_time(&self) -> SchedTime {
        // SAFETY: `queue_lock_` is held.
        let critical_deadline_queue = unsafe { &*self.critical_deadline_queue() };
        // SAFETY: `queue_lock_` is held.
        let deadline_queue = unsafe { &*self.deadline_queue() };

        let get_eligible_time = |queue: &RunQueue| {
            queue.front().map_or(SchedTime::from_raw(ZX_TIME_INFINITE), |front| {
                // SAFETY: `front` is in `queue` and `queue_lock_` is held, so its
                // `SchedulerState` is valid for shared access (see the locking note on
                // `SchedulerState`).
                unsafe { (*cpp_thread_get_scheduler_state(front.as_raw())).start_time }
            })
        };

        core::cmp::min(
            get_eligible_time(critical_deadline_queue),
            get_eligible_time(deadline_queue),
        )
    }

    /// Returns the completion time clamped to the start of the earliest deadline
    /// thread that will become eligible in that time frame.
    ///
    /// # Safety
    /// The caller must hold the scheduler's `queue_lock_`.
    unsafe fn clamp_to_deadline(&self, completion_time: SchedTime) -> SchedTime {
        // SAFETY: Forwarded from the caller.
        core::cmp::min(completion_time, unsafe { self.get_next_eligible_time() })
    }

    /// Returns the eligible deadline thread in the run queue with a finish time
    /// earlier than the given reference thread's finish time and criticality, or
    /// null if one does not exist.
    ///
    /// See the note in `find_earliest_eligible_thread` for lifecycle rules for the
    /// returned pointer (if any).
    ///
    /// # Safety
    /// `reference_thread` must be valid, and the caller must hold the scheduler's
    /// `queue_lock_` and at least shared access to `reference_thread`'s lock,
    /// which guard the deadline run queues and the reference thread's
    /// `SchedulerState`, respectively.
    unsafe fn find_earlier_deadline_thread(
        &self,
        eligible_time: SchedTime,
        reference_thread: *const Thread,
    ) -> *mut Thread {
        // The following logic governs how the scheduler finds an earlier deadline
        // thread than the reference thread based on system load:
        //
        // 1. **Scenario A: Feasible Total Demand.** When the combined demand of both
        //    critical and general queues is within CPU capacity:
        //    a. If both queues contain eligible tasks, the scheduler selects the task
        //       with the earliest eligible deadline, regardless of which queue it
        //       resides in.
        //    b. If only one queue has eligible tasks, the earliest eligible deadline
        //       task from that queue is selected.
        // 2. **Scenario B: Infeasible Total Demand (Oversubscription).** When total
        //    demand exceeds capacity, the system prioritizes the critical tier:
        //    a. If the critical deadline queue has eligible tasks, the scheduler
        //       selects the earliest eligible deadline task from this queue
        //       exclusively.
        //    b. The general deadline queue is only serviced if the critical deadline
        //       queue is empty or only has ineligible tasks.
        //
        // If the CPU is oversubscribed, an eligible critical deadline thread takes
        // precedence over any eligible general deadline thread, including the
        // reference thread. Otherwise, the scheduler selects the eligible deadline
        // thread with the earliest deadline, regardless of which type, including the
        // reference thread.
        // SAFETY: `queue_lock_` is held.
        let critical_deadline_queue = unsafe { self.critical_deadline_queue() };
        // SAFETY: `queue_lock_` is held.
        let deadline_queue = unsafe { self.deadline_queue() };

        // SAFETY: `queue_lock_` is held, keeping the run queue and its threads valid.
        let critical_thread = unsafe {
            find_earliest_eligible_thread(&*critical_deadline_queue, eligible_time, accept_all)
        };

        // SAFETY: `self` is valid and `queue_lock_` is held, which guards the power
        // level control state.
        let normalized_utilization = unsafe { cpp_scheduler_get_normalized_utilization(self) };
        let is_oversubscribed = normalized_utilization > K_CPU_UTILIZATION_LIMIT;

        let eligible_thread = if is_oversubscribed && !critical_thread.is_null() {
            critical_thread
        } else {
            // SAFETY: `queue_lock_` is held, keeping the run queue and its threads valid.
            let normal_thread = unsafe {
                find_earliest_eligible_thread(&*deadline_queue, eligible_time, accept_all)
            };
            // SAFETY: Both threads are either null or in this scheduler's run queues,
            // which `queue_lock_` guards.
            unsafe { self.earliest_deadline_thread(critical_thread, normal_thread) }
        };

        let mut selected_thread = core::ptr::null_mut();
        if !eligible_thread.is_null() {
            // The C++ implementation uses MarkHasOwnedThreadAccess here to satisfy the
            // static annotations for shared access to the eligible thread's state.
            // SAFETY: This is acceptable because `queue_lock_` is held and the eligible
            // thread is in a run queue protected by the same lock, so the fields read
            // below cannot be concurrently written (see the locking note on
            // `SchedulerState`).
            let eligible_state = unsafe { &*cpp_thread_get_scheduler_state(eligible_thread) };
            // SAFETY: `reference_thread` is valid and its lock is held at least shared.
            let ref_state =
                unsafe { &*cpp_thread_get_scheduler_state(reference_thread.cast_mut()) };

            let eligible_is_critical = eligible_state.effective_profile.critical;
            let reference_is_critical = ref_state.effective_profile.critical;

            if is_oversubscribed {
                if eligible_is_critical && !reference_is_critical {
                    selected_thread = eligible_thread;
                } else if !eligible_is_critical && reference_is_critical {
                    selected_thread = core::ptr::null_mut();
                } else if eligible_state.finish_time < ref_state.finish_time {
                    selected_thread = eligible_thread;
                }
            } else if eligible_state.finish_time < ref_state.finish_time {
                selected_thread = eligible_thread;
            }

            let critical_state = if eligible_is_critical && reference_is_critical {
                ktrace_rs::resolve_string!("both")
            } else if eligible_is_critical {
                ktrace_rs::resolve_string!("contender")
            } else if reference_is_critical {
                ktrace_rs::resolve_string!("active")
            } else {
                ktrace_rs::resolve_string!("none")
            };
            let selected_str = if !selected_thread.is_null() {
                ktrace_rs::resolve_string!("contender")
            } else {
                ktrace_rs::resolve_string!("active")
            };

            // SAFETY: Both threads are valid for the duration of this call, as above.
            local_ktrace_instant!(
                QUEUE,
                "find_earlier_deadline",
                "is_oversubscribed" => is_oversubscribed,
                "contender_name" => unsafe { thread_name(eligible_thread) }.as_str(),
                "contender_tid" => unsafe { cpp_thread_get_tid(eligible_thread) },
                "contender_finish_delta_ns" => eligible_state.finish_time - eligible_time,
                "contender_start_delta_ns" => eligible_time - eligible_state.start_time,
                "contender_remaining_slice_ns" => eligible_state.remaining_time_slice_ns(),
                "contender_capacity_ns" => eligible_state.time_slice_ns,
                "active_name" => unsafe { thread_name(reference_thread) }.as_str(),
                "active_tid" => unsafe { cpp_thread_get_tid(reference_thread) },
                "active_finish_delta_ns" => ref_state.finish_time - eligible_time,
                "active_start_delta_ns" => eligible_time - ref_state.start_time,
                "active_remaining_slice_ns" => ref_state.remaining_time_slice_ns(),
                "active_capacity_ns" => ref_state.time_slice_ns,
                "critical_state" => critical_state,
                "selected" => selected_str,
            );
        }

        selected_thread
    }

    /// Returns the thread with the earliest finish time. Either or both threads
    /// may be null. If there is a tie between `primary` and `secondary`, `primary`
    /// is returned.
    ///
    /// # Safety
    /// Each non-null thread must be in one of this scheduler's run queues, and the
    /// caller must hold the scheduler's `queue_lock_`, which guards the queued
    /// threads' `SchedulerState`.
    unsafe fn earliest_deadline_thread(
        &self,
        primary: *mut Thread,
        secondary: *mut Thread,
    ) -> *mut Thread {
        if primary.is_null() {
            return secondary;
        }
        if secondary.is_null() {
            return primary;
        }
        // SAFETY: Both threads are valid and `queue_lock_` is held.
        unsafe {
            self.assert_in_scheduler(primary);
            self.assert_in_scheduler(secondary);
        }
        // SAFETY: Both threads are in this scheduler's run queues, so their
        // `SchedulerState` is valid for shared access under `queue_lock_`.
        let primary_state = unsafe { &*cpp_thread_get_scheduler_state(primary) };
        let secondary_state = unsafe { &*cpp_thread_get_scheduler_state(secondary) };
        if primary_state.finish_time <= secondary_state.finish_time { primary } else { secondary }
    }

    /// Returns the completion time clamped to the start of the earliest deadline
    /// thread that will become eligible in that time frame and also has an earlier
    /// finish time than the given reference thread, accounting for criticality.
    ///
    /// # Safety
    /// `reference_thread` must be valid, and the caller must hold the scheduler's
    /// `queue_lock_` and at least shared access to `reference_thread`'s lock.
    unsafe fn clamp_to_earlier_deadline(
        &self,
        completion_time: SchedTime,
        reference_thread: *const Thread,
    ) -> SchedTime {
        // SAFETY: Forwarded from the caller.
        let thread =
            unsafe { self.find_earlier_deadline_thread(completion_time, reference_thread) };
        if !thread.is_null() {
            // SAFETY: `thread` is in one of this scheduler's run queues, which
            // `queue_lock_` guards, so its `SchedulerState` is valid for shared access.
            unsafe { self.assert_in_scheduler(thread) };
            let state = unsafe { &*cpp_thread_get_scheduler_state(thread) };
            core::cmp::min(completion_time, state.start_time)
        } else {
            completion_time
        }
    }

    /// Updates the timeslice of the thread based on the current run queue state.
    /// Returns the absolute deadline for the next time slice, which may be earlier
    /// than the completion of the time slice if other threads could preempt the
    /// given thread before the time slice is exhausted.
    ///
    /// # Safety
    /// `thread` must be valid, and the caller must hold both the scheduler's
    /// `queue_lock_` and `thread`'s lock exclusively, which together guard the
    /// `SchedulerState` fields read and written here.
    unsafe fn next_thread_timeslice(&self, thread: *mut Thread, now: SchedTime) -> SchedTime {
        let trace = local_ktrace_begin_scope!(DETAILED, "next_timeslice");
        // SAFETY: `thread` is valid.
        let state_ptr = unsafe { cpp_thread_get_scheduler_state(thread) };
        // SAFETY: `thread`'s lock is held exclusively, so its effective profile is stable.
        let is_fair = unsafe { (*state_ptr).effective_profile.is_fair() };

        if is_fair {
            // Calculate the time slice before writing it back, as `calculate_timeslice`
            // borrows the thread's state itself.
            // SAFETY: Forwarded from the caller.
            let time_slice_ns = unsafe { self.calculate_timeslice(thread) };
            // SAFETY: `thread`'s lock and `queue_lock_` are held exclusively and no
            // reference to its `SchedulerState` is live.
            unsafe { (*state_ptr).time_slice_ns = time_slice_ns };
        }

        // SAFETY: As above, no other reference to the `SchedulerState` is live and the
        // locks held prevent concurrent writes for the remainder of this function.
        let state = unsafe { &*state_ptr };

        // Calculate the deadline when the remaining time slice is completed. The
        // time slice is maintained by the deadline queuing logic, no need to update
        // it here. The target preemption time is based on the time slice scaled by
        // the performance of the CPU and clamped to the deadline. This increases
        // capacity on slower processors, however, bandwidth isolation is preserved
        // because CPU selection attempts to keep scaled total capacity below one.
        let remaining = state.remaining_time_slice_ns();
        let scaled_time_slice_ns = if state.effective_profile.is_deadline() {
            // SAFETY: `queue_lock_` is held.
            unsafe { self.scale_up(remaining) }
        } else {
            remaining
        };

        let target_preemption_time_ns =
            core::cmp::min(now + scaled_time_slice_ns, state.finish_time);

        ktrace_rs::end_scope!(
            trace,
            "scaled time slice" => scaled_time_slice_ns,
            "target preemption time" => target_preemption_time_ns,
        );

        target_preemption_time_ns
    }

    /// Returns the segment type inferred from whether the given thread is an idle
    /// thread or not.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread`.
    unsafe fn to_cpu_stats_segment_type(thread: *const Thread) -> CpuStatsSegmentType {
        // SAFETY: `thread` is a valid pointer per caller contract.
        if unsafe { cpp_thread_is_idle(thread) } {
            CpuStatsSegmentType::Idle
        } else {
            CpuStatsSegmentType::Busy
        }
    }

    /// Returns a delta value to additively update a predictor. Compares the given
    /// sample to the current value of the predictor and returns a delta such that
    /// the predictor either exponentially peaks or decays toward the sample. The
    /// rate of decay depends on the alpha parameter, while the rate of peaking
    /// depends on the beta parameter. The predictor is not permitted to become
    /// negative.
    ///
    /// A single-rate exponential moving average is updated as follows:
    ///
    ///   Sn = Sn-1 + a * (Yn - Sn-1)
    ///
    /// This function updates the exponential moving average using potentially
    /// different rates for peak and decay:
    ///
    ///   D  = Yn - Sn-1
    ///        [ Sn-1 + a * D      if D < 0
    ///   Sn = [
    ///        [ Sn-1 + b * D      if D >= 0
    fn peak_decay_delta(value: SchedDuration, sample: SchedDuration) -> SchedDuration {
        let delta = sample - value;
        let scaled_delta = if delta >= 0 {
            SchedDuration::from(K_EXPECTED_RUNTIME_BETA * delta)
        } else {
            SchedDuration::from(K_EXPECTED_RUNTIME_ALPHA * delta)
        };
        core::cmp::max(scaled_delta, -value)
    }
}

// Traits implementations to adapt the WavlTree to Thread with node state in the
// scheduler_queue_state member and key in the scheduler_state member.
//
// Soundness note on locking and aliasing for `RunQueue` operations:
// All `RunQueue` insertions, removals, queries, and observer callbacks execute
// exclusively while holding the owning `Scheduler`'s `queue_lock_`. While a
// `Thread` is enqueued in a `RunQueue` (`QueueDisposition::Enqueued`), the
// scheduler's `queue_lock_` synchronizes access to:
//   - `Thread::scheduler_queue_state_.run_queue_node` (`cpp_thread_get_queue_node`)
//   - `Thread::scheduler_state_.start_time`, `finish_time`, `subtree_invariants`,
//     and `effective_profile` (`cpp_thread_get_scheduler_state`)
// Any PI or affinity operation that mutates these fields on an enqueued thread
// must first acquire `queue_lock_` and remove the thread from the `RunQueue`
// before modifying them. Therefore, no concurrent mutation or conflicting
// `&mut SchedulerState` reference can exist while the `WavlTree` trait methods
// below borrow `run_queue_node` or `scheduler_state`.
impl WavlTreeContainable<Thread> for Thread {
    type Rank = bool;
    fn get_node(&self) -> &WavlTreeNode<Thread, Self::Rank> {
        // SAFETY: `self` is a valid `Thread` in or being inserted into a `RunQueue`
        // under `queue_lock_`, which exclusively guards `run_queue_node`.
        unsafe { &*cpp_thread_get_queue_node(core::ptr::from_ref(self).cast_mut()) }
    }
}

impl WavlTreeKeyable<(SchedTime, usize)> for Thread {
    type Key<'a> = (SchedTime, usize);

    fn get_key(&self) -> (SchedTime, usize) {
        // SAFETY: `self` is a valid `Thread` accessed under `queue_lock_`, and no
        // mutable reference to its `SchedulerState` exists during tree key comparisons.
        let state =
            unsafe { &*cpp_thread_get_scheduler_state(core::ptr::from_ref(self).cast_mut()) };
        (state.start_time, state as *const SchedulerState as usize)
    }
}

/// Observer traits that maintain the subtree invariant `min_finish_time` and
/// `min_weight` as nodes are added to and removed from the run queue.
pub struct SubtreeInvariantTraits;

impl WavlTreeAugmentedInvariantObserverTraits for SubtreeInvariantTraits {
    type Target = Thread;
    type Value = SubtreeInvariants;

    fn get_node_value(node: &Self::Target) -> Self::Value {
        // SAFETY: `node` is a valid `Thread` in the `RunQueue` accessed under
        // `queue_lock_`, with no concurrent mutable access to `SchedulerState`.
        let state =
            unsafe { &*cpp_thread_get_scheduler_state(core::ptr::from_ref(node).cast_mut()) };
        let min_weight = state.effective_profile.weight_or(SchedWeight::max());
        SubtreeInvariants { min_finish_time: state.finish_time, min_weight }
    }

    fn get_subtree_value(node: &Self::Target) -> Self::Value {
        // SAFETY: `node` is a valid `Thread` in the `RunQueue` accessed under
        // `queue_lock_`, with no concurrent mutable access to `SchedulerState`.
        let state =
            unsafe { &*cpp_thread_get_scheduler_state(core::ptr::from_ref(node).cast_mut()) };
        state.subtree_invariants
    }

    fn combine_values(a: Self::Value, b: Self::Value) -> Self::Value {
        SubtreeInvariants {
            min_finish_time: core::cmp::min(a.min_finish_time, b.min_finish_time),
            min_weight: core::cmp::min(a.min_weight, b.min_weight),
        }
    }

    fn set_subtree_value(node: &mut Self::Target, val: Self::Value) {
        // SAFETY: `node` is a valid `Thread` in the `RunQueue` mutated exclusively
        // under `queue_lock_`, and no other reference to its `SchedulerState` is live.
        let state = unsafe { &mut *cpp_thread_get_scheduler_state(node) };
        state.subtree_invariants = val;
    }

    fn reset_subtree_value(_node: &mut Self::Target) {
        // No-op, matches C++
    }
}

pub type RunQueueObserver =
    WavlTreeAugmentedInvariantObserver<DefaultObjectTag, SubtreeInvariantTraits>;

/// Alias of the `WavlTree` type for the run queue.
pub type RunQueue =
    WavlTree<(SchedTime, usize), *mut Thread, DefaultObjectTag, TrackingSize, RunQueueObserver>;

#[inline]
fn accept_all(_: *mut Thread) -> bool {
    true
}

/// Performs an augmented binary search for the task with the earliest finish
/// time that also has a start time equal to or earlier than the given eligible
/// time. A predicate may be supplied to filter candidates based on additional
/// conditions.
///
/// The tree is ordered by start time and is augmented by maintaining an
/// additional invariant: each task node in the tree stores the minimum finish
/// time of its descendants, including itself, in addition to its own start and
/// finish time. The combination of these three values permits traversing the
/// tree along a perfect partition of minimum finish times with eligible start
/// times.
///
/// See `fbl/wavl_tree_best_node_observer.h` for an explanation of how the
/// augmented invariant is maintained.
///
/// Note: Any `*mut Thread` returned by this function may only be used as long as
/// the queue lock is held. Dropping the queue lock allows the thread to be
/// removed from the scheduler, and potentially exit, destroying the `Thread` and
/// invalidating the thread pointer in the process.
///
/// # Safety
/// The caller must hold the owning `Scheduler`'s `queue_lock_`, ensuring that
/// `run_queue` is valid, not concurrently mutated, and that all `Thread` nodes
/// in `run_queue` remain valid and associated with this scheduler for the
/// duration of this call.
pub unsafe fn find_earliest_eligible_thread(
    run_queue: &RunQueue,
    eligible_time: SchedTime,
    predicate: impl Fn(*mut Thread) -> bool,
) -> *mut Thread {
    // Early out if there is no eligible thread.
    let front_cursor = run_queue.front_cursor();
    if !front_cursor.is_valid() {
        return core::ptr::null_mut();
    }
    // SAFETY: `front_cursor` is valid and `queue_lock_` is held, so its `Thread`
    // and `SchedulerState` are valid for shared access.
    let front_state = unsafe { &*cpp_thread_get_scheduler_state(front_cursor.as_raw_ptr()) };
    if front_state.start_time > eligible_time {
        return core::ptr::null_mut();
    }

    let mut node = Some(run_queue.root_cursor()).filter(|c| c.is_valid());
    let mut subtree: Option<Cursor<'_, _, _, _, _, _>> = None;
    let mut path: Option<Cursor<'_, _, _, _, _, _>> = None;

    // Descend the tree, with `node` following the path from the root to a leaf,
    // such that the path partitions the tree into two parts: the nodes on the
    // left represent eligible tasks, while the nodes on the right represent tasks
    // that are not eligible. Eligible tasks are both in the left partition and
    // along the search path, tracked by `path`.
    while let Some(curr_node) = node {
        let thread_ptr = curr_node.as_raw_ptr();
        // SAFETY: `curr_node` is a valid cursor in `run_queue` under `queue_lock_`.
        let node_state = unsafe { &*cpp_thread_get_scheduler_state(thread_ptr) };

        if node_state.start_time <= eligible_time {
            let path_needs_update = match path {
                None => true,
                Some(p) => {
                    // SAFETY: `p` is a valid cursor in `run_queue` under `queue_lock_`.
                    let p_state = unsafe { &*cpp_thread_get_scheduler_state(p.as_raw_ptr()) };
                    p_state.finish_time > node_state.finish_time
                }
            };
            if path_needs_update {
                path = Some(curr_node);
            }

            let left = Some(curr_node.left()).filter(|c| c.is_valid());
            if let Some(l) = left {
                // SAFETY: `l` is a valid cursor in `run_queue` under `queue_lock_`.
                let l_state = unsafe { &*cpp_thread_get_scheduler_state(l.as_raw_ptr()) };
                let subtree_needs_update = match subtree {
                    None => true,
                    Some(s) => {
                        // SAFETY: `s` is a valid cursor in `run_queue` under `queue_lock_`.
                        let s_state = unsafe { &*cpp_thread_get_scheduler_state(s.as_raw_ptr()) };
                        s_state.subtree_invariants.min_finish_time
                            > l_state.subtree_invariants.min_finish_time
                    }
                };
                if subtree_needs_update {
                    subtree = Some(l);
                }
            }

            node = Some(curr_node.right()).filter(|c| c.is_valid());
        } else {
            node = Some(curr_node.left()).filter(|c| c.is_valid());
        }
    }

    let Some(s) = subtree else {
        return match path {
            Some(p) => {
                let ptr = p.as_raw_ptr();
                if predicate(ptr) { ptr } else { core::ptr::null_mut() }
            }
            None => core::ptr::null_mut(),
        };
    };

    // SAFETY: `s` is a valid cursor in `run_queue` under `queue_lock_`.
    let s_state = unsafe { &*cpp_thread_get_scheduler_state(s.as_raw_ptr()) };
    let subtree_min_finish_time = s_state.subtree_invariants.min_finish_time;

    if let Some(p) = path {
        // SAFETY: `p` is a valid cursor in `run_queue` under `queue_lock_`.
        let p_state = unsafe { &*cpp_thread_get_scheduler_state(p.as_raw_ptr()) };
        let p_ptr = p.as_raw_ptr();
        if subtree_min_finish_time >= p_state.finish_time && predicate(p_ptr) {
            return p_ptr;
        }
    }

    // Find the node with the earliest finish time among the descendants of the
    // subtree with the smallest minimum finish time.
    let mut node = Some(s);
    while let Some(curr_node) = node {
        let curr_ptr = curr_node.as_raw_ptr();
        // SAFETY: `curr_node` is a valid cursor in `run_queue` under `queue_lock_`.
        let curr_state = unsafe { &*cpp_thread_get_scheduler_state(curr_ptr) };

        if subtree_min_finish_time == curr_state.finish_time && predicate(curr_ptr) {
            return curr_ptr;
        }

        let left = Some(curr_node.left()).filter(|c| c.is_valid());
        let go_left = match left {
            None => false,
            Some(l) => {
                // SAFETY: `l` is a valid cursor in `run_queue` under `queue_lock_`.
                let l_state = unsafe { &*cpp_thread_get_scheduler_state(l.as_raw_ptr()) };
                curr_state.subtree_invariants.min_finish_time
                    == l_state.subtree_invariants.min_finish_time
            }
        };

        if go_left {
            node = left;
        } else {
            node = Some(curr_node.right()).filter(|c| c.is_valid());
        }
    }

    core::ptr::null_mut()
}

/// Inserts a thread into the run queue.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid, pinned `RunQueue` and
/// `thread` points to a valid `Thread` that is not currently in any container.
/// The caller must hold the appropriate queue locks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_insert(rq: *mut RunQueue, thread: *mut Thread) {
    // SAFETY: The caller guarantees `rq` is valid and holds the scheduler's `queue_lock_`.
    let rq = unsafe { &mut *rq };
    // SAFETY: The caller guarantees `thread` is valid and not currently in any container.
    unsafe { rq.insert_raw(thread) };
}

/// Erases a thread from the run queue.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid `RunQueue` and `thread`
/// points to a valid `Thread`. The caller must hold the appropriate queue locks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_erase(
    rq: *mut RunQueue,
    thread: *mut Thread,
) -> *mut Thread {
    // SAFETY: The caller guarantees `rq` is valid and holds the scheduler's `queue_lock_`.
    let rq = unsafe { &mut *rq };
    // SAFETY: The caller guarantees `thread` is valid and currently in `rq`.
    unsafe { rq.erase_raw(thread) }.unwrap_or(core::ptr::null_mut())
}

/// Finds the earliest eligible thread in the run queue matching the predicate,
/// or any eligible thread if `predicate` is `None`.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid `RunQueue`, and that if
/// `predicate` is `Some`, the function and `context` pointer are valid and safe
/// to invoke for any thread in `rq`. The caller must hold the scheduler's `queue_lock_`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_find_earliest_eligible_thread(
    rq: *const RunQueue,
    eligible_time: SchedTime,
    predicate: Option<unsafe extern "C" fn(*const Thread, *const core::ffi::c_void) -> bool>,
    context: *const core::ffi::c_void,
) -> *mut Thread {
    // SAFETY: The caller guarantees `rq` is valid and holds `queue_lock_`.
    let rq = unsafe { &*rq };
    match predicate {
        Some(pred) => {
            let rust_predicate = |thread: *mut Thread| {
                // SAFETY: The caller guarantees `pred` and `context` are valid to invoke.
                unsafe { pred(thread, context) }
            };
            // SAFETY: The caller holds `queue_lock_` for `rq`.
            unsafe { find_earliest_eligible_thread(rq, eligible_time, rust_predicate) }
        }
        None => {
            // SAFETY: The caller holds `queue_lock_` for `rq`.
            unsafe { find_earliest_eligible_thread(rq, eligible_time, accept_all) }
        }
    }
}

/// Returns the number of threads in the run queue.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid `RunQueue`.
/// The caller must hold the appropriate queue locks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_size(rq: *const RunQueue) -> usize {
    // SAFETY: The caller guarantees `rq` is valid and holds `queue_lock_`.
    let rq = unsafe { &*rq };
    rq.len()
}

/// Returns true if the run queue is empty.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid `RunQueue`.
/// The caller must hold the appropriate queue locks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_is_empty(rq: *const RunQueue) -> bool {
    // SAFETY: The caller guarantees `rq` is valid and holds `queue_lock_`.
    let rq = unsafe { &*rq };
    rq.is_empty()
}

/// Returns the front thread in the run queue, or null if empty.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid `RunQueue`.
/// The caller must hold the appropriate queue locks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_front(rq: *const RunQueue) -> *mut Thread {
    // SAFETY: The caller guarantees `rq` is valid and holds `queue_lock_`.
    let rq = unsafe { &*rq };
    let front = rq.front_cursor().as_raw_ptr();
    if fbl::is_sentinel_ptr(front) { core::ptr::null_mut() } else { front }
}

#[repr(C)]
pub struct RunQueueIterator {
    current: *mut Thread,
}

// RunQueueIterator layout assertions
zr::static_assert!(core::mem::size_of::<RunQueueIterator>() == 8);
zr::static_assert!(core::mem::align_of::<RunQueueIterator>() == 8);

/// Creates a run queue iterator positioned at the front.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid `RunQueue`.
/// The caller must hold the appropriate queue locks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_iterator_create(
    rq: *const RunQueue,
) -> RunQueueIterator {
    // SAFETY: The caller guarantees `rq` is valid and holds `queue_lock_`.
    let rq = unsafe { &*rq };
    let front = rq.front_cursor().as_raw_ptr();
    let current = if fbl::is_sentinel_ptr(front) { core::ptr::null_mut() } else { front };
    RunQueueIterator { current }
}

/// Returns true if the iterator is valid (not at the end).
///
/// # Safety
/// The caller must ensure that `iter` points to a valid `RunQueueIterator`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_iterator_is_valid(
    iter: *const RunQueueIterator,
) -> bool {
    // SAFETY: The caller guarantees `iter` points to a valid `RunQueueIterator`.
    let iter = unsafe { &*iter };
    !iter.current.is_null()
}

/// Returns the current thread pointer, or null if invalid.
///
/// # Safety
/// The caller must ensure that `iter` points to a valid `RunQueueIterator`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_iterator_get(
    iter: *const RunQueueIterator,
) -> *mut Thread {
    // SAFETY: The caller guarantees `iter` points to a valid `RunQueueIterator`.
    unsafe { (*iter).current }
}

/// Advances the iterator to the next element.
///
/// # Safety
/// The caller must ensure that `iter` points to a valid `RunQueueIterator`,
/// that the owning scheduler's `queue_lock_` is held, and that if `iter` is
/// valid, `iter.current` is still enqueued in the `RunQueue`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_iterator_next(iter: *mut RunQueueIterator) {
    // SAFETY: The caller guarantees `iter` is valid, `queue_lock_` is held, and
    // `iter.current` is either null or a valid enqueued `Thread`.
    let iter = unsafe { &mut *iter };
    unsafe { RunQueue::successor_raw(&mut iter.current) };
}

/// Returns the root thread in the run queue, or null if empty.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid `RunQueue`.
/// The caller must hold the appropriate queue locks.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_root(rq: *const RunQueue) -> *mut Thread {
    // SAFETY: The caller guarantees `rq` is valid and holds `queue_lock_`.
    let rq = unsafe { &*rq };
    let root = rq.root_cursor().as_raw_ptr();
    if fbl::is_sentinel_ptr(root) { core::ptr::null_mut() } else { root }
}

/// Updates `sched`'s fair bandwidth period. See `Scheduler::update_fair_bandwidth_period`.
///
/// # Safety
/// `sched` must point to a valid `Scheduler` whose `queue_lock_` is held by the current CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_fair_bandwidth_period(
    sched: *const Scheduler,
    now: SchedTime,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).update_fair_bandwidth_period(now) };
}

/// # Safety
/// `sched` and `reference_thread` must be valid, and the caller must hold `sched`'s
/// `queue_lock_` and at least shared access to `reference_thread`'s lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_find_earlier_deadline_thread(
    sched: *const Scheduler,
    eligible_time: SchedTime,
    reference_thread: *const Thread,
) -> *mut Thread {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).find_earlier_deadline_thread(eligible_time, reference_thread) }
}

/// # Safety
/// `sched` must be valid, and the caller must hold its `queue_lock_`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_get_next_eligible_time(
    sched: *const Scheduler,
) -> SchedTime {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).get_next_eligible_time() }
}

/// # Safety
/// `sched` and `thread` must be valid, and the caller must hold `sched`'s `queue_lock_`
/// and at least shared access to `thread`'s lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_calculate_timeslice(
    sched: *const Scheduler,
    thread: *const Thread,
) -> SchedDuration {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).calculate_timeslice(thread) }
}

/// # Safety
/// `sched` must be valid, and the caller must hold its `queue_lock_`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_clamp_to_deadline(
    sched: *const Scheduler,
    completion_time: SchedTime,
) -> SchedTime {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).clamp_to_deadline(completion_time) }
}

/// # Safety
/// `sched` and `reference_thread` must be valid, and the caller must hold `sched`'s
/// `queue_lock_` and at least shared access to `reference_thread`'s lock.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_clamp_to_earlier_deadline(
    sched: *const Scheduler,
    completion_time: SchedTime,
    reference_thread: *const Thread,
) -> SchedTime {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).clamp_to_earlier_deadline(completion_time, reference_thread) }
}

/// # Safety
/// `sched` and `thread` must be valid, and the caller must hold both `sched`'s
/// `queue_lock_` and `thread`'s lock exclusively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_next_thread_timeslice(
    sched: *const Scheduler,
    thread: *mut Thread,
    now: SchedTime,
) -> SchedTime {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).next_thread_timeslice(thread, now) }
}

/// # Safety
/// `sched` and `thread` must be valid, and the caller must hold both `sched`'s
/// `queue_lock_` and `thread`'s lock exclusively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_activation(
    sched: *const Scheduler,
    thread: *mut Thread,
    now: SchedTime,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).update_activation(thread, now) };
}

/// # Safety
/// `sched` and `thread` must be valid, and the caller must hold both `sched`'s
/// `queue_lock_` and `thread`'s lock exclusively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_adjust_fair_bandwidth(
    sched: *const Scheduler,
    thread: *mut Thread,
    now: SchedTime,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).adjust_fair_bandwidth(thread, now) };
}

/// # Safety
/// The caller must ensure `sched` is valid and `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_user_set_processing_rate(
    sched: *const Scheduler,
    rate: SchedProcessingRate,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).user_set_processing_rate(rate) };
}

/// # Safety
/// The caller must ensure `sched` is valid and `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_user_set_processing_rate_limits(
    sched: *const Scheduler,
    min: SchedProcessingRate,
    max: SchedProcessingRate,
) -> bool {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).user_set_processing_rate_limits(min, max) }
}

/// # Safety
/// The caller must ensure `sched` is valid and `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_pend_power_level_request(
    sched: *const Scheduler,
    power_level: u8,
) -> bool {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).pend_power_level_request(power_level) }
}

/// # Safety
/// The caller must ensure `sched` is valid and `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_pend_power_level_request_for_rate(
    sched: *const Scheduler,
    processing_rate: SchedProcessingRate,
) -> bool {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).pend_power_level_request_for_rate(processing_rate) }
}

/// # Safety
/// The caller must ensure `sched` is valid and `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_reevaluate_current_power_level(sched: *const Scheduler) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).reevaluate_current_power_level() };
}

/// # Safety
/// The caller must ensure `sched` is valid and `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_total_deadline_utilization(
    sched: *const Scheduler,
    delta: SchedUtilization,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).update_total_deadline_utilization(delta) };
}

/// # Safety
/// The caller must ensure `sched` is valid and `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_processing_rate(
    sched: *const Scheduler,
    boot_ticks: zx_types::zx_instant_boot_ticks_t,
) -> bool {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).update_processing_rate(boot_ticks) }
}

/// # Safety
/// The caller must ensure `current_thread` and `next_thread` are valid and their
/// thread locks are held exclusively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_sched_trace_context_switch(
    current_thread: *mut Thread,
    next_thread: *mut Thread,
    current_cpu: CpuNum,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { sched_trace_context_switch(current_thread, next_thread, current_cpu) };
}

/// # Safety
/// The caller must ensure `thread` is valid and its thread lock is held exclusively.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_sched_trace_wakeup(thread: *mut Thread, target_cpu: CpuNum) {
    // SAFETY: Forwarded from the caller.
    unsafe { sched_trace_wakeup(thread, target_cpu) };
}

/// # Safety
/// `sched` must be valid and `queue_lock_` must be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_total_expected_runtime(
    sched: *const Scheduler,
    delta_ns: SchedDuration,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).update_total_expected_runtime(delta_ns) };
}

/// # Safety
/// `sched` must be valid and `queue_lock_` must be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_cpu_stats_segment(
    sched: *const Scheduler,
    now: SchedTime,
    is_idle: bool,
    segment_processing_rate: SchedProcessingRate,
) {
    let seg_type = if is_idle { CpuStatsSegmentType::Idle } else { CpuStatsSegmentType::Busy };
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).update_cpu_stats_segment(now, seg_type, segment_processing_rate) };
}

/// # Safety
/// `sched` and `current_thread` must be valid and locked.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_estimated_energy_consumption(
    sched: *const Scheduler,
    current_thread: *mut Thread,
    mono_time: SchedTime,
    boot_ticks: zx_types::zx_instant_boot_ticks_t,
    actual_runtime_ns: SchedDuration,
) {
    // SAFETY: Forwarded from the caller.
    unsafe {
        (*sched).update_estimated_energy_consumption(
            current_thread,
            mono_time,
            boot_ticks,
            actual_runtime_ns,
        )
    };
}

/// Pure arithmetic helper; see `Scheduler::peak_decay_delta`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_scheduler_peak_decay_delta(
    value: SchedDuration,
    sample: SchedDuration,
) -> SchedDuration {
    Scheduler::peak_decay_delta(value, sample)
}

/// Initializes a run queue in-place.
///
/// # Safety
/// The caller must ensure that `rq` points to allocated, uninitialized memory
/// of size and alignment matching `RunQueue`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_init(rq: *mut RunQueue) {
    use fbl::pin_init::PinInit;
    let init = RunQueue::new();
    // SAFETY: `rq` points to valid, uninitialized memory of correct size/alignment,
    // and `RunQueue::new()` is infallible (`Infallible` error type).
    unsafe {
        init.__pinned_init(rq).unwrap_or_else(|e| match e {});
    }
}

/// Destroys a run queue in-place.
///
/// # Safety
/// The caller must ensure that `rq` points to a valid, initialized `RunQueue`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_run_queue_destroy(rq: *mut RunQueue) {
    // SAFETY: `rq` points to valid initialized memory.
    unsafe {
        core::ptr::drop_in_place(rq);
    }
}

// RAII lock helper
struct QueueLockGuard {
    // Safety invariant: `scheduler` points to a valid `Scheduler` whose
    // `queue_lock_` is held by the current CPU for the lifetime of this guard.
    scheduler: *mut Scheduler,
}
impl QueueLockGuard {
    /// Acquires `scheduler`'s queue lock and returns an RAII guard that releases
    /// it on drop.
    ///
    /// # Safety
    /// `scheduler` must point to a valid `Scheduler` instance, interrupts must
    /// already be disabled, and `queue_lock_` must not already be held by the
    /// current CPU.
    unsafe fn acquire(scheduler: *mut Scheduler) -> Self {
        // SAFETY: `scheduler` is valid, interrupts are disabled, and `queue_lock_`
        // is not already held.
        unsafe {
            cpp_scheduler_lock_queue(scheduler);
        }
        Self { scheduler }
    }
}
impl Drop for QueueLockGuard {
    fn drop(&mut self) {
        // SAFETY: By the type's safety invariant, `self.scheduler` is valid and
        // its `queue_lock_` is held by the current CPU.
        unsafe {
            cpp_scheduler_unlock_queue(self.scheduler);
        }
    }
}

// RAII lock helper which also saves and restores the interrupt state, the
// equivalent of `Guard<MonitoredSpinLock, IrqSave>`.
//
// TODO(https://fxbug.dev/537458631): The C++ shim acquires the underlying
// spinlock directly, bypassing lockdep validation. Route through `Guard` once
// cross-language inlining allows it.
struct QueueLockIrqSaveGuard {
    // Safety invariant: `scheduler` points to a valid `Scheduler` whose
    // `queue_lock_` is held by the current CPU for the lifetime of this guard,
    // and `irq_state` is the interrupt state saved when it was acquired.
    scheduler: *mut Scheduler,
    irq_state: u64,
}
impl QueueLockIrqSaveGuard {
    /// Disables interrupts, acquires `scheduler`'s queue lock, and returns an
    /// RAII guard that releases the lock and restores the interrupt state on
    /// drop.
    ///
    /// # Safety
    /// `scheduler` must point to a valid `Scheduler` instance and `queue_lock_`
    /// must not already be held by the current CPU.
    unsafe fn acquire(scheduler: *mut Scheduler) -> Self {
        // SAFETY: `scheduler` is valid and `queue_lock_` is not already held.
        let irq_state = unsafe { cpp_scheduler_lock_queue_irq_save(scheduler) };
        Self { scheduler, irq_state }
    }
}
impl Drop for QueueLockIrqSaveGuard {
    fn drop(&mut self) {
        // SAFETY: By the type's safety invariant, `self.scheduler` is valid, its
        // `queue_lock_` is held by the current CPU, and `self.irq_state` is the
        // state saved on acquisition.
        unsafe {
            cpp_scheduler_unlock_queue_irq_restore(self.scheduler, self.irq_state);
        }
    }
}

// Handle all of the common tasks associated with each of the possible PI
// interactions. The outline of this is:
//
// 1) If the target is an active thread (meaning either running or runnable),
//    we need to:
// 1.1) Enter the scheduler's queue lock.
// 1.2) If the thread is active, but not actually running, remove the target
//      thread from its scheduler's run queue if it is in the queue.
// 1.3) Now update the thread's effective profile.
// 1.4) Apply any changes in the thread's effective profile to its scheduler's
//      bookkeeping.
// 1.5) Update the dynamic parameters of the thread.
// 1.6) Either re-insert the thread into its scheduler's run queue (if it was
//      READY AND in the queue) or adjust its scheduler's preemption time (if it
//      was RUNNING).
// 1.7) Trigger a reschedule of the thread's CPU.
// 2) If the target is either an OwnedWaitQueue, or a thread which is not
//    active:
// 2.1) Recompute the target's effective profile, adjust the target's position
//      in its wait queue if the target is a thread which is currently
//      blocked in a wait queue.
// 2.2) Recompute the target's dynamic scheduler parameters.

const MIGRATE_STAGE_SAVE: i32 = 0;
const MIGRATE_STAGE_RESTORE: i32 = 1;

/// Indicates how to select the target CPU for the woken task. This flag is a
/// hint to enable more optimal CPU placement for "synchronous wake"
/// operations, but is not required for correctness. See `unblock_synchronous`
/// for more details.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum UnblockType {
    /// Indicates that the target CPU selection process should place the woken
    /// task based solely on its own scheduling properties and last CPU history.
    Normal = 0,

    /// Indicates that the target CPU selection process should place the task
    /// based on both the woken and current threads' scheduling properties,
    /// particularly the CPU and urgency of the current thread, such that, under
    /// compatible conditions, the woken thread can replace the current thread on
    /// a CPU as it blocks in the same reschedule operation.
    Synchronous = 1,
}

const IDLE_WEIGHT: i32 = i32::MIN;

#[inline]
fn sched_user_thread_state(state: ThreadState) -> zx_types::zx_thread_state_t {
    match state {
        ThreadState::Initial | ThreadState::Ready => zx_types::ZX_THREAD_STATE_NEW,
        ThreadState::Running => zx_types::ZX_THREAD_STATE_RUNNING,
        ThreadState::Blocked | ThreadState::BlockedReadLock | ThreadState::Sleeping => {
            zx_types::ZX_THREAD_STATE_BLOCKED
        }
        ThreadState::Suspended => zx_types::ZX_THREAD_STATE_SUSPENDED,
        ThreadState::Death => zx_types::ZX_THREAD_STATE_DEAD,
    }
}

/// Writes a context switch record to the ktrace buffer. This is always enabled
/// so that user mode tracing can track which threads are running.
///
/// # Safety
/// `current_thread` and `next_thread` must point to valid `Thread` instances,
/// their thread locks must be held exclusively, and no active references to
/// their `SchedulerState` may be held across this call.
unsafe fn sched_trace_context_switch(
    current_thread: *mut Thread,
    next_thread: *mut Thread,
    current_cpu: CpuNum,
) {
    // SAFETY: `current_thread` and `next_thread` are valid with their thread locks held
    // exclusively. Each `SchedulerState` borrow is a non-overlapping temporary expression
    // so `current_thread == next_thread` does not alias.
    unsafe {
        ktrace_rs::context_switch!(
            "kernel:sched",
            current_cpu,
            sched_user_thread_state((*cpp_thread_get_scheduler_state(current_thread)).state),
            cpp_thread_get_tid(current_thread),
            cpp_thread_get_tid(next_thread),
            "outgoing_weight" => if cpp_thread_is_idle(current_thread) {
                IDLE_WEIGHT
            } else {
                (*cpp_thread_get_scheduler_state(current_thread))
                    .get_weight_or_packed_deadline_params()
            },
            "incoming_weight" => if cpp_thread_is_idle(next_thread) {
                IDLE_WEIGHT
            } else {
                (*cpp_thread_get_scheduler_state(next_thread))
                    .get_weight_or_packed_deadline_params()
            },
        );
    }
}

/// Writes a thread wakeup record to the ktrace buffer. This is always enabled
/// so that user mode tracing can track which threads are waking.
///
/// # Safety
/// `thread` must point to a valid `Thread` whose thread lock is held exclusively,
/// no active references to its `SchedulerState` may be held across this call, and
/// the current CPU must have a valid current thread.
unsafe fn sched_trace_wakeup(thread: *mut Thread, target_cpu: CpuNum) {
    let current_thread = super::thread::current_get();
    // SAFETY: `thread` is valid with its lock held exclusively, and `current_thread` is valid
    // on the current CPU (`is_idle` and `tid` are immutable after thread initialization).
    unsafe {
        if !cpp_thread_is_idle(current_thread) {
            ktrace_rs::thread_wakeup!(
                "kernel:sched",
                target_cpu,
                cpp_thread_get_tid(thread),
                "weight" => if cpp_thread_is_idle(thread) {
                    IDLE_WEIGHT
                } else {
                    (*cpp_thread_get_scheduler_state(thread))
                        .get_weight_or_packed_deadline_params()
                },
                "waker" => ktrace_rs::Koid(cpp_thread_get_tid(current_thread)),
            );
        } else {
            ktrace_rs::thread_wakeup!(
                "kernel:sched",
                target_cpu,
                cpp_thread_get_tid(thread),
                "weight" => if cpp_thread_is_idle(thread) {
                    IDLE_WEIGHT
                } else {
                    (*cpp_thread_get_scheduler_state(thread))
                        .get_weight_or_packed_deadline_params()
                },
            );
        }
    }
}

impl Scheduler {
    /// Sets or clears this scheduler's bit in the global idle state mask; similar
    /// to the active state mask.
    ///
    /// # Safety
    /// `this_cpu` must have been initialized by `sched_early_init`.
    pub unsafe fn set_idle(&self, is_idle: bool) {
        // SAFETY: Forwarded from the caller.
        let mask = cpu_num_to_mask(unsafe { self.this_cpu() });
        if is_idle {
            IDLE_SCHEDULERS.fetch_or(mask, core::sync::atomic::Ordering::Relaxed);
        } else {
            IDLE_SCHEDULERS.fetch_and(!mask, core::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Changes this scheduler's CPU's active state. We require that the scheduler's
    /// `queue_lock` be held during the operation. This enforces the invariant that
    /// "the scheduler's active bit only changes while holding the scheduler's
    /// queue lock".
    ///
    /// This means that things like `find_target_cpu` are free to check the global
    /// active mask to see whether or not we _think_ that a given scheduler is
    /// likely to be active for selection purposes, but when it comes time to
    /// actually add a thread to a scheduler's queues, we check the active state of
    /// the scheduler's assigned CPU from within the safety of the queue lock using
    /// `is_scheduler_active_locked()`.
    ///
    /// # Safety
    /// `self` must be the current CPU's scheduler (`set_blocking_disallowed`
    /// operates on the current CPU), `this_cpu` must have been initialized by
    /// `sched_early_init`, and the caller must hold `queue_lock_`.
    unsafe fn set_is_active(&self, is_active: bool) {
        // SAFETY: Forwarded from the caller.
        let this_cpu = unsafe { self.this_cpu() };
        debug_assert_eq!(this_cpu, crate::arch_rs::curr_cpu_num());
        let mask = cpu_num_to_mask(this_cpu);
        if is_active {
            ACTIVE_SCHEDULERS.fetch_or(mask, core::sync::atomic::Ordering::Relaxed);
        } else {
            ACTIVE_SCHEDULERS.fetch_and(!mask, core::sync::atomic::Ordering::Relaxed);
        }
        crate::arch_rs::set_blocking_disallowed(!is_active);
    }

    /// Sets/clears the current CPU's bit in the global active mask. Holds the
    /// scheduler's queue lock while the bit is changed, enforcing the invariant
    /// that a scheduler's active bit can only ever change while holding that
    /// scheduler's lock.
    ///
    /// # Safety
    /// The current CPU's `Scheduler` must be initialized and its `queue_lock_`
    /// must not already be held by the current CPU. Interrupts must be disabled,
    /// or the calling thread must otherwise be unable to migrate to another CPU,
    /// so that the scheduler looked up here is still the current CPU's when the
    /// queue lock is acquired.
    pub unsafe fn set_curr_cpu_active(is_active: bool) {
        // SAFETY: The current CPU's scheduler is initialized, so `cpp_scheduler_get`
        // returns a valid pointer.
        let scheduler = unsafe { cpp_scheduler_get(crate::arch_rs::curr_cpu_num()) };
        // SAFETY: `scheduler` is valid and `queue_lock_` is not already held.
        let _guard = unsafe { QueueLockIrqSaveGuard::acquire(scheduler) };
        // SAFETY: `scheduler` is valid and `_guard` holds its `queue_lock_`.
        unsafe { (*scheduler).set_is_active(is_active) };
    }

    /// Checks to see if `self` scheduler is currently active while holding its
    /// `queue_lock`, ensuring that it will stay in its current state until after we
    /// drop the lock.
    ///
    /// # Safety
    /// `this_cpu` must have been initialized by `sched_early_init`, and the caller
    /// must hold `queue_lock_`.
    pub unsafe fn is_scheduler_active_locked(&self) -> bool {
        // SAFETY: Forwarded from the caller.
        peek_is_active(unsafe { self.this_cpu() })
    }

    /// Dumps the state of the run queue to stdout.
    #[inline]
    pub fn dump(&self, queue_state_only: bool) {
        // SAFETY: `self` is a valid `Scheduler` reference.
        unsafe { cpp_scheduler_dump((self as *const Self).cast_mut(), queue_state_only) };
    }

    /// Returns a reference to this scheduler's queue lock (`queue_lock_`).
    #[inline]
    pub fn queue_lock(&self) -> &ksync::KMonitoredSpinlock<SchedulerQueueLockClass> {
        // SAFETY: `self` is a valid `Scheduler` and `cpp_scheduler_get_queue_lock` returns a
        // pointer to its `queue_lock_` member, which is layout-compatible with
        // `KMonitoredSpinlock`.
        unsafe { &*cpp_scheduler_get_queue_lock(self) }
    }

    /// Run the passed callable while holding the specified scheduler's lock.
    /// Currently used only by the threadload debug function in `kernel/debug.rs`.
    pub fn run_in_locked_scheduler<R, F: FnOnce(&Self) -> R>(which_cpu: CpuNum, callable: F) -> R {
        // The call to PerCpu::get will eventually assert if our cpu ID is out of range.
        let scheduler = &super::percpu::PerCpu::get(which_cpu).scheduler;
        ksync::lock!(let _queue_guard = scheduler.queue_lock().lock_with(ksync::source_tag!()));
        callable(scheduler)
    }
}

impl Scheduler {
    /// Writes the CPU stats for the given CPU number to `out_stats`, forward
    /// projected to account for idle or normalized busy time elapsed since the
    /// last relevant state change.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `cpu_num` is a valid CPU number, that `out_stats`
    /// points to valid memory for a `CpuStats` structure, and that the target CPU's
    /// `queue_lock_` is not already held.
    unsafe fn project_cpu_stats(cpu_num: CpuNum, out_stats: *mut CpuStats) {
        debug_assert!((cpu_num as usize) < PerCpu::processor_count());

        // SAFETY: `cpu_num` is a valid CPU number, so `cpp_scheduler_get` and
        // `cpp_get_cpu_stats` return valid pointers.
        let (scheduler, percpu_stats_ptr) =
            unsafe { (cpp_scheduler_get(cpu_num), cpp_get_cpu_stats(cpu_num)) };

        // SAFETY: `scheduler` is valid and `queue_lock_` is not already held.
        let _guard = unsafe { QueueLockIrqSaveGuard::acquire(scheduler) };

        // Some CPU stats values are synchronized by the queue lock, while others are
        // updated via unsynchronized relaxed atomic writes. Copy word-by-word with
        // relaxed atomic loads to avoid formal data races on the unsynchronized
        // members, matching `concurrent::WellDefinedCopyFrom`.
        const WORD_COUNT: usize = {
            assert!(core::mem::size_of::<CpuStats>().is_multiple_of(core::mem::size_of::<u64>()));
            core::mem::size_of::<CpuStats>() / core::mem::size_of::<u64>()
        };
        let src_words = percpu_stats_ptr.cast::<core::sync::atomic::AtomicU64>();
        let dst_words = out_stats.cast::<u64>();
        for i in 0..WORD_COUNT {
            // SAFETY: Both `percpu_stats_ptr` and `out_stats` point to valid,
            // 8-byte-aligned `CpuStats` structs (`17 * 8 == 136` bytes).
            unsafe {
                let val = (*src_words.add(i)).load(core::sync::atomic::Ordering::Relaxed);
                dst_words.add(i).write(val);
            }
        }

        let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
        // SAFETY: `out_stats` was initialized above and is exclusively owned by this call.
        let last_updated_instant = unsafe { (*out_stats).last_updated_instant };
        let duration_since_last_update_ns = now - SchedTime::from_raw(last_updated_instant);
        debug_assert!(
            duration_since_last_update_ns.raw_value() >= 0,
            "now={} last_updated_instant={} duration_since_last_update_ns={}",
            now.raw_value(),
            last_updated_instant,
            duration_since_last_update_ns.raw_value()
        );

        if peek_is_idle(cpu_num) {
            // SAFETY: `out_stats` is valid and exclusively owned by this call.
            unsafe {
                (*out_stats).idle_time += duration_since_last_update_ns.raw_value();
            }
        } else {
            // SAFETY: `scheduler` is valid and its `queue_lock_` is held by `_guard`.
            let rate = unsafe { (*scheduler).processing_rate() };
            let scaled = SchedDuration::from(duration_since_last_update_ns * rate).raw_value();
            // SAFETY: `out_stats` is valid and exclusively owned by this call.
            unsafe {
                (*out_stats).normalized_busy_time += scaled;
            }
        }
    }
}

/// Returns the CPU stats for the given CPU number, forward projected to
/// account for idle or normalized busy time elapsed since the last relevant
/// state change.
///
/// # Safety
///
/// The caller must ensure that `cpu_num` is a valid CPU number, that `out_stats`
/// points to valid memory for a `CpuStats` structure, and that the target CPU's
/// `queue_lock_` is not already held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_get_projected_cpu_stats(
    cpu_num: CpuNum,
    out_stats: *mut CpuStats,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { Scheduler::project_cpu_stats(cpu_num, out_stats) };
}

impl Scheduler {
    /// Writes the diagnostic header for this scheduler's queue state to `output_target`.
    ///
    /// Kept out of line so that its formatting temporaries are not live on the
    /// caller's frame while the run queues are walked under `queue_lock_`.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    #[inline(never)]
    unsafe fn dump_header(&self, output_target: &mut libc::stdio::FILE) {
        use core::fmt::Write;
        let state = self.state.get();
        let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
        // SAFETY: The caller holds `queue_lock_`, synchronizing access to the run queues,
        // queued threads' `SchedulerState` fields, and `RustSchedulerState`.
        let (
            variable_now,
            min_weight,
            variable_eligible_time,
            monotonic_eligible_time,
            weight_total,
            min_utilization,
            norm_deadline_util,
            fair_period_raw,
            runnable_task_count,
            total_expected_runtime_raw,
            mono_ref_raw,
            var_ref_raw,
            slope,
        ) = unsafe {
            let variable_now = self.monotonic_to_variable(now);
            let mut min_weight = SchedWeight::from_raw(0);
            let mut variable_eligible_time = SchedTime::max();
            if !(*state).fair_run_queue.is_empty() {
                let root = (*state).fair_run_queue.root_cursor().as_raw_ptr();
                self.assert_in_scheduler(root);
                let root_state = cpp_thread_get_scheduler_state(root);
                min_weight = core::cmp::min(
                    (*state).active_thread_weight,
                    (*root_state).subtree_invariants.min_weight,
                );

                let front =
                    core::ptr::from_ref((*state).fair_run_queue.front().unwrap()).cast_mut();
                self.assert_in_scheduler(front);
                let front_state = cpp_thread_get_scheduler_state(front);
                variable_eligible_time = (*front_state).start_time;
            }
            let monotonic_eligible_time = self.variable_to_monotonic(variable_eligible_time);
            let weight_total = (*state).weight_total;
            let min_utilization = if weight_total != SchedWeight::from_raw(0) {
                SchedUtilization::from(min_weight / weight_total)
            } else {
                SchedUtilization::from_raw(0)
            };
            let norm_deadline_util = cpp_scheduler_get_normalized_utilization(self);
            (
                variable_now,
                min_weight,
                variable_eligible_time,
                monotonic_eligible_time,
                weight_total,
                min_utilization,
                norm_deadline_util,
                (*state).fair_period.raw_value(),
                (*state).runnable_task_count,
                (*state).total_expected_runtime_ns.raw_value(),
                (*state).fair_affine_transform.monotonic_reference_time.raw_value(),
                (*state).fair_affine_transform.variable_reference_time.raw_value(),
                (*state).fair_affine_transform.slope,
            )
        };
        // Fixed-point values are formatted through their `Display` impls so that the
        // formatting buffers live in the formatter's frame rather than here.
        let _ = write!(
            output_target,
            "\tw_sum={} w_min={} T_v={} count={} ema_sum={}\n\
             \tU_{{c,sum}}={} U_{{v,min}}={}\n\
             \tnow={} vnow={} eligible={} veligible={}\n\
             \tmono_ref={} var_ref={} slope={}\n",
            weight_total,
            min_weight,
            fair_period_raw,
            runnable_task_count,
            total_expected_runtime_raw,
            norm_deadline_util,
            min_utilization,
            now.raw_value(),
            variable_now.raw_value(),
            monotonic_eligible_time.raw_value(),
            variable_eligible_time.raw_value(),
            mono_ref_raw,
            var_ref_raw,
            slope,
        );
    }

    /// Writes a single diagnostic line for `thread` to `output_target`.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread` associated with this scheduler,
    /// and the caller must hold `queue_lock_`.
    unsafe fn dump_thread(
        &self,
        output_target: &mut libc::stdio::FILE,
        thread: *const Thread,
        is_active: bool,
    ) {
        use core::fmt::Write;
        // SAFETY: `thread` is associated with this scheduler and `queue_lock_` is held.
        // We project fields through raw pointers rather than forming `&SchedulerState`
        // or `&EffectiveProfile` because `dirty_flags` and profile inputs may be
        // written concurrently under `thread`'s lock alone.
        let (
            discipline,
            weight,
            raw_start_time,
            raw_finish_time,
            time_slice_ns,
            remaining_time_slice_ns,
            expected_runtime_ns,
            effective_period,
        ) = unsafe {
            let state = cpp_thread_get_scheduler_state(thread.cast_mut());
            let ep = &raw const (*state).effective_profile;
            let raw_start_time = (*state).start_time;
            let raw_finish_time = (*state).finish_time;
            let time_slice_ns = (*state).time_slice_ns;
            let time_slice_used_ns = (*state).time_slice_used_ns;
            (
                EffectiveProfile::discipline_raw(ep),
                (*ep).weight,
                raw_start_time,
                raw_finish_time,
                time_slice_ns,
                time_slice_ns - time_slice_used_ns,
                (*state).expected_runtime_ns,
                raw_finish_time - raw_start_time,
            )
        };
        // Copied into its own local (not through the tuple above) so the name buffer
        // is materialized once on this frame.
        // SAFETY: `thread` is a valid `Thread`.
        let name = unsafe { thread_name(thread) };
        let name = name.as_str();
        let prefix = if is_active { "->" } else { "  " };
        if discipline == SchedDiscipline::Fair {
            // A fair thread that is the active thread has start and finish
            // times on the monotonic timeline. All fair threads in run queues
            // have start and finish times on the variable timeline. Display all
            // start and finish times on the monotonic timeline for consistent
            // presentation.
            let start_time = if is_active {
                raw_start_time
            } else {
                // SAFETY: `queue_lock_` is held.
                unsafe { self.variable_to_monotonic(raw_start_time) }
            };
            let finish_time = if is_active {
                raw_finish_time
            } else {
                // SAFETY: `queue_lock_` is held.
                unsafe { self.variable_to_monotonic(raw_finish_time) }
            };
            let _ = writeln!(
                output_target,
                "\t{} name={} weight={} start={} finish={} ts={} ema={} T_e={}",
                prefix,
                name,
                weight,
                start_time.raw_value(),
                finish_time.raw_value(),
                remaining_time_slice_ns.raw_value(),
                expected_runtime_ns.raw_value(),
                effective_period.raw_value(),
            );
        } else {
            // SAFETY: `thread` is a valid deadline thread associated with this scheduler
            // and `queue_lock_` is held.
            let (deadline, is_critical) = unsafe {
                let state = cpp_thread_get_scheduler_state(thread.cast_mut());
                let ep = &raw const (*state).effective_profile;
                (EffectiveProfile::deadline_raw(ep), self.is_critical_deadline_thread(thread))
            };
            let critical_str = if is_critical { "critical" } else { "" };
            let _ = writeln!(
                output_target,
                "\t{} name={} deadline=({}, {}) start= {} finish= {} ts= {} ema= {} T_e= {} {}",
                prefix,
                name,
                deadline.capacity_ns.raw_value(),
                deadline.deadline_ns.raw_value(),
                raw_start_time.raw_value(),
                raw_finish_time.raw_value(),
                time_slice_ns.raw_value(),
                expected_runtime_ns.raw_value(),
                effective_period.raw_value(),
                critical_str,
            );
        }
    }
}

impl Scheduler {
    /// Dumps the state of the run queue to `output_target`.
    ///
    /// # Safety
    /// Interrupts must be disabled and `queue_lock_` must not be held.
    unsafe fn dump_to(&self, output_target: &mut libc::stdio::FILE, queue_state_only: bool) {
        // We're about to acquire the `queue_lock` and write some things.
        // Depending on the output target, writing may end up calling `DLog::Write`,
        // which may call `Event::Signal`, which may re-enter the Scheduler. If that
        // happens while we're holding the `queue_lock` that would be bad.
        // `DLog::Write` has a hack that allows it to defer the Signal operation when
        // there's an active chain lock transaction. So even though we don't *need* a
        // chain lock transaction, we establish one anyway in order to leverage the
        // deferred Signal behavior and avoid a re-entrancy issue.
        //
        // TODO(https://fxbug.dev/331847876): Remove this hack once we have a better
        // solution to scheduler re-entrancy issues.
        ChainLockTransaction::until_done(
            NO_IRQ_SAVE_OPTION,
            clt_tag!("Scheduler::Dump"),
            |_token| {
                let sched = self as *const Scheduler as *mut Scheduler;
                // SAFETY: `self` is valid, interrupts are disabled by caller contract,
                // and `queue_lock_` is not already held.
                let _guard = unsafe { QueueLockGuard::acquire(sched) };

                // SAFETY: `_guard` holds `queue_lock_`.
                unsafe {
                    self.dump_header(output_target);
                }

                if !queue_state_only {
                    // SAFETY: `_guard` holds `queue_lock_`, synchronizing access to
                    // `active_thread` and the three run queues.
                    unsafe {
                        let state = self.state.get();
                        let active_thread = (*state).active_thread;
                        if !active_thread.is_null() {
                            self.assert_in_scheduler(active_thread);
                            self.dump_thread(output_target, active_thread, true);
                        }
                        for run_queue in [
                            &raw const (*state).critical_deadline_run_queue,
                            &raw const (*state).deadline_run_queue,
                            &raw const (*state).fair_run_queue,
                        ] {
                            let mut iter = rust_kernel_run_queue_iterator_create(run_queue);
                            while rust_kernel_run_queue_iterator_is_valid(&iter) {
                                let thread = rust_kernel_run_queue_iterator_get(&iter);
                                self.assert_in_scheduler(thread);
                                self.dump_thread(output_target, thread, false);
                                rust_kernel_run_queue_iterator_next(&mut iter);
                            }
                        }
                    }
                }

                DONE
            },
        );
    }
}

/// Dumps the state of the run queue to the specified output target. Note that
/// interrupts must be disabled in order to call this function.
///
/// # Safety
/// The caller must ensure `sched` and `output_target` are valid pointers and
/// that `output_target` is not aliased for the duration of the call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_dump(
    sched: *mut Scheduler,
    output_target: *mut libc::stdio::FILE,
    queue_state_only: bool,
) {
    // SAFETY: `sched` and `output_target` are valid and unaliased per the caller's
    // contract; interrupts are disabled and `queue_lock_` is not held.
    unsafe { (*sched).dump_to(&mut *output_target, queue_state_only) };
}

/// Dumps info about the currently active thread (if any). Note that
/// interrupts must be disabled in order to call this function.
///
/// # Safety
/// The caller must ensure `sched` and `output_target` are valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_dump_active_thread(
    sched: *mut Scheduler,
    output_target: *mut libc::stdio::FILE,
) {
    // See comment in `rust_kernel_scheduler_dump`.
    //
    // TODO(https://fxbug.dev/331847876): Remove this hack once we have a better
    // solution to scheduler re-entrancy issues.
    ChainLockTransaction::until_done(
        NO_IRQ_SAVE_OPTION,
        clt_tag!("Scheduler::DumpActiveThread"),
        |_token| {
            // SAFETY: `sched` is valid, interrupts are disabled by caller contract,
            // and `queue_lock_` is not already held.
            let _guard = unsafe { QueueLockGuard::acquire(sched) };
            // SAFETY: `sched` and `output_target` are valid and `_guard` holds `queue_lock_`.
            unsafe {
                let active_thread = (*(*sched).state.get()).active_thread;
                if !active_thread.is_null() {
                    (*sched).assert_in_scheduler(active_thread);
                    cpp_thread_dump_active(active_thread, output_target);
                }
            }
            DONE
        },
    );
}

/// Peeks at the active scheduler mask. See `peek_active_mask`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_scheduler_peek_active_mask() -> CpuMask {
    peek_active_mask()
}

/// Peeks at the idle scheduler mask. See `peek_idle_mask`.
///
/// # Safety
/// No preconditions; this is a lockless atomic load.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_peek_idle_mask() -> CpuMask {
    peek_idle_mask()
}

/// Sets or clears `sched`'s bit in the idle scheduler mask. See `Scheduler::set_idle`.
///
/// # Safety
/// `sched` must point to a valid `Scheduler` whose `this_cpu` has been initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_set_idle(sched: *const Scheduler, is_idle: bool) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).set_idle(is_idle) };
}

/// Sets or clears the current CPU's bit in the active scheduler mask. See
/// `Scheduler::set_curr_cpu_active`.
///
/// # Safety
/// The current CPU's `Scheduler` must be initialized and its `queue_lock_` must
/// not already be held by the current CPU. Interrupts must be disabled, or the
/// calling thread must otherwise be unable to migrate to another CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_set_curr_cpu_active(is_active: bool) {
    // SAFETY: Forwarded from the caller.
    unsafe { Scheduler::set_curr_cpu_active(is_active) };
}

/// Returns true if new wakeup accounting is enabled. See `enable_new_wakeup_accounting`.
///
/// # Safety
/// No preconditions beyond boot options having been initialized.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_enable_new_wakeup_accounting() -> bool {
    enable_new_wakeup_accounting()
}

/// Unconditionally validates `sched`'s invariants. See
/// `Scheduler::validate_invariants_unconditional`.
///
/// # Safety
/// `sched` must point to a valid `Scheduler` whose `queue_lock_` is held by the
/// current CPU, and a `ChainLockTransaction` must be active on the current CPU.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_validate_invariants_unconditional(sched: *const Scheduler) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).validate_invariants_unconditional() };
}

/// # Safety
/// `sched` must be valid and its `queue_lock_` must be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_on_power_domain_set_updated(
    sched: *mut Scheduler,
    active_rate: SchedProcessingRate,
    max_rate: SchedProcessingRate,
    has_domain: bool,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).on_power_domain_set_updated(active_rate, max_rate, has_domain) };
}

/// # Safety
/// `sched` must be valid and its `queue_lock_` must NOT be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_active_power_level(
    sched: *mut Scheduler,
    power_level: u8,
) -> zx_types::zx_status_t {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).update_active_power_level(power_level) }
}

/// # Safety
/// `sched` must be valid during early boot topology init.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_initialize_processing_rate(
    sched: *mut Scheduler,
    scale: SchedProcessingRate,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).initialize_processing_rate(scale) };
}

/// # Safety
/// `info` must point to `count` valid `zx_cpu_performance_info_t` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_get_performance_scales(
    info: *mut zx_types::zx_cpu_performance_info_t,
    count: usize,
) {
    // SAFETY: When `count > 0`, the caller guarantees `info` points to `count` valid elements.
    let slice =
        if count == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(info, count) } };
    // SAFETY: Per-CPU schedulers are initialized and `queue_lock_` is not held by the caller.
    unsafe { Scheduler::get_performance_scales(slice) };
}

/// # Safety
/// `info` must point to `count` valid `zx_cpu_performance_info_t` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_get_default_performance_scales(
    info: *mut zx_types::zx_cpu_performance_info_t,
    count: usize,
) {
    // SAFETY: When `count > 0`, the caller guarantees `info` points to `count` valid elements.
    let slice =
        if count == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(info, count) } };
    // SAFETY: Per-CPU schedulers are initialized and `queue_lock_` is not held by the caller.
    unsafe { Scheduler::get_default_performance_scales(slice) };
}

/// # Safety
/// `limits` must point to `count` valid `zx_cpu_perf_limit_t` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_get_processing_limits(
    limits: *mut zx_types::zx_cpu_perf_limit_t,
    count: usize,
) {
    // SAFETY: When `count > 0`, the caller guarantees `limits` points to `count` valid elements.
    let slice = if count == 0 {
        &mut []
    } else {
        unsafe { core::slice::from_raw_parts_mut(limits, count) }
    };
    // SAFETY: Per-CPU schedulers are initialized and `queue_lock_` is not held by the caller.
    unsafe { Scheduler::get_processing_limits(slice) };
}

/// Blocks `current_thread` in the scheduler.
///
/// The scheduler drops `current_thread`'s lock during the context switch and re-acquires it
/// before returning when `current_thread` becomes runnable again.
///
/// # Safety
///
/// The caller must ensure that:
/// - `current_thread.owner()` is the currently executing [`Thread`] on the current CPU in a
///   blocked state (`Blocked` or `BlockedReadLock`), and blocking is permitted in the current
///   context (no spinlocks held and blocking not disallowed).
/// - The active transaction is finalized with `current_thread.owner().lock()` as the only held
///   chain lock.
#[inline]
pub unsafe fn block(
    current_thread: &mut ChainLockProof<'_, Thread>,
    clt_token: &ChainLockTransactionToken,
) {
    assert_transaction_active(clt_token);
    // SAFETY: `current_thread` proves `current_thread.owner().lock()` is held in the active
    // transaction, and the caller upholds the scheduler blocking preconditions.
    unsafe {
        cpp_scheduler_block(current_thread.owner().as_raw());
    }
}

/// Finishes unblocking `thread`, placing it into a compatible scheduler's run queue, releasing
/// its lock, and rescheduling if needed/permitted.
///
/// # Safety
///
/// The caller must ensure that:
/// - `thread.owner()` is a blocked thread that has been dequeued from its wait queue and is
///   ready to be placed into a scheduler run queue.
/// - The active transaction is finalized.
#[inline]
pub unsafe fn unblock(thread: OwnerGuard<'_, Thread>, clt_token: &ChainLockTransactionToken) {
    assert_transaction_active(clt_token);
    let thread = thread.into_unguarded();
    // SAFETY: `thread.lock()` is held via the consumed `OwnerGuard` in the active transaction,
    // and the caller guarantees `thread` is ready to be unblocked in the scheduler.
    unsafe {
        cpp_scheduler_unblock(thread.as_raw());
    }
}

/// Finishes unblocking all threads in `list`, placing each into a compatible scheduler's run
/// queue, releasing its lock, and rescheduling if needed/permitted.
///
/// `list` is drained and left empty on return.
///
/// # Safety
///
/// The caller must ensure that:
/// - Every [`Thread`] in `list` is a non-idle blocked thread that has been dequeued from its wait
///   queue, has its lock held in the active transaction (marked as needing release via
///   `mark_released` / `assert_acquired`), and is ready to be placed into a scheduler run queue
///   (passing ownership of releasing each thread's lock to the scheduler).
/// - The active transaction is finalized.
#[inline]
pub unsafe fn unblock_list(
    list: Pin<&mut ThreadUnblockList>,
    clt_token: &ChainLockTransactionToken,
) {
    assert_transaction_active(clt_token);
    // SAFETY: Every thread in `list` has its lock held in the finalized active transaction and is
    // ready to be unblocked in the scheduler, and `cpp_scheduler_unblock_list` moves the nodes out
    // of `list` in-place, leaving `list` as a valid empty `Thread::UnblockList`.
    unsafe {
        cpp_scheduler_unblock_list(list.as_raw().cast());
    }
}

/// Kernel unit tests for Rust scheduler components.
#[cfg(ktest)]
#[unittest::suite(name = "scheduler_rust")]
mod tests {
    use super::{
        Affine, BandwidthReservationCache, BaseProfile, EffectiveProfile, SchedDeadlineParams,
        SchedDuration, SchedTime, SchedUtilization, SchedWeight, Slope, sched_ms, sched_ns,
    };

    // The kernel-side counterpart of `//zircon/kernel/lib/sched/affine-tests.cc`.
    /// Verifies `Affine` timeline transformation properties.
    #[test]
    fn test_affine_properties() {
        let error_limit: i64 = 1i64 << Affine::K_PRECISION_BITS;
        let mut affine = Affine::default();

        unittest::expect_eq!(affine.slope.raw_value(), Affine::K_MAX_SLOPE.raw_value());
        unittest::expect_eq!(affine.monotonic_reference_time.raw_value(), 0);
        unittest::expect_eq!(affine.variable_reference_time.raw_value(), 0);

        let min_time = SchedTime::min();
        let max_time = SchedTime::max();

        // The timelines should be identical in the initial state.
        unittest::expect_eq!(
            affine.monotonic_to_variable(min_time).raw_value(),
            min_time.raw_value()
        );
        unittest::expect_eq!(
            affine.monotonic_to_variable(max_time).raw_value(),
            max_time.raw_value()
        );
        unittest::expect_eq!(
            affine.variable_to_monotonic(min_time).raw_value(),
            min_time.raw_value()
        );
        unittest::expect_eq!(
            affine.variable_to_monotonic(max_time).raw_value(),
            max_time.raw_value()
        );

        // Deterministic PRNG (xorshift64*) for reproducible property testing in-kernel.
        let mut rng_state: u64 = 0x853c_49e6_748f_ea9b ^ (Affine::K_PRECISION_BITS as u64);
        let mut next_u64 = || -> u64 {
            rng_state ^= rng_state >> 12;
            rng_state ^= rng_state << 25;
            rng_state ^= rng_state >> 27;
            rng_state.wrapping_mul(0x2545_f491_4f6c_dd1d)
        };

        for _ in 0..256 {
            let x = SchedTime::from_raw(next_u64() as i64);
            unittest::expect_eq!(affine.monotonic_to_variable(x).raw_value(), x.raw_value());
            unittest::expect_eq!(affine.variable_to_monotonic(x).raw_value(), x.raw_value());
        }

        let interval_count: i64 = 200;
        let interval_size = max_time.raw_value() / interval_count;
        let min_slope_raw = Affine::K_MIN_SLOPE.raw_value();
        let max_slope_raw = Affine::K_MAX_SLOPE.raw_value();
        let slope_span = (max_slope_raw - min_slope_raw + 1) as u64;

        let mut max_error: i64 = 0;
        for i in 0..interval_count {
            let interval_start = i * interval_size;
            let interval_end = interval_start + interval_size - 1;
            let span = (interval_end - interval_start + 1) as u64;
            let reference_time = SchedTime::from_raw(interval_start + (next_u64() % span) as i64);

            let slope = Slope::from_raw(min_slope_raw + (next_u64() % slope_span) as i64);

            let previous_affine = affine;
            affine.change_slope_at_monotonic_time(reference_time, slope);

            unittest::expect_eq!(
                previous_affine.monotonic_to_variable(reference_time).raw_value(),
                affine.monotonic_to_variable(reference_time).raw_value()
            );

            let rem_span = (interval_end - reference_time.raw_value() + 1) as u64;
            for _ in 0..8 {
                let monotonic = SchedTime::from_raw(
                    reference_time.raw_value() + (next_u64() % rem_span) as i64,
                );
                let variable_from_monotonic = affine.monotonic_to_variable(monotonic);
                let monotonic_from_variable = affine.variable_to_monotonic(variable_from_monotonic);
                let error = (monotonic_from_variable - monotonic).raw_value().abs();
                if error > max_error {
                    max_error = error;
                }
            }
        }

        unittest::expect_le!(max_error, error_limit);
    }

    /// Verifies `EffectiveProfile::get_weight_or_packed_deadline_params` packing logic.
    #[test]
    fn test_effective_profile_packing_logic() {
        let make_deadline_ep = |capacity_ns: SchedDuration, deadline_ns: SchedDuration| {
            let mut ep = EffectiveProfile::new(&BaseProfile::new_fair(0));
            ep.set_deadline(
                SchedDeadlineParams {
                    capacity_ns,
                    deadline_ns,
                    utilization: SchedUtilization::from_raw(0),
                },
                false,
            );
            ep
        };

        // Test case 1: Nominal deadline values (multiple of 10us)
        // Capacity 1ms (100 in 10us units), Deadline 10ms (1000 in 10us units)
        // Expected packed value: -1001000
        {
            let mut ep = make_deadline_ep(sched_ms(1), sched_ms(10));
            unittest::expect_eq!(ep.get_weight_or_packed_deadline_params(), -1_001_000);
        }

        // Test case 2: Unaligned capacity
        // Capacity 1ms + 5us (1,005,000 ns) -> 100 units; Deadline 10ms -> 1000 units.
        {
            let mut ep = make_deadline_ep(sched_ns(1_005_000), sched_ms(10));
            unittest::expect_eq!(ep.get_weight_or_packed_deadline_params(), -1_001_000);
        }

        // Test case 3: Clamping small values (< 10us -> 1 unit)
        {
            let mut ep = make_deadline_ep(sched_ns(1_000), sched_ns(2_000));
            unittest::expect_eq!(ep.get_weight_or_packed_deadline_params(), -10_001);
        }

        // Test case 4: Clamping large values (1s -> 9,999 units)
        {
            let mut ep = make_deadline_ep(sched_ms(1_000), sched_ms(1_000));
            unittest::expect_eq!(ep.get_weight_or_packed_deadline_params(), -99_999_999);
        }

        // Test case 5: Fair weights
        {
            let mut ep = EffectiveProfile::new(&BaseProfile::new_fair(0));
            ep.set_fair(SchedWeight::from_raw(0));
            unittest::expect_eq!(ep.get_weight_or_packed_deadline_params(), 0);

            ep.set_fair(SchedWeight::from_raw(12345));
            unittest::expect_eq!(ep.get_weight_or_packed_deadline_params(), 12345);
        }
    }

    /// Verifies `BandwidthReservationCache::add` insertion, eviction, and update behavior.
    #[test]
    fn test_bandwidth_reservation_cache_add() {
        let mut cache = BandwidthReservationCache::<4>::new();
        let t = SchedTime::from_raw;
        let u = SchedUtilization::from_raw;

        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);

        unittest::expect_eq!(cache.add(1, t(10), u(1)).raw_value(), 0);
        unittest::expect_eq!(cache.add(2, t(20), u(2)).raw_value(), 0);
        unittest::expect_eq!(cache.add(3, t(30), u(3)).raw_value(), 0);
        unittest::expect_eq!(cache.add(4, t(40), u(4)).raw_value(), 0);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 10);

        // Evicts entry with latest finish time (tid 4, finish 40, util 4).
        unittest::expect_eq!(cache.add(5, t(15), u(5)).raw_value(), 4);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 11);

        // Later finish time than all existing entries has no effect.
        unittest::expect_eq!(cache.add(6, t(60), u(6)).raw_value(), 6);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 11);

        // Updating existing tid returns previous reservation.
        unittest::expect_eq!(cache.add(5, t(25), u(6)).raw_value(), 5);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 12);
    }

    /// Verifies `BandwidthReservationCache::prune` removal of expired reservations.
    #[test]
    fn test_bandwidth_reservation_cache_prune() {
        let mut cache = BandwidthReservationCache::<4>::new();
        let t = SchedTime::from_raw;
        let u = SchedUtilization::from_raw;

        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);

        unittest::expect_eq!(cache.add(1, t(10), u(1)).raw_value(), 0);
        unittest::expect_eq!(cache.add(2, t(20), u(2)).raw_value(), 0);
        unittest::expect_eq!(cache.add(3, t(30), u(3)).raw_value(), 0);
        unittest::expect_eq!(cache.add(4, t(40), u(4)).raw_value(), 0);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 10);

        // Pruning to earlier than any finish time should have no effect.
        unittest::expect_eq!(cache.prune(t(5)).raw_value(), 0);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 10);

        unittest::expect_eq!(cache.prune(t(10)).raw_value(), 1);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 9);

        unittest::expect_eq!(cache.prune(t(25)).raw_value(), 2);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 7);

        // Pruning should remove all entries with earlier finish times.
        unittest::expect_eq!(cache.prune(t(50)).raw_value(), 7);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);
    }

    /// Verifies `BandwidthReservationCache::clear` removal of all reservations.
    #[test]
    fn test_bandwidth_reservation_cache_clear() {
        let mut cache = BandwidthReservationCache::<4>::new();
        let t = SchedTime::from_raw;
        let u = SchedUtilization::from_raw;

        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);

        unittest::expect_eq!(cache.add(1, t(10), u(1)).raw_value(), 0);
        unittest::expect_eq!(cache.add(2, t(20), u(2)).raw_value(), 0);
        unittest::expect_eq!(cache.add(3, t(30), u(3)).raw_value(), 0);
        unittest::expect_eq!(cache.add(4, t(40), u(4)).raw_value(), 0);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 10);

        // Clearing the cache should remove all reservations and deferred utilization.
        unittest::expect_eq!(cache.clear().raw_value(), 10);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);
    }

    /// Verifies `BandwidthReservationCache::clamp_to_next_finish_time` clamping behavior.
    #[test]
    fn test_bandwidth_reservation_cache_clamp_to_next_finish_time() {
        let mut cache = BandwidthReservationCache::<4>::new();
        let t = SchedTime::from_raw;
        let u = SchedUtilization::from_raw;

        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);

        unittest::expect_eq!(cache.add(1, t(10), u(1)).raw_value(), 0);
        unittest::expect_eq!(cache.add(2, t(20), u(2)).raw_value(), 0);
        unittest::expect_eq!(cache.add(3, t(30), u(3)).raw_value(), 0);
        unittest::expect_eq!(cache.add(4, t(40), u(4)).raw_value(), 0);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 10);

        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(5)).raw_value(), 5);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(10)).raw_value(), 10);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(20)).raw_value(), 10);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(30)).raw_value(), 10);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(40)).raw_value(), 10);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(50)).raw_value(), 10);

        unittest::expect_eq!(cache.remove(1).raw_value(), 1);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 9);

        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(5)).raw_value(), 5);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(10)).raw_value(), 10);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(20)).raw_value(), 20);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(30)).raw_value(), 20);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(40)).raw_value(), 20);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(50)).raw_value(), 20);

        unittest::expect_eq!(cache.clear().raw_value(), 9);

        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(5)).raw_value(), 5);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(10)).raw_value(), 10);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(20)).raw_value(), 20);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(30)).raw_value(), 30);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(40)).raw_value(), 40);
        unittest::expect_eq!(cache.clamp_to_next_finish_time(t(50)).raw_value(), 50);
    }

    /// Verifies `BandwidthReservationCache` mixed add, evict, prune, and remove operations.
    #[test]
    fn test_bandwidth_reservation_cache_mix() {
        let mut cache = BandwidthReservationCache::<4>::new();
        let t = SchedTime::from_raw;
        let u = SchedUtilization::from_raw;

        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);

        unittest::expect_eq!(cache.add(1, t(10), u(1)).raw_value(), 0);
        unittest::expect_eq!(cache.add(2, t(20), u(2)).raw_value(), 0);
        unittest::expect_eq!(cache.add(3, t(30), u(3)).raw_value(), 0);
        unittest::expect_eq!(cache.add(4, t(40), u(4)).raw_value(), 0);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 10);

        // Adding another reservation with an earlier finish time than at least one
        // existing entry should evict the entry with the latest finish time and
        // return the reservation that was removed.
        unittest::expect_eq!(cache.add(5, t(15), u(5)).raw_value(), 4);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 11);

        // Attempting to add another reservation with a later finish time than any
        // existing entry in the cache should have no effect.
        unittest::expect_eq!(cache.add(6, t(60), u(6)).raw_value(), 6);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 11);

        // Prune the reservations with finish times less than or equal to 20,
        // returning the total bandwidth reservation removed.
        unittest::expect_eq!(cache.prune(t(20)).raw_value(), 8);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 3);

        // Removing an entry that is not in the cache should have no effect.
        unittest::expect_eq!(cache.remove(6).raw_value(), 0);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 3);

        // Removing an existing entry should return the reservation that was removed.
        unittest::expect_eq!(cache.remove(3).raw_value(), 3);
        unittest::expect_eq!(cache.total_deferred_utilization().raw_value(), 0);
    }
}
