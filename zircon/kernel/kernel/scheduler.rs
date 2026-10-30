// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::owned_wait_queue::{OwnedWaitQueue, ThreadUnblockList};
use super::percpu::PerCpu;
use super::relaxed_atomic::RelaxedAtomicI64;
use super::scheduler_state::{
    HIGHEST_PRIORITY, IDLE_PRIORITY, LOWEST_PRIORITY, ThreadStateKind as ThreadState,
};
use super::stats::CpuStats;
use super::thread::{Thread, ThreadName};
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

/// Diagnostic state reported when the target preemption time is inconsistent
/// with the current time slice in `Scheduler::reschedule_common_helper`.
struct PreemptionTimeDiagnostics {
    cpu: CpuNum,
    capacity_ns: SchedDuration,
    deadline_ns: SchedDuration,
    finish_time: SchedTime,
    now: SchedTime,
    target_preemption_time_ns: SchedTime,
    start_of_current_time_slice_ns: SchedTime,
    total_runtime_ns: SchedDuration,
    actual_runtime_ns: SchedDuration,
    time_slice_ns: SchedDuration,
    time_slice_used_ns: SchedDuration,
    remaining_time_slice_ns: SchedDuration,
}

/// Panics with the diagnostics logged by the C++ `DEBUG_ASSERT_MSG` in
/// `RescheduleCommon`. Kept out of line and cold so that the formatting state
/// does not occupy the stack frame of the reschedule path.
#[cold]
#[inline(never)]
fn panic_preemption_time_inconsistent(d: &PreemptionTimeDiagnostics) -> ! {
    panic!(
        "cpu={} capacity_ns={} deadline_ns={}\n\
         finish_time                   ={:16}\n\
         now                           ={:16}\n\
         target_preemption_time_ns     ={:16}\n\
         start_of_current_time_slice_ns={:16}\n\
         total_runtime_ns              ={:16}\n\
         actual_runtime_ns             ={:16}\n\
         time_slice_ns                 ={:16}\n\
         time_slice_used_ns            ={:16}\n\
         remaining_time_slice_ns       ={:16}",
        d.cpu,
        d.capacity_ns.raw_value(),
        d.deadline_ns.raw_value(),
        d.finish_time.raw_value(),
        d.now.raw_value(),
        d.target_preemption_time_ns.raw_value(),
        d.start_of_current_time_slice_ns.raw_value(),
        d.total_runtime_ns.raw_value(),
        d.actual_runtime_ns.raw_value(),
        d.time_slice_ns.raw_value(),
        d.time_slice_used_ns.raw_value(),
        d.remaining_time_slice_ns.raw_value()
    );
}

/// Logs the critical deadline utilization warning emitted by `Scheduler::insert`.
///
/// Kept out of line and cold so that the formatting state does not occupy the
/// stack frame of the hot insert path.
#[cold]
#[inline(never)]
fn warn_critical_deadline_utilization_exceeded(utilization: SchedUtilization, cpu: CpuNum) {
    debug::dprintf!(
        CRITICAL,
        "SCHEDULER WARNING: Critical deadline utilization ({}) exceeds limit ({}) on CPU {}\n",
        utilization,
        K_CPU_UTILIZATION_LIMIT,
        cpu,
    );
}

/// Reports a thread with no CPU available to run on; see `Scheduler::find_target_cpu`.
///
/// Kept out of line and cold so that the formatting state does not occupy the
/// stack frame of the hot wakeup path.
///
/// # Safety
/// `thread` must point to a valid `Thread`.
#[cold]
#[inline(never)]
unsafe fn panic_no_available_cpu(
    thread: *const Thread,
    hard_affinity: CpuMask,
    soft_affinity: CpuMask,
    active_mask: CpuMask,
) -> ! {
    panic!(
        "thread={} affinity={:#x} soft_affinity={:#x} active={:#x} idle={:#x} \
         arch_ints_disabled={}",
        // SAFETY: `thread` is valid per the caller's contract.
        unsafe { thread_name(thread) },
        hard_affinity,
        soft_affinity,
        active_mask,
        peek_idle_mask(),
        crate::arch_rs::ints_disabled(),
    );
}

/// Copies the thread's name into a stack-allocated buffer using byte-wise
/// relaxed atomic loads so that concurrent calls to `Thread::set_name` (which
/// does not synchronize with scheduler locks) cannot cause a data race or
/// out-of-bounds read.
///
/// # Safety
/// `thread` must be null or point to a valid `Thread`.
#[inline]
unsafe fn thread_name(thread: *const Thread) -> ThreadName {
    if thread.is_null() {
        core::hint::cold_path();
        ThreadName::empty()
    } else {
        // SAFETY: `thread` is non-null and valid per the caller's contract.
        unsafe { Thread::name_copy_ptr(thread) }
    }
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

/// Values computed by `reschedule_common_helper` and consumed by the remainder
/// of `reschedule_common`.
///
/// The idle flags are latched once so that the reschedule path does not repeat
/// loads of `Thread::flags_` (which cannot be CSE'd across the intervening
/// calls) for values that are invariant for the duration of the operation.
#[derive(Debug, Clone, Copy)]
pub struct RustRescheduleResult {
    pub need_context_switch: bool,
    pub timeslice_expired: bool,
    pub current_is_migrating: bool,
    pub next_thread: *mut Thread,
    /// Whether `current_thread` is the idle thread.
    pub curr_is_idle: bool,
    /// Whether `next_thread` is the idle thread.
    pub next_is_idle: bool,
}

/// A utility result type to help avoid missing updates to a thread's
/// start/finish times when dequeuing from a run queue with a variable
/// timeline.
#[repr(C)]
#[derive(Debug)]
pub struct EvaluateResult {
    pub status: EvaluateStatus,
    pub thread: *mut Thread,
    pub mono_start_time_ns: SchedTime,
    pub mono_finish_time_ns: SchedTime,
}

impl EvaluateResult {
    /// Constructs an empty result.
    pub fn empty() -> Self {
        Self {
            status: EvaluateStatus::Idle,
            thread: core::ptr::null_mut(),
            mono_start_time_ns: SchedTime::from_raw(0),
            mono_finish_time_ns: SchedTime::from_raw(0),
        }
    }

    /// Constructs a selected result from a `DequeueResult`, preserving any
    /// pending update to start/finish times.
    pub fn selected(mut res: DequeueResult) -> Self {
        let out = Self {
            status: EvaluateStatus::Selected,
            thread: res.thread,
            mono_start_time_ns: res.mono_start_time_ns,
            mono_finish_time_ns: res.mono_finish_time_ns,
        };
        res.mono_start_time_ns = SchedTime::from_raw(0);
        res.mono_finish_time_ns = SchedTime::from_raw(0);
        out
    }

    /// Updates the thread's start/finish times while holding the thread's lock.
    /// May be called unconditionally and more than once, whether or not there
    /// are pending updates.
    ///
    /// # Safety
    ///
    /// The caller must ensure `self.thread` is a valid `Thread` pointer and its lock is held.
    pub unsafe fn update_thread_timeline(&mut self) {
        if self.is_updated_pending() {
            // SAFETY: `self.thread` is valid and its lock is held exclusively by the caller.
            let state = unsafe { &mut *cpp_thread_get_scheduler_state(self.thread) };
            state.start_time = self.mono_start_time_ns;
            state.finish_time = self.mono_finish_time_ns;
            self.mono_start_time_ns = SchedTime::from_raw(0);
            self.mono_finish_time_ns = SchedTime::from_raw(0);
        }
    }

    /// Returns true if the thread's timeline needs to be updated by calling
    /// `update_thread_timeline`.
    pub fn is_updated_pending(&self) -> bool {
        self.mono_start_time_ns.raw_value() != 0 || self.mono_finish_time_ns.raw_value() != 0
    }

    pub fn steal_work() -> Self {
        Self {
            status: EvaluateStatus::StealWork,
            thread: core::ptr::null_mut(),
            mono_start_time_ns: SchedTime::from_raw(0),
            mono_finish_time_ns: SchedTime::from_raw(0),
        }
    }
}

impl Drop for EvaluateResult {
    fn drop(&mut self) {
        debug_assert!(
            !self.is_updated_pending(),
            "Destroyed an EvaluateResult with a pending timeline update!"
        );
    }
}

/// A utility result type to help avoid missing updates to a thread's
/// start/finish times when dequeuing from a run queue with a variable
/// timeline.
#[repr(C)]
#[derive(Debug)]
pub struct DequeueResult {
    pub thread: *mut Thread,
    pub mono_start_time_ns: SchedTime,
    pub mono_finish_time_ns: SchedTime,
}

impl DequeueResult {
    /// Constructs an empty result.
    pub fn empty() -> Self {
        Self {
            thread: core::ptr::null_mut(),
            mono_start_time_ns: SchedTime::from_raw(0),
            mono_finish_time_ns: SchedTime::from_raw(0),
        }
    }

    /// Constructs a result without a pending timeline update. If `thread` is
    /// null, an empty result is constructed.
    pub fn from_thread(thread: *mut Thread) -> Self {
        Self {
            thread,
            mono_start_time_ns: SchedTime::from_raw(0),
            mono_finish_time_ns: SchedTime::from_raw(0),
        }
    }

    /// Constructs a result with a pending timeline update.
    pub fn with_pending_update(
        thread: *mut Thread,
        mono_start_time_ns: SchedTime,
        mono_finish_time_ns: SchedTime,
    ) -> Self {
        debug_assert!(!thread.is_null());
        debug_assert!(mono_start_time_ns.raw_value() != 0 || mono_finish_time_ns.raw_value() != 0);
        Self { thread, mono_start_time_ns, mono_finish_time_ns }
    }

    /// Updates the thread's start/finish times while holding the thread's lock.
    /// May be called unconditionally and more than once, whether or not there
    /// are pending updates.
    ///
    /// # Safety
    ///
    /// The caller must ensure `self.thread` is a valid `Thread` pointer and its lock is held.
    pub unsafe fn update_thread_timeline(&mut self) {
        if self.is_updated_pending() {
            // SAFETY: `self.thread` is valid and its lock is held exclusively by the caller.
            let state = unsafe { &mut *cpp_thread_get_scheduler_state(self.thread) };
            state.start_time = self.mono_start_time_ns;
            state.finish_time = self.mono_finish_time_ns;
            self.mono_start_time_ns = SchedTime::from_raw(0);
            self.mono_finish_time_ns = SchedTime::from_raw(0);
        }
    }

    /// Returns true if the thread's timeline needs to be updated by calling
    /// `update_thread_timeline`.
    pub fn is_updated_pending(&self) -> bool {
        self.mono_start_time_ns.raw_value() != 0 || self.mono_finish_time_ns.raw_value() != 0
    }
}

impl Drop for DequeueResult {
    fn drop(&mut self) {
        debug_assert!(
            !self.is_updated_pending(),
            "Destroyed a DequeueResult with a pending timeline update!"
        );
    }
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

impl SchedDeadlineParams {
    /// Constructs a new [`SchedDeadlineParams`] from `capacity_ns` and `deadline_ns`.
    ///
    /// # Panics
    ///
    /// Panics if `deadline_ns` is zero, and asserts in debug builds that
    /// `0 < capacity_ns <= deadline_ns`.
    #[inline]
    pub fn new(capacity_ns: SchedDuration, deadline_ns: SchedDuration) -> Self {
        debug_assert!(SchedDuration::from_raw(0) < capacity_ns);
        debug_assert!(capacity_ns <= deadline_ns);
        Self {
            capacity_ns,
            deadline_ns,
            utilization: SchedUtilization::from(capacity_ns / deadline_ns),
        }
    }

    /// Creates a `SchedDeadlineParams` with explicit `utilization`, matching C++
    /// `SchedDeadlineParams(capacity_ns, deadline_ns, utilization)`.
    #[inline]
    pub fn with_utilization(
        capacity_ns: SchedDuration,
        deadline_ns: SchedDuration,
        utilization: SchedUtilization,
    ) -> Self {
        debug_assert!(SchedDuration::from_raw(0) < capacity_ns);
        debug_assert!(capacity_ns <= deadline_ns);
        // Because different code paths either use capacity and deadline to derive
        // utilization or utilization and deadline to derive capacity, there can be
        // small differences, due to rounding, between the resulting utilization
        // values, even when the capacity and deadline values are the same here.
        // Checking consistency requires verifying that at least one relationship is
        // valid.
        debug_assert!(
            utilization == SchedUtilization::from(capacity_ns / deadline_ns)
                || capacity_ns == SchedDuration::from(utilization * deadline_ns),
            "{} = {}/{} ; {} = {}*{}",
            SchedUtilization::from(capacity_ns / deadline_ns).raw_value(),
            capacity_ns.raw_value(),
            deadline_ns.raw_value(),
            SchedDuration::from(utilization * deadline_ns).raw_value(),
            utilization.raw_value(),
            deadline_ns.raw_value(),
        );
        Self { capacity_ns, deadline_ns, utilization }
    }
}

impl Default for SchedDeadlineParams {
    #[inline]
    fn default() -> Self {
        Self {
            capacity_ns: SchedDuration::from_raw(0),
            deadline_ns: SchedDuration::from_raw(0),
            utilization: SchedUtilization::from_raw(0),
        }
    }
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
/// A `&SchedulerState` (or `&EffectiveProfile`) may therefore only be formed
/// while the thread's lock is held; with only `queue_lock_` held, read the
/// queue-guarded fields and the stable `effective_profile` parameters through
/// a raw pointer (`(*state_ptr).field`, `EffectiveProfile::discipline_raw`,
/// `EffectiveProfile::deadline_raw`, `SchedulerState::get_effective_cpu_mask_raw`),
/// since another CPU holding the thread's lock may concurrently write the
/// thread-locked fields of the same object. This is the Rust counterpart of
/// the C++ `MarkHasOwnedThreadAccess` idiom, with the reference restriction
/// that Rust's aliasing rules additionally require.
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

    /// Recomputes `self` from `bp` and `ipv`.
    /// This should only ever be called while holding the thread's lock.
    pub fn recompute(&mut self, bp: &BaseProfile, ipv: &InheritedProfileValues) {
        self.assert_dirty();
        ipv.assert_consistency();

        if bp.is_deadline() {
            let bp_deadline = bp.deadline();
            let total_util = ipv.uncapped_utilization + bp_deadline.utilization;
            let new_util = core::cmp::min(total_util, K_THREAD_UTILIZATION_MAX);
            let new_deadline = core::cmp::min(ipv.min_deadline, bp_deadline.deadline_ns);
            let new_capacity = SchedDuration::from(new_util * new_deadline);

            self.set_deadline(
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
            self.set_deadline(
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
            self.set_fair(bp.fair_weight() + ipv.total_weight);
        }

        self.clean();
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
    pub const fn convert_priority_to_weight(priority: i32) -> SchedWeight {
        debug_assert!(priority >= LOWEST_PRIORITY && priority <= HIGHEST_PRIORITY);
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
        self.effective_profile.recompute(&self.base_profile, &self.inherited_profile_values);
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
        // SAFETY: `self` is a valid reference, so the affinity fields are stable.
        unsafe { Self::get_effective_cpu_mask_raw(self, active_mask) }
    }

    /// Returns the effective CPU mask of the state at `this` without forming a
    /// reference to it, for use when only the owning scheduler's `queue_lock_` is
    /// held and other fields may be concurrently written by a CPU holding the
    /// thread's lock (see `EffectiveProfile::discipline_raw`).
    ///
    /// # Safety
    /// `this` must point to a valid `SchedulerState` whose `hard_affinity` and
    /// `soft_affinity` are stable for the duration of the call.
    pub unsafe fn get_effective_cpu_mask_raw(this: *const Self, active_mask: CpuMask) -> CpuMask {
        // SAFETY: Reading the affinity fields through the raw pointer per the
        // caller's contract does not create a reference to the whole state.
        let (hard_affinity, soft_affinity) =
            unsafe { ((*this).hard_affinity, (*this).soft_affinity) };

        // The thread may run on any active CPU allowed by both its hard and
        // soft CPU affinity.
        let available_mask = hard_affinity & soft_affinity & active_mask;

        // Return the mask honoring soft affinity if it is viable, otherwise ignore
        // soft affinity and honor only hard affinity.
        if available_mask != 0 { available_mask } else { hard_affinity & active_mask }
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

    fn cpp_thread_set_ready(thread: *mut Thread);
    fn cpp_arch_num_spinlocks_held() -> u32;
    fn cpp_unblock_list_pop_back(list: *mut UnblockList) -> *mut Thread;

    fn cpp_thread_get_queue_node(thread: *mut Thread) -> *mut WavlTreeNode<Thread, bool>;
    fn cpp_thread_get_scheduler_state(thread: *mut Thread) -> *mut SchedulerState;
    fn cpp_thread_get_queue_state(thread: *mut Thread) -> *mut SchedulerQueueState;
    fn cpp_owq_get_inherited_state(
        owq: *mut OwnedWaitQueue,
    ) -> *mut WaitQueueInheritedSchedulerState;
    fn cpp_owq_get_inherited_state_const(
        owq: *const OwnedWaitQueue,
    ) -> *const WaitQueueInheritedSchedulerState;
    fn cpp_scheduler_get_normalized_utilization(sched: *const Scheduler) -> SchedUtilization;
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
        sched: *mut Scheduler,
        request: *const PowerLevelUpdateRequest,
    ) -> CpuMask;
    fn cpp_scheduler_power_state_update_active_power_level(
        sched: *mut Scheduler,
        power_level: u8,
        out_active_rate: *mut SchedProcessingRate,
    ) -> zx_types::zx_status_t;
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
    fn cpp_scheduler_take_save_state_list(
        sched: *mut Scheduler,
        holder: *mut SaveStateList,
    ) -> bool;
    fn cpp_save_state_list_pop(holder: *mut SaveStateList) -> *mut Thread;
    fn cpp_save_state_list_destroy(holder: *mut SaveStateList);

    fn cpp_thread_call_migrate_fn_for_cpu(cpu: CpuNum);
    fn cpp_scheduler_block(current_thread: *mut Thread);
}

/// Returns a pointer to the `ChainLock` embedded in `thread`.
///
/// # Safety
/// `thread` must point to a valid `Thread`.
#[inline(always)]
unsafe fn thread_get_lock(thread: *const Thread) -> *const ChainLock {
    // SAFETY: The caller guarantees `thread` is valid; `Thread::lock_ptr` projects the lock field
    // without forming a `&Thread`, and no guard or proof is formed from it here.
    unsafe { (*Thread::lock_ptr(thread)).as_chain_lock() }
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

// DequeueResult layout parity
zr::static_assert!(core::mem::size_of::<DequeueResult>() == 24);
zr::static_assert!(core::mem::align_of::<DequeueResult>() == 8);
zr::static_assert!(core::mem::offset_of!(DequeueResult, thread) == 0);
zr::static_assert!(core::mem::offset_of!(DequeueResult, mono_start_time_ns) == 8);
zr::static_assert!(core::mem::offset_of!(DequeueResult, mono_finish_time_ns) == 16);

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

/// Returns a tracing string ref for the given preemption type.
fn preempt_type_to_str(t: PreemptType) -> &'static ktrace_rs::InternedString {
    match t {
        PreemptType::Irq => PREEMPT_IRQ,
        PreemptType::FlushPending => PREEMPT_FLUSH_PENDING,
        PreemptType::Reschedule => PREEMPT_RESCHEDULE,
    }
}

impl Scheduler {
    /// Emits queue event traces for trace-based scheduler performance analysis.
    ///
    /// Records details about the threads entering/exiting the run queues for various
    /// CPUs, as well as which task on each CPU is currently active. These events are
    /// used for trace analysis to compute statistics about overall utilization,
    /// taking CPU affinity into account.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread` associated with this scheduler, and
    /// the caller must hold `queue_lock_`.
    unsafe fn trace_thread_queue_event(
        &self,
        name: &'static ktrace_rs::InternedString,
        thread: *const Thread,
    ) {
        // Traces marking the end of a queue/dequeue operation have arguments encoded
        // as follows:
        //
        // arg0[ 0..64] : TID
        //
        // arg1[ 0..15] : CPU availability mask.
        // arg1[16..19] : CPU_ID of the affected queue.
        // arg1[20..27] : Number of runnable tasks on this CPU after the queue event.
        // arg1[28..28] : 1 == fair, 0 == deadline
        // arg1[29..29] : 1 == eligible, 0 == ineligible
        // arg1[30..30] : 1 == idle thread, 0 == normal thread
        //
        if SCHEDULER_QUEUE_TRACING_ENABLED {
            // SAFETY: `thread` is associated with this scheduler and `queue_lock_` is held.
            // We project fields through raw pointers rather than forming `&SchedulerState` or
            // `&EffectiveProfile` because `steal_work` calls this under `queue_lock_` alone
            // while `dirty_flags` may be modified under `thread`'s lock alone.
            unsafe {
                // TODO(johngro): plumb this in from above
                let now = crate::platform_rs::timer::current_mono_time().0;
                let state_ptr = cpp_thread_get_scheduler_state(thread.cast_mut());
                let ep_ptr = &raw const (*state_ptr).effective_profile;
                let is_fair = EffectiveProfile::discipline_raw(ep_ptr) == SchedDiscipline::Fair;
                let eligible = is_fair || ((*state_ptr).start_time.raw_value() <= now);

                let sched_state = self.state.get();
                let active_thread = (*sched_state).active_thread;
                let active_is_not_idle =
                    !active_thread.is_null() && !Thread::is_idle_ptr(active_thread);

                let cnt = (*sched_state).fair_run_queue.len()
                    + (*sched_state).critical_deadline_run_queue.len()
                    + (*sched_state).deadline_run_queue.len()
                    + (if active_is_not_idle { 1 } else { 0 });

                let effective_cpu_mask =
                    SchedulerState::get_effective_cpu_mask_raw(state_ptr, peek_active_mask())
                        as u64;
                let this_cpu = self.this_cpu() as u64;
                let is_idle = Thread::is_idle_ptr(thread);

                let arg0 = Thread::tid_ptr(thread);
                let arg1 = (effective_cpu_mask & 0xFFFF)
                    | (core::cmp::min(this_cpu, 0xF) << 16)
                    | ((core::cmp::min(cnt as u64, 0xFF)) << 20)
                    | ((if is_fair { 1u64 } else { 0u64 }) << 28)
                    | ((if eligible { 1u64 } else { 0u64 }) << 29)
                    | ((if is_idle { 1u64 } else { 0u64 }) << 30);

                ktrace_rs::instant!(
                    "kernel:probe",
                    name,
                    ktrace_rs::Context::Cpu,
                    "arg0" => arg0,
                    "arg1" => arg1
                );
            }
        }
    }

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
                unsafe { Thread::tid_ptr(thread) },
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
                unsafe { Thread::tid_ptr(thread) },
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
                unsafe { Thread::tid_ptr(t) }
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
                            unsafe { Thread::tid_ptr(t) }
                        );
                    }
                    if (ep.dirty_flags & EffectiveProfile::DIRTY_FLAG_INHERITED_DIRTY) == 0 {
                        assert!(
                            ipv.uncapped_utilization == SchedUtilization::from_raw(0),
                            "Fair thread {} has clean IPV, but non-zero inherited utilization ({})",
                            // SAFETY: `t` is valid.
                            unsafe { Thread::tid_ptr(t) },
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
                unsafe { Thread::tid_ptr(t) }
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
                            unsafe { Thread::tid_ptr(t) },
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
        debug_assert!(!unsafe { Thread::is_idle_ptr(thread) });

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
            unsafe { thread_name(current_thread) },
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
    /// Removes the thread from this CPU's scheduler to move it to another CPU
    /// (i.e. stealing or migration). This method does not require the thread to be
    /// locked exclusively, and consequently leaves a stale value for the thread's
    /// current CPU id. The stale value is later corrected when the thread is
    /// locked exclusively and added to the new target CPU by calling `insert`.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread` associated with this scheduler, and
    /// the caller must hold `queue_lock_`.
    unsafe fn remove(&self, now: SchedTime, thread: *mut Thread, stolen_by: CpuNum) {
        let _trace = local_ktrace_begin_scope!(DETAILED, "remove");
        // SAFETY: `thread` is associated with this scheduler and `queue_lock_` is held.
        // We project `SchedulerState` and `EffectiveProfile` fields via raw pointers
        // rather than forming `&mut SchedulerState` or `&EffectiveProfile` because
        // `steal_work` calls `remove` under `queue_lock_` alone while another CPU
        // may hold `thread`'s lock.
        unsafe {
            let queue_state = &mut *cpp_thread_get_queue_state(thread);
            let state_ptr = cpp_thread_get_scheduler_state(thread);
            let ep_ptr = &raw const (*state_ptr).effective_profile;

            debug_assert!(!Thread::is_idle_ptr(thread));
            debug_assert!(!queue_state.in_queue());

            // Ensure that removal happens only once, even if block() is called multiple times.
            if queue_state.on_remove(stolen_by) {
                let expected_runtime_ns = (*state_ptr).expected_runtime_ns;
                let delta = SchedDuration::from_raw(-expected_runtime_ns.raw_value());
                self.update_total_expected_runtime(delta);

                let discipline = EffectiveProfile::discipline_raw(ep_ptr);
                if discipline == SchedDiscipline::Fair {
                    let mut weight_total = self.weight_total();
                    weight_total -= (*ep_ptr).weight;
                    debug_assert!(weight_total.raw_value() >= 0);
                    self.set_weight_total(weight_total);
                    self.update_fair_bandwidth_period(now);
                } else {
                    // Add the thread to the bandwidth demand cache and adjust the total
                    // demand if there was an eviction of another active entry. If the thread
                    // is being stolen, simply remove its demand from the total demand, since
                    // its influence is migrating to another CPU.
                    let deadline = EffectiveProfile::deadline_raw(ep_ptr);
                    let mut utilization_to_remove = deadline.utilization;
                    if self.power_level_control_is_enabled() && stolen_by == INVALID_CPU {
                        utilization_to_remove = self.bandwidth_reservation_cache_add(
                            Thread::tid_ptr(thread),
                            (*state_ptr).finish_time,
                            utilization_to_remove,
                        );
                    }

                    if self.is_critical_deadline_thread(thread) {
                        let mut critical_deadline_utilization =
                            self.critical_deadline_utilization();
                        critical_deadline_utilization -= deadline.utilization;
                        debug_assert!(critical_deadline_utilization.raw_value() >= 0);
                        self.set_critical_deadline_utilization(critical_deadline_utilization);
                    }

                    let delta_util = SchedUtilization::from_raw(-utilization_to_remove.raw_value());
                    self.update_total_deadline_utilization(delta_util);
                }

                let runnable_task_count = self.runnable_task_count();
                debug_assert!(runnable_task_count > 0);
                self.set_runnable_task_count(runnable_task_count - 1);
                self.trace_total_runnable_threads();
            }
        }
    }
    fn arch_yield() {
        libarch::r#yield()
    }
    /// Makes a thread active on this CPU's scheduler and inserts it into the
    /// run queue tree.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread`, and the caller must hold both
    /// `queue_lock_` and `thread`'s lock exclusively.
    unsafe fn insert(&self, now: SchedTime, thread: *mut Thread, placement: Placement) {
        // SAFETY: `thread` is valid and both `queue_lock_` and `thread`'s lock are held
        // exclusively. We use `state_ptr` rather than holding a long-lived `&mut SchedulerState`
        // across `adjust_fair_bandwidth`, `update_activation`, and `queue_thread`, which
        // themselves borrow `thread`'s `SchedulerState`.
        unsafe {
            let queue_state = &mut *cpp_thread_get_queue_state(thread);
            let state_ptr = cpp_thread_get_scheduler_state(thread);
            let start_time = (*state_ptr).start_time;
            let finish_time = (*state_ptr).finish_time;

            let _trace = local_ktrace_begin_scope!(
                DETAILED,
                "insert",
                "start time" => start_time,
                "finish time" => finish_time,
                "expired finish time" => now >= finish_time,
            );

            debug_assert_eq!((*state_ptr).state, ThreadState::Ready);
            debug_assert!(!Thread::is_idle_ptr(thread));
            debug_assert!(self.is_scheduler_active_locked());

            // Ensure insertion happens only once, even if unblock is called multiple times.
            if queue_state.on_insert() {
                // Insertion can happen from a different CPU. Set the thread's current
                // CPU to the one this scheduler instance services.
                (*state_ptr).curr_cpu = self.this_cpu();
                self.update_total_expected_runtime((*state_ptr).expected_runtime_ns);

                if (*state_ptr).effective_profile.is_fair() {
                    let mut weight_total = self.weight_total();
                    weight_total += (*state_ptr).effective_profile.weight;
                    debug_assert!(weight_total.raw_value() > 0);
                    self.set_weight_total(weight_total);

                    self.update_fair_bandwidth_period(now);

                    // If the thread is unblocking, set the start and finish time using the new
                    // period.
                    // TODO(https://fxbug.dev/322207536): Stop resetting start and finish times
                    // when unblocking once we solve races higher in the stack.
                    let enable_new_wakeup = enable_new_wakeup_accounting();
                    if !enable_new_wakeup && placement == Placement::Insertion {
                        let period = (*state_ptr).effective_period();
                        (*state_ptr).start_time = now;
                        (*state_ptr).finish_time = now + period;
                    }
                    let remaining_ns = (*state_ptr).remaining_time_slice_ns();
                    if remaining_ns.raw_value() > 0 && (*state_ptr).finish_time > now {
                        self.adjust_fair_bandwidth(thread, now);
                        // If we're unblocking, update the start and finish time again now that
                        // we've adjusted the fair bandwidth.
                        // TODO(https://fxbug.dev/322207536): Stop resetting start and finish
                        // times when unblocking once we solve races higher in the stack.
                        if !enable_new_wakeup && placement == Placement::Insertion {
                            let period = (*state_ptr).effective_period();
                            (*state_ptr).start_time = now;
                            (*state_ptr).finish_time = now + period;
                        }
                    }
                } else {
                    // Remove the thread from the bandwidth demand cache, if present, and
                    // subtract any cached utilization that is still reflected in the
                    // total demand. This value may be different than the thread's current
                    // bandwidth demand due to bandwidth inheritance or a profile change since
                    // last blocking.
                    // TODO(eieio): Optimize based on placement type?
                    let utilization_to_remove = if self.power_level_control_is_enabled() {
                        self.bandwidth_reservation_cache_remove(Thread::tid_ptr(thread))
                    } else {
                        SchedUtilization::from_raw(0)
                    };

                    let deadline_util = (*state_ptr).effective_profile.deadline().utilization;
                    if (*state_ptr).effective_profile.is_deadline()
                        && (*state_ptr).effective_profile.critical
                    {
                        let mut critical_deadline_utilization =
                            self.critical_deadline_utilization();
                        critical_deadline_utilization += deadline_util;
                        self.set_critical_deadline_utilization(critical_deadline_utilization);
                        if critical_deadline_utilization > K_CPU_UTILIZATION_LIMIT {
                            static LAST_LOG_TIME_NS: core::sync::atomic::AtomicI64 =
                                core::sync::atomic::AtomicI64::new(0);
                            let last_ns =
                                LAST_LOG_TIME_NS.load(core::sync::atomic::Ordering::Relaxed);
                            if now.raw_value() - last_ns >= sched_s(1).raw_value()
                                && LAST_LOG_TIME_NS
                                    .compare_exchange(
                                        last_ns,
                                        now.raw_value(),
                                        core::sync::atomic::Ordering::Relaxed,
                                        core::sync::atomic::Ordering::Relaxed,
                                    )
                                    .is_ok()
                            {
                                warn_critical_deadline_utilization_exceeded(
                                    critical_deadline_utilization,
                                    self.this_cpu(),
                                );
                            }
                        }
                    }

                    self.update_total_deadline_utilization(deadline_util - utilization_to_remove);
                }

                // Increase the processing rate when the required utilization increases
                // beyond the current rate, accounting for current limits.
                self.power_level_control_update_rate();

                let runnable_task_count = self.runnable_task_count() + 1;
                debug_assert!(runnable_task_count > 0);
                self.set_runnable_task_count(runnable_task_count);
                self.trace_total_runnable_threads();

                // Start a new activation, if necessary.
                self.update_activation(thread, now);

                if placement != Placement::Association {
                    self.queue_thread(thread, placement, now);
                } else {
                    // Connect the flow into the previous queue to the new queue.
                    local_ktrace_flow_step!(FLOW, "sched_latency", (*state_ptr).flow_id);
                }
            }
        }
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

        // SAFETY: `queue_lock_` is held by the caller with interrupts disabled.
        // `cpp_scheduler_power_state_post_update_request` latches the power domain
        // under `queue_lock_`, releases the lock around the post (which may acquire
        // other CPUs' `queue_lock_`s), and re-acquires it before returning.
        let cpus_to_reschedule_mask = unsafe {
            cpp_scheduler_power_state_post_update_request(
                self as *const Scheduler as *mut Scheduler,
                &request,
            )
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
    /// Sends a power level request through this scheduler's power level controller for testing.
    /// This method is called from thread and IRQ context to ensure that the underlying machinery
    /// can be safely called within `block`, `unblock`, and other scheduling operations.
    ///
    /// Returns true if the request was sent, false otherwise.
    ///
    /// # Safety
    /// `self` must be a valid initialized `Scheduler` whose `queue_lock_` is not
    /// already held by the current CPU (`TA_EXCL(queue_lock_)`).
    pub unsafe fn request_power_level_for_testing(&self, power_level: u8) -> bool {
        let _irqd = crate::arch_rs::InterruptDisableGuard::new();
        let sched_mut = self as *const Scheduler as *mut Scheduler;
        // SAFETY: `sched_mut` is valid, interrupts are disabled by `_irqd`, and
        // `queue_lock_` is not already held.
        let need_reschedule = unsafe {
            let _guard = QueueLockGuard::acquire(sched_mut);
            cpp_scheduler_power_state_is_valid_active_power_level(self, power_level)
                && self.pend_power_level_request(power_level)
        };
        if need_reschedule {
            // SAFETY: Interrupts are disabled and `self.this_cpu()` is initialized.
            unsafe { Self::reschedule_mask(cpu_num_to_mask(self.this_cpu())) };
        }
        need_reschedule
    }

    /// Updates the performance scales of the requested CPUs and returns the
    /// effective values in place, which may be different than the requested values
    /// if they are below the minimum safe values for the respective CPUs.
    ///
    /// Requires `info.len() <= num CPUs`.
    ///
    /// # Safety
    /// `info.len()` and each `entry.logical_cpu_number` must be valid CPU numbers
    /// (`< PerCpu::processor_count()`), and no `queue_lock_` may be held by the
    /// current CPU (`TA_EXCL(queue_lock_)`).
    pub unsafe fn update_processing_rates(info: &mut [zx_types::zx_cpu_performance_info_t]) {
        debug_assert!(info.len() <= PerCpu::processor_count());
        let _irqd = crate::arch_rs::InterruptDisableGuard::new();

        let mut cpus_to_reschedule_mask: CpuMask = 0;
        for entry in info.iter_mut() {
            debug_assert!((entry.logical_cpu_number as usize) < PerCpu::processor_count());

            cpus_to_reschedule_mask |= cpu_num_to_mask(entry.logical_cpu_number);
            // SAFETY: `entry.logical_cpu_number` is a valid CPU number, interrupts are
            // disabled by `_irqd`, and `queue_lock_` is acquired before forming
            // `&Scheduler` on `sched_ptr`.
            unsafe {
                let sched_ptr = cpp_scheduler_get(entry.logical_cpu_number);
                let _guard = QueueLockGuard::acquire(sched_ptr);
                let scheduler = &*sched_ptr;

                // TODO(eieio): Apply a minimum value threshold.
                scheduler
                    .user_set_processing_rate(to_sched_processing_rate(entry.performance_scale));
                // Return the original performance scale (i.e. processing rate).
                entry.performance_scale = to_user_performance_scale(scheduler.processing_rate());
            }
        }

        // SAFETY: Interrupts are disabled and no `queue_lock_` is held.
        unsafe { Self::reschedule_mask(cpus_to_reschedule_mask) };
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

    /// Updates the performance limits of the requested CPUs.
    ///
    /// Requires `limits.len() <= num CPUs`.
    ///
    /// # Safety
    /// `limits.len()` and each `entry.logical_cpu_number` must be valid CPU
    /// numbers (`< PerCpu::processor_count()`), and no `queue_lock_` may be held by
    /// the current CPU (`TA_EXCL(queue_lock_)`).
    pub unsafe fn update_processing_limits(limits: &[zx_types::zx_cpu_perf_limit_t]) {
        debug_assert!(limits.len() <= PerCpu::processor_count());
        let _irqd = crate::arch_rs::InterruptDisableGuard::new();

        // Set the limits for each CPU in the given limits slice, keeping track of one
        // CPU per domain to evaluate the domain's active power level.
        let mut cpus_for_handled_domains: CpuMask = 0;
        let mut cpus_to_evaluate_updates: CpuMask = 0;
        for entry in limits.iter() {
            if entry.limit_type == zx_types::ZX_CPU_PERF_LIMIT_TYPE_RATE {
                debug_assert!((entry.logical_cpu_number as usize) < PerCpu::processor_count());
                // SAFETY: `entry.logical_cpu_number` is a valid CPU number, interrupts
                // are disabled by `_irqd`, and `_guard` acquires `queue_lock_` before
                // forming `&Scheduler` on `sched_ptr`.
                unsafe {
                    let sched_ptr = cpp_scheduler_get(entry.logical_cpu_number);
                    let _guard = QueueLockGuard::acquire(sched_ptr);
                    let scheduler = &*sched_ptr;

                    // Convert the user limits, expressed in units of USER_PROCESSING_RATE_SCALE,
                    // using the 64-bit `FastFixed` division path. The generic `Fixed` division
                    // evaluates in i128 and requires the 128-bit division builtins, which the
                    // kernel does not link. Values above the scale saturate and are clamped to
                    // 1.0 by `user_set_processing_rate_limits`.
                    let scale = FastFixed::<u64, 0>::from_raw(USER_PROCESSING_RATE_SCALE);
                    let min =
                        SchedProcessingRate::from(FastFixed::<u64, 0>::from_raw(entry.min) / scale);
                    let max =
                        SchedProcessingRate::from(FastFixed::<u64, 0>::from_raw(entry.max) / scale);
                    scheduler.user_set_processing_rate_limits(min, max);

                    // A rate change only needs to be evaluated for one active CPU in each domain.
                    let cpu_mask = cpu_num_to_mask(entry.logical_cpu_number);
                    let is_domain_handled = (cpus_for_handled_domains & cpu_mask) != 0;
                    if scheduler.is_scheduler_active_locked()
                        && scheduler.power_level_control_is_enabled()
                        && !is_domain_handled
                    {
                        cpus_to_evaluate_updates |= cpu_mask;
                        let domain_cpu_mask = cpp_scheduler_power_state_domain_cpus(scheduler);
                        cpus_for_handled_domains |= domain_cpu_mask;
                    }
                }
            }
        }

        // Reevaluate the power level for each updated domain.
        let mut cpu_mask = cpus_to_evaluate_updates;
        while cpu_mask != 0 {
            let cpu = remove_cpu_from_mask(&mut cpu_mask);
            // SAFETY: `cpu` is a valid CPU number, interrupts are disabled, and
            // `_guard` acquires `queue_lock_` before forming `&Scheduler`.
            unsafe {
                let sched_ptr = cpp_scheduler_get(cpu);
                let _guard = QueueLockGuard::acquire(sched_ptr);
                let scheduler = &*sched_ptr;
                scheduler.reevaluate_current_power_level();
            }
        }

        // SAFETY: Interrupts are disabled and no `queue_lock_` is held.
        unsafe { Self::reschedule_mask(cpus_to_evaluate_updates) };
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

    /// Evaluates the given `run_queue` for the thread with the earliest finish time
    /// that has a start time earlier than or equal to the given `eligible_time` and
    /// passes the given `predicate`, and asserts that any returned thread is
    /// associated with this scheduler.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`, which protects `run_queue` and the
    /// scheduler state of threads in it.
    unsafe fn find_earliest_eligible_thread<F>(
        &self,
        run_queue: &RunQueue,
        eligible_time: SchedTime,
        predicate: F,
    ) -> *mut Thread
    where
        F: Fn(*mut Thread) -> bool,
    {
        // SAFETY: `queue_lock_` is held by the caller.
        let out = unsafe { find_earliest_eligible_thread(run_queue, eligible_time, predicate) };
        if !out.is_null() {
            // SAFETY: `out` is in `run_queue` and `queue_lock_` is held.
            unsafe { self.assert_in_scheduler(out) };
        }
        out
    }

    /// Returns true if there is at least one eligible deadline thread in the
    /// run queue.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    unsafe fn is_deadline_thread_eligible(&self, eligible_time: SchedTime) -> bool {
        // SAFETY: `queue_lock_` is held by the caller, which guards the deadline run
        // queues and the `start_time` of threads enqueued in them.
        unsafe {
            let critical_queue = self.critical_deadline_queue();
            let deadline_queue = self.deadline_queue();

            for queue in [critical_queue, deadline_queue] {
                if let Some(front_ref) = (*queue).front() {
                    let front = core::ptr::from_ref(front_ref).cast_mut();
                    self.assert_in_run_queue(front);
                    let state_ptr = cpp_thread_get_scheduler_state(front);
                    if (*state_ptr).start_time <= eligible_time {
                        return true;
                    }
                }
            }
            false
        }
    }

    /// Returns true if there is at least one eligible fair thread in the run
    /// queue.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    unsafe fn is_fair_thread_eligible(&self, eligible_time: SchedTime) -> bool {
        // SAFETY: `queue_lock_` is held by the caller, which guards the fair run
        // queue, the affine transform, and the `start_time` of enqueued threads.
        unsafe {
            let fair_queue = self.fair_run_queue();
            let Some(front_ref) = (*fair_queue).front() else {
                return false;
            };

            let variable_eligible_time = self.monotonic_to_variable(eligible_time);
            let front = core::ptr::from_ref(front_ref).cast_mut();
            self.assert_in_run_queue(front);
            let state_ptr = cpp_thread_get_scheduler_state(front);
            (*state_ptr).start_time <= variable_eligible_time
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

    /// Reports a violated start/finish time invariant in `queue_thread`.
    ///
    /// Kept out of line and cold so that the formatting state does not occupy the
    /// stack frame of the hot queue path. The message is matched by tefmocheck.
    ///
    /// # Safety
    /// `state_ptr` must be valid and the caller must hold `queue_lock_`.
    #[cold]
    #[inline(never)]
    unsafe fn panic_start_time_not_before_finish_time(
        &self,
        now: SchedTime,
        state_ptr: *const SchedulerState,
    ) -> ! {
        // SAFETY: Forwarded from the caller.
        unsafe {
            // LINT.IfChange(scheduler_start_finish_time)
            panic!(
                "state.start_time < state.finish_time: now={} start={} finish={} \
                 remaining_time_slice={} fair_period={} mono_ref={} var_ref={} slope={}\n",
                now.raw_value(),
                (*state_ptr).start_time.raw_value(),
                (*state_ptr).finish_time.raw_value(),
                (*state_ptr).remaining_time_slice_ns().raw_value(),
                self.fair_period().raw_value(),
                (*self.state.get()).fair_affine_transform.monotonic_reference_time.raw_value(),
                (*self.state.get()).fair_affine_transform.variable_reference_time.raw_value(),
                (*self.state.get()).fair_affine_transform.slope,
            );
            // LINT.ThenChange(//tools/testing/tefmocheck/check-configs.yaml:scheduler_start_finish_time)
        }
    }

    /// Dequeues the eligible thread with the earliest deadline. The caller must
    /// ensure that there is at least one eligible thread in either the critical
    /// deadline run queue or the deadline run queue.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    unsafe fn dequeue_deadline_thread(&self, eligible_time: SchedTime) -> DequeueResult {
        let trace = local_ktrace_begin_scope!(DETAILED, "dequeue_deadline_thread");
        // SAFETY: `queue_lock_` is held by the caller, which guards the deadline run
        // queues, power level control state, and the SOVs (`start_time`, `finish_time`,
        // queue node) of threads enqueued in this scheduler (matching C++
        // `MarkHasOwnedThreadAccess`).
        unsafe {
            let critical_queue = self.critical_deadline_queue_mut();
            let deadline_queue = self.deadline_queue_mut();
            debug_assert!(!(*critical_queue).is_empty() || !(*deadline_queue).is_empty());

            let critical_thread =
                self.find_earliest_eligible_thread(&*critical_queue, eligible_time, accept_all);

            let is_oversubscribed =
                cpp_scheduler_get_normalized_utilization(self) > K_CPU_UTILIZATION_LIMIT;

            let eligible_thread = if is_oversubscribed && !critical_thread.is_null() {
                critical_thread
            } else {
                let normal_thread =
                    self.find_earliest_eligible_thread(&*deadline_queue, eligible_time, accept_all);
                // There must be at least one eligible deadline thread, whether critical or
                // normal.
                debug_assert!(!critical_thread.is_null() || !normal_thread.is_null());
                self.earliest_deadline_thread(critical_thread, normal_thread)
            };

            debug_assert!(
                !eligible_thread.is_null(),
                "eligible_time={}",
                eligible_time.raw_value()
            );

            // `eligible_thread`'s scheduler variables may be accessed here: `queue_lock_` is held
            // and the thread is in a run queue protected by the same lock.
            let is_critical = self.is_critical_deadline_thread(eligible_thread);
            let queue = if is_critical { critical_queue } else { deadline_queue };
            (*queue).erase_raw(eligible_thread);
            self.trace_thread_queue_event(
                ktrace_rs::resolve_string!("tqe_deque_deadline"),
                eligible_thread,
            );
            debug_assert_eq!(
                (*cpp_thread_get_queue_state(eligible_thread)).disposition(),
                QueueDisposition::Associated
            );

            let state_ptr = cpp_thread_get_scheduler_state(eligible_thread);
            let start_time = (*state_ptr).start_time;
            let finish_time = (*state_ptr).finish_time;
            if finish_time <= eligible_time {
                COUNTER_DEADLINE_PAST.add(1);
            }

            ktrace_rs::end_scope!(
                trace,
                "start time" => start_time,
                "finish time" => finish_time,
            );

            DequeueResult::from_thread(eligible_thread)
        }
    }

    /// Dequeues the eligible thread with the earliest virtual finish time. The
    /// caller must ensure that there is at least one thread in the queue.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    unsafe fn dequeue_fair_thread(&self, monotonic_eligible_time: SchedTime) -> DequeueResult {
        let trace = local_ktrace_begin_scope!(DETAILED, "dequeue_fair_thread");
        // SAFETY: `queue_lock_` is held by the caller, which guards the fair run
        // queue, the affine transform, and the SOVs (`start_time`, `finish_time`,
        // `subtree_invariants`, queue node) of threads enqueued in this scheduler
        // (matching C++ `MarkHasOwnedThreadAccess`).
        unsafe {
            let fair_queue = self.fair_run_queue_mut();

            // Convert monotonic eligible time to variable time to compare with fair run
            // queue values.
            let mut variable_eligible_time = self.monotonic_to_variable(monotonic_eligible_time);

            // `earliest_thread`'s scheduler variables may be accessed here: `queue_lock_`
            // is held and the thread is in a run queue protected by the same lock.
            let earliest_thread = (*fair_queue)
                .front()
                .map_or(core::ptr::null_mut(), |t| core::ptr::from_ref(t).cast_mut());
            debug_assert!(!earliest_thread.is_null());
            let earliest_state_ptr = cpp_thread_get_scheduler_state(earliest_thread);

            // If there are no eligible fair threads, snap the variable timeline to the
            // earliest start time.
            let earliest_start = (*earliest_state_ptr).start_time;
            if variable_eligible_time < earliest_start {
                COUNTER_FAIR_TIMELINE_SNAP_COUNT.add(1);
                let diff = earliest_start - variable_eligible_time;
                COUNTER_FAIR_TIMELINE_SNAP_TIME.add(diff.raw_value());

                (*self.state.get())
                    .fair_affine_transform
                    .snap(monotonic_eligible_time, earliest_start);
                variable_eligible_time = earliest_start;
            }

            let eligible_thread = self.find_earliest_eligible_thread(
                &*fair_queue,
                variable_eligible_time,
                accept_all,
            );
            debug_assert!(
                !eligible_thread.is_null(),
                "vearliest_start={}, veligible_time={} , vstart_time={}, vfinish_time={}, vmin_finish_time={}!",
                earliest_start.raw_value(),
                variable_eligible_time.raw_value(),
                (*earliest_state_ptr).start_time.raw_value(),
                (*earliest_state_ptr).finish_time.raw_value(),
                (*earliest_state_ptr).subtree_invariants.min_finish_time.raw_value(),
            );

            // Same argument as before for `MarkHasOwnedThreadAccess(*eligible_thread)`:
            // we are holding `queue_lock_`, and we just found this thread in the scheduler.
            (*fair_queue).erase_raw(eligible_thread);
            self.trace_thread_queue_event(
                ktrace_rs::resolve_string!("tqe_deque_fair"),
                eligible_thread,
            );
            debug_assert_eq!(
                (*cpp_thread_get_queue_state(eligible_thread)).disposition(),
                QueueDisposition::Associated
            );

            let state_ptr = cpp_thread_get_scheduler_state(eligible_thread);
            let var_start_time = (*state_ptr).start_time;
            let var_finish_time = (*state_ptr).finish_time;
            let mono_start_time_ns = self.variable_to_monotonic(var_start_time);
            let mono_finish_time_ns = self.variable_to_monotonic(var_finish_time);

            ktrace_rs::end_scope!(
                trace,
                "var_start_time" => var_start_time,
                "var_finish_time" => var_finish_time,
                "mono_start_time" => mono_start_time_ns,
                "mono_finish_time" => mono_finish_time_ns,
            );

            DequeueResult::with_pending_update(
                eligible_thread,
                mono_start_time_ns,
                mono_finish_time_ns,
            )
        }
    }

    /// Returns the eligible fair thread in the run queue with a finish time
    /// earlier than the given finish time, or null if one does not exist.
    ///
    /// See the note in `find_earliest_eligible_thread` for lifecycle rules for the
    /// returned pointer (if any).
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    unsafe fn find_earlier_fair_thread(
        &self,
        eligible_time: SchedTime,
        finish_time: SchedTime,
    ) -> *mut Thread {
        // SAFETY: `queue_lock_` is held by the caller, which guards the fair run
        // queue, the affine transform, and the `finish_time` of enqueued threads
        // (matching C++ `MarkHasOwnedThreadAccess`).
        unsafe {
            let fair_queue = self.fair_run_queue();
            // Convert monotonic to variable times to compare with fair run queue values.
            let variable_eligible_time = self.monotonic_to_variable(eligible_time);
            let variable_finish_time = self.monotonic_to_variable(finish_time);

            let eligible_thread = self.find_earliest_eligible_thread(
                &*fair_queue,
                variable_eligible_time,
                accept_all,
            );

            if !eligible_thread.is_null() {
                // `eligible_thread`'s scheduler variables may be accessed here: `queue_lock_` is held
                // and the thread is in a run queue protected by the same lock.
                let state_ptr = cpp_thread_get_scheduler_state(eligible_thread);
                if (*state_ptr).finish_time < variable_finish_time {
                    return eligible_thread;
                }
            }
            core::ptr::null_mut()
        }
    }

    /// Removes the eligible fair thread with a finish time earlier than the given
    /// finish time and returns it, or an empty result if one does not exist.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    unsafe fn dequeue_earlier_fair_thread(
        &self,
        eligible_time: SchedTime,
        finish_time: SchedTime,
    ) -> DequeueResult {
        let _trace = local_ktrace_begin_scope!(DETAILED, "dequeue_earlier_fair_thread");
        // SAFETY: `queue_lock_` is held by the caller, which guards the fair run
        // queue, the affine transform, and the SOVs (`start_time`, `finish_time`,
        // queue node) of threads enqueued in this scheduler (matching C++
        // `MarkHasOwnedThreadAccess`).
        unsafe {
            let eligible_thread = self.find_earlier_fair_thread(eligible_time, finish_time);
            if !eligible_thread.is_null() {
                // `eligible_thread`'s scheduler variables may be accessed here: `queue_lock_` is held
                // and the thread is in a run queue protected by the same lock.
                let fair_queue = self.fair_run_queue_mut();
                (*fair_queue).erase_raw(eligible_thread);
                self.trace_thread_queue_event(
                    ktrace_rs::resolve_string!("tqe_deque_earlier_fair"),
                    eligible_thread,
                );
                debug_assert_eq!(
                    (*cpp_thread_get_queue_state(eligible_thread)).disposition(),
                    QueueDisposition::Associated
                );

                let state_ptr = cpp_thread_get_scheduler_state(eligible_thread);
                let mono_start_time_ns = self.variable_to_monotonic((*state_ptr).start_time);
                let mono_finish_time_ns = self.variable_to_monotonic((*state_ptr).finish_time);

                DequeueResult::with_pending_update(
                    eligible_thread,
                    mono_start_time_ns,
                    mono_finish_time_ns,
                )
            } else {
                DequeueResult::empty()
            }
        }
    }

    /// Removes the eligible deadline thread with a finish time earlier than the
    /// given reference thread's finish time and criticality and returns it, or an
    /// empty result if one does not exist.
    ///
    /// # Safety
    /// `reference_thread` must point to a valid `Thread`, and the caller must hold
    /// `queue_lock_` and at least shared access to `reference_thread`'s lock.
    unsafe fn dequeue_earlier_deadline_thread(
        &self,
        eligible_time: SchedTime,
        reference_thread: *const Thread,
    ) -> DequeueResult {
        let _trace = local_ktrace_begin_scope!(DETAILED, "dequeue_earlier_deadline_thread");
        // SAFETY: `queue_lock_` and `reference_thread`'s lock are held by the caller,
        // guarding the deadline run queues and the SOVs of enqueued threads (matching
        // C++ `MarkHasOwnedThreadAccess`).
        unsafe {
            let eligible_thread =
                self.find_earlier_deadline_thread(eligible_time, reference_thread);
            if !eligible_thread.is_null() {
                // `eligible_thread`'s scheduler variables may be accessed here: `queue_lock_` is held
                // and the thread is in a run queue protected by the same lock.
                let is_critical = self.is_critical_deadline_thread(eligible_thread);
                let queue = if is_critical {
                    self.critical_deadline_queue_mut()
                } else {
                    self.deadline_queue_mut()
                };
                (*queue).erase_raw(eligible_thread);
                self.trace_thread_queue_event(
                    ktrace_rs::resolve_string!("tqe_deque_earlier_deadline"),
                    eligible_thread,
                );
                debug_assert_eq!(
                    (*cpp_thread_get_queue_state(eligible_thread)).disposition(),
                    QueueDisposition::Associated
                );
                DequeueResult::from_thread(eligible_thread)
            } else {
                DequeueResult::empty()
            }
        }
    }

    /// Adds a thread to the run queue tree. The thread must be active on this
    /// CPU.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread`, and the caller must hold both
    /// `queue_lock_` and `thread`'s lock exclusively.
    unsafe fn queue_thread(&self, thread: *mut Thread, placement: Placement, now: SchedTime) {
        let trace = local_ktrace_begin_scope!(
            DETAILED,
            "queue_thread",
            "placement" => placement_to_str(placement),
        );
        // SAFETY: `thread` is valid and both `queue_lock_` and `thread`'s lock are held
        // exclusively. We use `state_ptr` rather than holding `&mut SchedulerState` across
        // `insert_raw` (whose WAVLTree observer hooks borrow `thread`'s `SchedulerState`)
        // and `trace_thread_queue_event`.
        unsafe {
            let state_ptr = cpp_thread_get_scheduler_state(thread);

            debug_assert_eq!((*state_ptr).state, ThreadState::Ready);
            debug_assert!(!Thread::is_idle_ptr(thread));
            debug_assert!(placement != Placement::Association);

            // Check the consistency of the start and finish times before adding the
            // thread to the run queue.
            if cfg!(debug_assertions) && (*state_ptr).start_time >= (*state_ptr).finish_time {
                self.panic_start_time_not_before_finish_time(now, state_ptr);
            }

            // Only update the enqueue time and emit a flow event if this is an insertion,
            // preemption, or migration. In contrast, an adjustment only changes the queue
            // position in the same queue due to a parameter change and should not perform
            // these actions.
            if !matches!(placement, Placement::Adjustment) {
                if matches!(placement, Placement::Migration) {
                    // Connect the flow into the previous queue to the new queue.
                    local_ktrace_flow_step!(FLOW, "sched_latency", (*state_ptr).flow_id);
                } else {
                    // Reuse this member to track the time the thread enters the run queue. It
                    // is not read outside of the scheduler unless the thread state is
                    // THREAD_RUNNING.
                    (*state_ptr).last_started_running = now;
                    let flow_id = self.next_flow_id();
                    (*state_ptr).flow_id = flow_id;
                    local_ktrace_flow_begin!(FLOW, "sched_latency", flow_id);
                }
            }

            let is_fair = (*state_ptr).effective_profile.is_fair();
            let is_critical = (*state_ptr).effective_profile.critical;

            // Insert the thread into the appropriate run queue.
            if is_fair {
                // Convert start and finish times to variable time before inserting.
                // TODO(eieio): Convert from standard period to local period.
                self.monotonic_to_variable_in_place(&mut (*state_ptr).start_time);
                self.monotonic_to_variable_in_place(&mut (*state_ptr).finish_time);

                let fair_queue = self.fair_run_queue_mut();
                (*fair_queue).insert_raw(thread);
            } else {
                let queue = if is_critical {
                    self.critical_deadline_queue_mut()
                } else {
                    self.deadline_queue_mut()
                };
                (*queue).insert_raw(thread);
            }

            if !matches!(placement, Placement::Adjustment) {
                self.trace_thread_queue_event(ktrace_rs::resolve_string!("tqe_enque"), thread);
            }

            ktrace_rs::end_scope!(
                trace,
                "start time" => (*state_ptr).start_time,
                "finish time" => (*state_ptr).finish_time,
            );
        }
    }

    /// Removes a specific thread from its current `RunQueue`. Note that the thread
    /// must currently exist in its queue, it is an error otherwise.
    ///
    /// # Safety
    /// `thread` must point to a valid `Thread`, and the caller must hold both
    /// `queue_lock_` and `thread`'s lock exclusively.
    unsafe fn erase_from_queue(&self, thread: *mut Thread) {
        // SAFETY: `thread` is valid and both `queue_lock_` and `thread`'s lock are held
        // exclusively. We use `state_ptr` rather than holding `&mut SchedulerState` across
        // `erase_raw` (whose WAVLTree observer hooks borrow `thread`'s `SchedulerState`).
        unsafe {
            debug_assert!((*cpp_thread_get_queue_state(thread)).in_queue());
            let state_ptr = cpp_thread_get_scheduler_state(thread);
            let is_fair = (*state_ptr).effective_profile.is_fair();
            let is_critical = (*state_ptr).effective_profile.critical;

            let queue = if is_fair {
                self.fair_run_queue_mut()
            } else if is_critical {
                self.critical_deadline_queue_mut()
            } else {
                self.deadline_queue_mut()
            };

            (*queue).erase_raw(thread);

            if is_fair {
                self.variable_to_monotonic_in_place(&mut (*state_ptr).start_time);
                self.variable_to_monotonic_in_place(&mut (*state_ptr).finish_time);
            }
        }
    }

    /// Removes the thread at the head of the first eligible run queue. If there is
    /// an eligible deadline thread, it takes precedence over available fair
    /// threads. If there is no eligible work, returns `EvaluateResult::steal_work()`
    /// to indicate that the caller should attempt to steal work from other busy CPUs
    /// or fall back to this scheduler's idle thread.
    ///
    /// In all cases where a thread is returned, the thread will not be locked, and
    /// will not be a member of any current run queue. One of the following *must*
    /// happen to the thread after this:
    ///
    /// 1) The thread is added back to one of these scheduler's run queues (this
    ///    is common during a PI interaction involving the thread).
    /// 2) The thread is flagged as `QueueDisposition::Associated` in its
    ///    scheduler-owned variables, before becoming the active thread in its
    ///    scheduler (after dropping the scheduler lock and re-obtaining the
    ///    thread's lock exclusively). This happens in `reschedule_common_helper`
    ///    when `evaluate_next_thread` chooses a new thread to run.
    /// 3) The thread is flagged as `QueueDisposition::Stolen` after being removed
    ///    from another scheduler's run queue, but before being locked exclusively
    ///    and re-inserted into its new scheduler's run queue.
    ///
    /// Note that these rules apply to *all* the `dequeue_*` methods.
    ///
    /// # Safety
    /// The caller must hold `queue_lock_`.
    unsafe fn dequeue_thread(&self, now: SchedTime) -> EvaluateResult {
        // SAFETY: `queue_lock_` is held by the caller.
        unsafe {
            if self.is_deadline_thread_eligible(now) {
                let res = self.dequeue_deadline_thread(now);
                return EvaluateResult::selected(res);
            }

            let fair_queue = self.fair_run_queue();
            if !(*fair_queue).is_empty() {
                let res = self.dequeue_fair_thread(now);
                return EvaluateResult::selected(res);
            }

            EvaluateResult::steal_work()
        }
    }

    /// Evaluates the schedule and returns the thread that should execute.
    ///
    /// # Safety
    /// `current_thread` must point to a valid `Thread`, and the caller must hold
    /// both `queue_lock_` and `current_thread`'s lock exclusively.
    unsafe fn evaluate_next_thread(
        &self,
        now: SchedTime,
        current_thread: *mut Thread,
        current_is_migrating: bool,
    ) -> EvaluateResult {
        let _trace = local_ktrace_begin_scope!(DETAILED, "find_thread");

        // SAFETY: `current_thread` is valid and both `queue_lock_` and
        // `current_thread`'s lock are held exclusively by the caller. We use
        // `state_ptr` rather than holding `&SchedulerState` across
        // `update_activation` and `queue_thread`, both of which mutate
        // `current_thread`'s `SchedulerState`.
        unsafe {
            // First things first. If we have pending power work, we choose the idle-power thread.
            if cpp_scheduler_has_pending_power_work(self) {
                let idle_power_thread = cpp_scheduler_get_idle_thread(self);
                return EvaluateResult::selected(DequeueResult::from_thread(idle_power_thread));
            }

            let state_ptr = cpp_thread_get_scheduler_state(current_thread);
            let is_idle = Thread::is_idle_ptr(current_thread);
            let is_runnable = (*state_ptr).state == ThreadState::Ready;
            let is_deadline = (*state_ptr).effective_profile.is_deadline();

            if is_runnable && !is_idle && !current_is_migrating {
                // Start a new activation, if necessary, before comparing with other threads.
                self.update_activation(current_thread, now);

                // Determine eligibility after updating the activation period.
                let is_eligible = (*state_ptr).start_time <= now;

                if is_deadline && !is_eligible {
                    // Ineligible deadline threads must wait until they become eligible again.
                    return self.dequeue_thread(now);
                }

                if self.is_deadline_thread_eligible(now) {
                    if is_deadline {
                        // The current thread is deadline and eligible. Select the eligible
                        // thread with the earliest finish time, which may still be the current
                        // thread.
                        let earlier_thread_result =
                            self.dequeue_earlier_deadline_thread(now, current_thread);
                        if !earlier_thread_result.thread.is_null() {
                            return EvaluateResult::selected(earlier_thread_result);
                        }

                        // The current thread is eligible and has the earliest finish time.
                        return EvaluateResult::selected(DequeueResult::from_thread(
                            current_thread,
                        ));
                    }

                    // The current thread is fair. An eligible deadline thread should preempt
                    // the current thread.
                    let res = self.dequeue_deadline_thread(now);
                    return EvaluateResult::selected(res);
                }

                if !is_deadline {
                    if is_eligible {
                        if self.is_fair_thread_eligible(now) {
                            // Select the eligible fair thread with the earliest finish time,
                            // which may still be the current thread.
                            let finish_time = (*state_ptr).finish_time;
                            let earlier_thread_result =
                                self.dequeue_earlier_fair_thread(now, finish_time);
                            if !earlier_thread_result.thread.is_null() {
                                return EvaluateResult::selected(earlier_thread_result);
                            }
                        }

                        // The current fair thread has the earliest eligible finish time.
                        return EvaluateResult::selected(DequeueResult::from_thread(
                            current_thread,
                        ));
                    }

                    if self.is_fair_thread_eligible(now) {
                        // Select an eligible fair thread.
                        let res = self.dequeue_fair_thread(now);
                        return EvaluateResult::selected(res);
                    }

                    // No fair threads are currently eligible. Temporarily add the current
                    // thread to the run queue, before snapping the variable timeline to the
                    // earliest eligible time, to simplify selection in the updated timeline.
                    // Use Placement::Adjustment to avoid emitting inconsistent trace events.
                    self.queue_thread(current_thread, Placement::Adjustment, now);

                    // Snap the variable timeline to the earliest eligible time and select the
                    // thread with the earliest eligible finish time.
                    let next_thread = self.dequeue_fair_thread(now);
                    debug_assert!(!next_thread.thread.is_null());

                    // If the current thread is not selected to run again, it must be removed
                    // from the run queue to avoid being stolen during lock juggling. It will
                    // be returned to the run queue after the lock juggling is completed and
                    // before switching to the next thread.
                    if next_thread.thread != current_thread {
                        self.erase_from_queue(current_thread);
                    }

                    return EvaluateResult::selected(next_thread);
                }

                // The current deadline thread has remaining time and no eligible contender.
                debug_assert!(is_deadline);
                debug_assert!(is_eligible);
                return EvaluateResult::selected(DequeueResult::from_thread(current_thread));
            }

            // The thread is either no longer runnable, migrating, or the idle thread.
            self.dequeue_thread(now)
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
            self.find_earliest_eligible_thread(&*critical_deadline_queue, eligible_time, accept_all)
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
                self.find_earliest_eligible_thread(&*deadline_queue, eligible_time, accept_all)
            };
            // SAFETY: Both threads are either null or in this scheduler's run queues,
            // which `queue_lock_` guards.
            unsafe { self.earliest_deadline_thread(critical_thread, normal_thread) }
        };

        let mut selected_thread = core::ptr::null_mut();
        if !eligible_thread.is_null() {
            // SAFETY: Shared access to the eligible thread's scheduler state is sound
            // because `queue_lock_` is held and the eligible thread is in a run queue
            // protected by the same lock, so the fields read below cannot be
            // concurrently written (see the locking note on `SchedulerState`).
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
                "contender_tid" => unsafe { Thread::tid_ptr(eligible_thread) },
                "contender_finish_delta_ns" => eligible_state.finish_time - eligible_time,
                "contender_start_delta_ns" => eligible_time - eligible_state.start_time,
                "contender_remaining_slice_ns" => eligible_state.remaining_time_slice_ns(),
                "contender_capacity_ns" => eligible_state.time_slice_ns,
                "active_name" => unsafe { thread_name(reference_thread) }.as_str(),
                "active_tid" => unsafe { Thread::tid_ptr(reference_thread) },
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

    /// Returns the segment type inferred from whether the thread is an idle
    /// thread or not.
    #[inline]
    fn to_cpu_stats_segment_type(is_idle: bool) -> CpuStatsSegmentType {
        if is_idle { CpuStatsSegmentType::Idle } else { CpuStatsSegmentType::Busy }
    }

    /// Returns true if the thread owning `state` needs to be migrated from this
    /// CPU to another.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// - `state` is the `SchedulerState` of a valid, initialized `Thread`.
    /// - An active `ChainLockTransaction` holds that thread's lock
    ///   (`TA_REQ(chainlock_transaction_token, thread->get_lock())`).
    /// - `self.this_cpu()` has been initialized.
    #[inline]
    unsafe fn needs_migration_local(&self, state: *const SchedulerState) -> bool {
        // SAFETY: `state` belongs to a thread locked by the caller.
        let state = unsafe { &*state };

        // Threads that are not ready should not be migrated. Check this first to
        // avoid touching the shared active mask on the common blocking path.
        if state.state != ThreadState::Ready {
            return false;
        }

        // If we have no active CPUs, then we cannot migrate this thread, so return false.
        let active_mask = peek_active_mask();
        if active_mask == 0 {
            return false;
        }

        // We need to migrate if the set of CPUs this thread can run on does not
        // include the current CPU.
        // SAFETY: `this_cpu` is initialized per the caller's contract.
        let current_cpu_mask = cpu_num_to_mask(unsafe { self.this_cpu() });
        (state.get_effective_cpu_mask(active_mask) & current_cpu_mask) == 0
    }

    /// Returns true if the thread owning `state` has a time slice that is
    /// adjustable under changes to the fair scheduler demand on the CPU.
    ///
    /// # Safety
    /// The caller must ensure `state` is the `SchedulerState` of a valid,
    /// initialized `Thread` whose lock is held at least shared
    /// (`TA_REQ_SHARED(thread->get_lock())`), and `is_idle` reflects that thread.
    #[inline]
    unsafe fn is_thread_adjustable(state: *const SchedulerState, is_idle: bool) -> bool {
        // Checking the thread state avoids unnecessary adjustments on a thread that
        // is no longer competing.
        // SAFETY: `state` belongs to a thread locked by the caller.
        let state = unsafe { &*state };
        !is_idle && state.effective_profile.is_fair() && state.state == ThreadState::Ready
    }

    /// Removes the thread from this CPU's scheduler and updates the current CPU
    /// id. The thread must not be in the run queue.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// - `thread` points to a valid, initialized `Thread` structure.
    /// - Both the scheduler's `queue_lock_` and `thread`'s lock are held exclusively.
    unsafe fn remove_thread_locked(&self, now: SchedTime, thread: *mut Thread) {
        // SAFETY: `thread` is valid and `queue_lock_` is held.
        unsafe { self.remove(now, thread, INVALID_CPU) };
        // SAFETY: `thread` is valid and its lock is held exclusively by the caller.
        unsafe {
            (*cpp_thread_get_scheduler_state(thread)).curr_cpu = INVALID_CPU;
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

    /// Common logic for the reschedule API.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// - `self` is the current CPU's `Scheduler` with interrupts disabled and
    ///   `queue_lock_` held (`TA_REQ(queue_lock_)`).
    /// - `current_thread` is the current CPU's active thread with its lock held
    ///   exclusively by the active `ChainLockTransaction` using the reserved
    ///   scheduler token (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`).
    unsafe fn reschedule_common_helper(
        &self,
        current_thread: *mut Thread,
        now: SchedTime,
        boot_ticks: zx_types::zx_instant_boot_ticks_t,
    ) -> RustRescheduleResult {
        // No spinlocks should be held as we come into a reschedule operation. Only
        // the current thread's lock and `queue_lock_` should be held.
        // We keep `current_state` as a raw pointer `*mut SchedulerState` so no `&mut`
        // reference spans helper calls that also access `current_thread`'s state.
        // SAFETY: `current_thread` is valid and locked by the caller, and `self`'s
        // `queue_lock_` is held.
        let (current_state, current_queue_state, curr_is_idle, current_cpu) = unsafe {
            (
                cpp_thread_get_scheduler_state(current_thread),
                cpp_thread_get_queue_state(current_thread),
                Thread::is_idle_ptr(current_thread),
                self.this_cpu(),
            )
        };

        // When calling into reschedule, the current thread is only allowed to be in a
        // limited number of states, depending on where it came from.
        //
        // RUNNING   : Thread is the currently active thread on this CPU.
        // BLOCKED*  : Thread has been inserted into a wait queue and is in the
        //             process of de-activating.
        // SUSPENDED : Thread is in the process of suspending (but is not in any queue).
        // DEAD      : Thread is in the process of dying, this is its final reschedule.
        // SAFETY: `current_thread`'s lock and `self`'s `queue_lock_` are held.
        unsafe {
            self.assert_in_scheduler(current_thread);
            debug_assert_ne!((*current_state).state, ThreadState::Ready);
            if (*current_state).state == ThreadState::Running {
                debug_assert_eq!(
                    (*current_queue_state).disposition(),
                    QueueDisposition::Associated
                );
                (*current_state).state = ThreadState::Ready;
            }
        }

        // TODO(https://fxbug.dev/381899402): Find the root cause of small negative
        // values in the actual runtime calculation.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        let (actual_runtime_ns, total_runtime_ns) = unsafe {
            let actual = core::cmp::max(
                now - (*current_state).last_started_running,
                SchedDuration::from_raw(0),
            );
            (*current_state).last_started_running = now;
            let total = now - self.start_of_current_time_slice_ns();
            (*current_state).runtime_ns += actual;
            (actual, total)
        };

        // CRITICAL: Maintain Coherency of Idle and Busy States
        //
        // Apply the following rule:
        //
        //   1. Initial Update: Always performed under the first lock acquisition to
        //      account for time elapsed since the last reschedule.
        //
        // See update_cpu_stats_segment for the full set of coherency rules.
        //
        // It is not yet known whether the CPU is transitioning to/from idle or the
        // processing rate is changing. Simply advance the CPU stats to reflect the
        // elapsed idle time or normalized busy time at the last known processing rate
        // since the last update.
        // SAFETY: `queue_lock_` is held.
        unsafe {
            self.update_cpu_stats_segment(
                now,
                Self::to_cpu_stats_segment_type(curr_is_idle),
                self.processing_rate(),
            )
        };

        // Update the energy consumption accumulators for the current task and
        // processor.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        unsafe {
            self.update_estimated_energy_consumption(
                current_thread,
                now,
                boot_ticks,
                actual_runtime_ns,
            )
        };

        // Update the used time slice before evaluating the next task. Scale the
        // actual runtime of a deadline task by the relative performance of the CPU,
        // effectively increasing the capacity of the task in proportion to the
        // performance ratio. The remaining time slice may become negative due to
        // scheduler overhead.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        let is_deadline = unsafe { (*current_state).effective_profile.is_deadline() };
        let scaled_runtime = if is_deadline {
            // SAFETY: `queue_lock_` is held.
            unsafe { self.scale_down(actual_runtime_ns) }
        } else {
            actual_runtime_ns
        };
        // SAFETY: `current_thread`'s lock is held exclusively.
        unsafe {
            (*current_state).time_slice_used_ns += scaled_runtime;
        }

        // Adjust the rate of the current thread when fair bandwidth demand changes.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        if unsafe {
            Self::is_thread_adjustable(current_state, curr_is_idle)
                && self.weight_total() != self.scheduled_weight_total()
                && (*current_state).remaining_time_slice_ns().raw_value() > 0
                && (*current_state).finish_time.raw_value() > now.raw_value()
        } {
            let _trace_adjust_rate = local_ktrace_begin_scope!(
                DETAILED,
                "adjust_rate",
                "total runtime" => total_runtime_ns
            );
            // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
            unsafe {
                self.set_scheduled_weight_total(self.weight_total());
                self.adjust_fair_bandwidth(current_thread, now);
                let target_preemption = (*current_state).remaining_time_slice_ns() + now;
                let target_preemption = core::cmp::max(target_preemption, now);
                let target_preemption =
                    core::cmp::min(target_preemption, (*current_state).finish_time);
                self.set_target_preemption_time_ns(target_preemption);
            }
        }

        // Use a small epsilon to avoid tripping the consistency check due to
        // rounding when scaling the time slice.
        let time_slice_epsilon = sched_ns(100);
        // SAFETY: `current_thread`'s lock is held exclusively.
        let timeslice_expired = unsafe {
            now.raw_value() >= (*current_state).finish_time.raw_value()
                || (*current_state).remaining_time_slice_ns().raw_value()
                    <= time_slice_epsilon.raw_value()
        };

        // Check the consistency of the target preemption time and the current time
        // slice.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        unsafe {
            if cfg!(debug_assertions)
                && !(now < self.target_preemption_time_ns() || timeslice_expired)
            {
                let ep = &(*current_state).effective_profile;
                panic_preemption_time_inconsistent(&PreemptionTimeDiagnostics {
                    cpu: current_cpu,
                    capacity_ns: if is_deadline { ep.deadline().capacity_ns } else { sched_ns(0) },
                    deadline_ns: if is_deadline { ep.deadline().deadline_ns } else { sched_ns(0) },
                    finish_time: (*current_state).finish_time,
                    now,
                    target_preemption_time_ns: self.target_preemption_time_ns(),
                    start_of_current_time_slice_ns: self.start_of_current_time_slice_ns(),
                    total_runtime_ns,
                    actual_runtime_ns,
                    time_slice_ns: (*current_state).time_slice_ns,
                    time_slice_used_ns: (*current_state).time_slice_used_ns,
                    remaining_time_slice_ns: (*current_state).remaining_time_slice_ns(),
                });
            }
        }

        // If the current thread needs to be migrated, remove it for migration.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        let current_is_migrating = unsafe { self.needs_migration_local(current_state) };
        if current_is_migrating {
            // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
            unsafe { self.remove(now, current_thread, INVALID_CPU) };
        }

        // Select the next thread to run.
        //
        // WARNING: This method temporarily drops the queue lock during work stealing,
        // when there are no threads in the run queue and the current thread is no
        // longer in the running state. Do not split updates to different values
        // across this call that need to be coherently read from another CPU and
        // synchronized using the queue lock. Doing so creates an incomplete critical
        // section that can result in incoherent reads of the values updated in
        // different acquisitions of the queue lock.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        let mut next_thread_result =
            unsafe { self.evaluate_next_thread(now, current_thread, current_is_migrating) };

        // Update the outgoing thread's runtime stats only when switching away from
        // it (before dropping `queue_lock_` for work stealing or lock juggling).
        // When `current_thread` continues running (either continuing its current
        // slice or starting a new slice without leaving the CPU), it never enters
        // the run queue and its runtime stats remain in `THREAD_RUNNING`.
        if !curr_is_idle && next_thread_result.thread != current_thread {
            // SAFETY: `current_thread` is valid and both `current_thread`'s lock and
            // `queue_lock_` are held.
            unsafe {
                cpp_thread_update_runtime_stats(current_thread, (*current_state).state as i32);
            }
        }

        if next_thread_result.status == EvaluateStatus::StealWork {
            // Release the queue lock while attempting to steal work, leaving IRQs
            // disabled. Latch our scale up factor to use while determining whether or
            // not we can steal a given thread before we drop our lock.
            // SAFETY: `queue_lock_` is held on entry; we latch `processing_rate` before
            // unlocking `queue_lock_`, assert the `ChainLockTransaction` is active while
            // stealing work, and re-acquire `queue_lock_` afterward.
            let stolen = unsafe {
                let rate = self.processing_rate();
                self.unlock_queue();
                let _ = ChainLockTransaction::assert_active();
                let stolen = self.steal_work(now, rate);
                self.lock_queue();
                stolen
            };

            if !stolen.thread.is_null() {
                next_thread_result = EvaluateResult::selected(stolen);
            } else {
                // SAFETY: `self` is a valid initialized `Scheduler`.
                next_thread_result = EvaluateResult::selected(DequeueResult {
                    thread: unsafe { cpp_scheduler_get_idle_thread(self) },
                    mono_start_time_ns: SchedTime::from_raw(0),
                    mono_finish_time_ns: SchedTime::from_raw(0),
                });
            }
        }

        debug_assert!(!next_thread_result.thread.is_null());
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        debug_assert!(!unsafe { (*current_queue_state).in_queue() });

        // Flush pending preemptions.
        // SAFETY: `current_thread` is valid and running on the current CPU with IRQs disabled.
        let ps = unsafe { &*Thread::preemption_state_ptr(current_thread) };
        let preempts_pending = ps.preempts_pending();
        super::mp::reschedule(preempts_pending, 0);
        ps.preempts_pending_clear();

        // If the `next_thread` is not the same as the current thread, we will need to
        // (briefly) drop the scheduler's queue lock in order to acquire the next
        // thread's lock exclusively. If we have selected the same thread as before,
        // we can skip this step.
        //
        // If we do need to acquire `next_thread`'s lock, we use the same chain lock
        // token that was used to acquire `current_thread`'s lock. We know this is safe
        // because:
        //
        // TODO(johngro): THIS IS NOT SAFE - FIX THIS COMMENT
        //
        // This turned out to not be safe after all. The deadlock potential below is
        // why we need to switch to the special "scheduler" token at the start of the
        // operation.
        //
        // While there cannot be any cycles in this individual operation (trivial to
        // show since there are two different locks here), deadlock is still possible.
        //
        // The flow is:
        // 1) T0 is running on CPU X, and is locked entering a reschedule operation.
        // 2) T1 is selected as the next thread to run, and is being locked
        //    exclusively (after dropping the scheduler's lock). At the same time:
        // 3) T2 is running on CPU Y, and is blocking on an OWQ which currently has T0
        //    as the owner. T1 has been selected as the new owner.
        // 4) T2 and the queue have been locked by the BAAO operation.
        // 5) T1 is the start of the new owner chain and is locked.
        // 6) The next step in the BAAO locking is to lock the old owner chain,
        //    starting from T0.
        //
        // So:
        // X has T0 and wants T1.
        // Y has T1 and wants T0.
        //
        // CPU Y is in a sequence which obeys the backoff protocol, so as long as its
        // token value > X's token value, Y will back off and we will be OK. If not,
        // however, we deadlock. The reschedule operation cannot backoff and drop
        // T0's lock, so it is stuck trying to get T1 from Y.
        //
        // TODO(johngro): END TODO
        //
        // 1) `current_thread` is currently running. Because of this, we know that it
        //    is the target node of its PI graph.
        // 2) `next_thread` is currently READY, which means it is also the target node
        //    of its PI graph.
        // 3) PI graphs always have exactly one target node.
        // 4) Therefore, current and next cannot be members of the same graph, and can
        //    both be acquired unconditionally.
        //
        // WARNING: This block temporarily drops the queue lock to acquire the next
        // thread's lock. Do not split updates to different values across this call
        // that need to be coherently read from another CPU and synchronized using the
        // queue lock. Doing so creates an incomplete critical section that can result
        // in incoherent reads of the values updated in different acquisitions of the
        // queue lock.
        let next_thread = next_thread_result.thread;
        if current_thread != next_thread {
            // The ChainLockTransaction we were in when we entered `reschedule_common` has
            // already been finalized. We need to restart it in order to obtain new
            // locks.
            // SAFETY: The caller entered `reschedule_common` with an active
            // `ChainLockTransaction` holding `current_thread`'s lock.
            let token = unsafe { ChainLockTransaction::assert_active() };
            ChainLockTransaction::active_ref(&token)
                .restart(clt_tag!("Scheduler::RescheduleCommon (restart)"), &token);

            // Release the queue lock, acquire the next thread lock, and re-acquire the
            // queue lock. While the queue lock is unlocked, a PI operation or affinity
            // change can update the next thread's state, since these operations could
            // already hold the next thread's lock while waiting for the queue lock.
            // These operations are obligated to reschedule this CPU if they invalidate
            // the next thread's state, such that a different thread should run or the
            // current CPU is no longer in the affinity mask. However, since the next
            // thread is no longer in the run queue, it cannot be discovered and stolen
            // by another CPU while the queue lock is unlocked.
            // SAFETY: `queue_lock_` is held on entry and `next_thread` is a valid
            // runnable or idle thread selected by `evaluate_next_thread` / `steal_work`.
            unsafe {
                self.unlock_queue();
                let _ = ChainLockTransaction::assert_active();
                // Acquire the lock without backing off using an explicit retry loop
                // because the current thread's lock is held and ChainLock::Acquire
                // asserts that no chain locks are currently held to avoid misuse in
                // the general case.
                let next_lock = thread_get_lock(next_thread);
                while !(*next_lock).try_acquire_with_options(
                    AllowFinalized::No,
                    RecordBackoff::No,
                    &token,
                ) {
                    Self::arch_yield();
                }
                self.lock_queue();
            }
            // All of the locks needed to complete the reschedule are held.
            ChainLockTransaction::finalize(&token);
        }

        // Update the state of the current and next thread.
        // SAFETY: `next_thread` is valid and its lock is held (either because
        // `next_thread == current_thread` or because we acquired it above), and
        // `queue_lock_` is held.
        unsafe {
            let token = ChainLockTransaction::assert_active();
            let next_lock = thread_get_lock(next_thread);
            (*next_lock).assert_acquired(&token);
            self.assert_in_scheduler(next_thread);
        }

        // Keep `next_state` as a raw pointer `*mut SchedulerState` to avoid creating
        // aliasing `&mut SchedulerState` references when `current_thread == next_thread`
        // or across helper calls. (The accessor is a constant offset from `next_thread`,
        // so it is computed unconditionally and folds into addressing modes.)
        // SAFETY: `next_thread` is valid.
        let next_state = unsafe { cpp_thread_get_scheduler_state(next_thread) };
        // Reuse the idle flag latched for `current_thread` when the same thread was
        // selected; otherwise read `next_thread`'s flags once.
        let next_is_idle = if next_thread == current_thread {
            curr_is_idle
        } else {
            // SAFETY: `next_thread` is valid.
            unsafe { Thread::is_idle_ptr(next_thread) }
        };

        // Convert from variable time to monotonic time if necessary.
        // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
        unsafe { next_thread_result.update_thread_timeline() };

        // If the next thread was just stolen from another CPU, finish associating it
        // with this CPU and scheduler.
        // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
        unsafe {
            let next_queue_state = cpp_thread_get_queue_state(next_thread);
            if (*next_queue_state).disposition() == QueueDisposition::Stolen {
                debug_assert_eq!((*next_state).state, ThreadState::Ready);
                debug_assert_ne!((*next_state).curr_cpu, current_cpu);
                debug_assert_eq!((*next_queue_state).stolen_by, current_cpu);
                debug_assert!(!next_thread_result.is_updated_pending());
                self.insert(now, next_thread, Placement::Association);
            }
            debug_assert_eq!((*next_queue_state).disposition(), QueueDisposition::Associated);
        }

        let mut need_context_switch = false;
        if current_thread != next_thread {
            need_context_switch = true;
            // Update stall contributions of the current and next thread.
            // SAFETY: Both `current_thread` and `next_thread` are valid and locked.
            unsafe { cpp_stall_accumulator_apply_context_switch(current_thread, next_thread) };

            // If the current thread is not the idle thread and not migrating, return it
            // to the run queue or remove its bookkeeping, depending on whether it is
            // READY.
            if !curr_is_idle && !current_is_migrating {
                // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
                if unsafe { (*current_state).state } == ThreadState::Ready {
                    let placement = if timeslice_expired {
                        Placement::Insertion
                    } else {
                        Placement::Preemption
                    };
                    // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
                    unsafe { self.queue_thread(current_thread, placement, now) };
                } else {
                    // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
                    unsafe { self.remove_thread_locked(now, current_thread) };
                }
            }
        }

        // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
        unsafe {
            (*next_state).state = ThreadState::Running;
            (*next_state).last_cpu = current_cpu;
            debug_assert_eq!((*next_state).curr_cpu, current_cpu);
            self.set_active_thread(next_thread);

            // Keep track of the weight of the active thread, since it is not in the run
            // queue contributing to the min_weight subtree invariant.
            let active_thread_weight = if !next_is_idle && (*next_state).effective_profile.is_fair()
            {
                (*next_state).effective_profile.weight
            } else {
                SchedWeight::max()
            };
            self.set_active_thread_weight(active_thread_weight);

            // Handle any pending migration work.
            cpp_thread_call_migrate_fn_locked(next_thread, MIGRATE_STAGE_RESTORE);
        }

        // Update the expected runtime of the current thread and the per-CPU total.
        // Only update the thread and aggregate values if the current thread is still
        // associated with this CPU or is no longer ready.
        // SAFETY: `current_thread`'s lock and `queue_lock_` are held.
        unsafe {
            let current_is_associated =
                !(*current_queue_state).active || (*current_state).curr_cpu == current_cpu;

            if !curr_is_idle
                && current_is_associated
                && (timeslice_expired || current_thread != next_thread)
            {
                let _trace_update_ema =
                    local_ktrace_begin_scope!(DETAILED, "update_expected_runtime");
                // Adjust the runtime for the relative performance of the CPU to account for
                // different performance levels in the estimate. The relative performance
                // scale is in the range (0.0, 1.0], such that the adjusted runtime is
                // always less than or equal to the monotonic runtime.
                let adjusted_total_runtime_ns = self.scale_down(total_runtime_ns);
                (*current_state).banked_runtime_ns += adjusted_total_runtime_ns;

                if timeslice_expired || !(*current_queue_state).active {
                    let delta_ns = Self::peak_decay_delta(
                        (*current_state).expected_runtime_ns,
                        (*current_state).banked_runtime_ns,
                    );
                    (*current_state).expected_runtime_ns += delta_ns;
                    (*current_state).banked_runtime_ns = SchedDuration::from_raw(0);

                    // Adjust the aggregate value by the same amount. The adjustment is only
                    // necessary when the thread is still active on this CPU.
                    if (*current_queue_state).active {
                        self.update_total_expected_runtime(delta_ns);
                    }
                }
            }
        }

        RustRescheduleResult {
            need_context_switch,
            timeslice_expired,
            current_is_migrating,
            next_thread,
            curr_is_idle,
            next_is_idle,
        }
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

    fn set_subtree_value(node: &Self::Target, val: Self::Value) {
        // SAFETY: `node` is a valid `Thread` in the `RunQueue` mutated exclusively
        // under `queue_lock_`, and no other reference to its `SchedulerState` is live.
        let state =
            unsafe { &mut *cpp_thread_get_scheduler_state(core::ptr::from_ref(node).cast_mut()) };
        state.subtree_invariants = val;
    }

    fn reset_subtree_value(_node: &Self::Target) {
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
/// Provides accessors that abstract the distinction between a thread's
/// `SchedulerState` and an owned wait queue's `WaitQueueInheritedSchedulerState`
/// when working with generic methods that operate on an upstream type and a
/// target type, where each type might be either a `Thread` or an
/// `OwnedWaitQueue`.
trait PiNodeState {
    /// The node's current effective profile (base profile combined with any
    /// inherited profile), as of the last `recompute_effective_profile`.
    fn effective_profile(&self) -> EffectiveProfile;
    /// Start of the node's current activation period (eligible time). For
    /// deadline nodes this is `finish_time - relative deadline`.
    fn start_time(&self) -> SchedTime;
    fn set_start_time(&mut self, val: SchedTime);
    /// End of the node's current activation period (absolute deadline for
    /// deadline nodes). Must be strictly greater than `start_time`.
    fn finish_time(&self) -> SchedTime;
    fn set_finish_time(&mut self, val: SchedTime);
    /// Total time slice (capacity) granted for the current activation period.
    fn time_slice_ns(&self) -> SchedDuration;
    fn set_time_slice_ns(&mut self, val: SchedDuration);
    /// Portion of `time_slice_ns` already consumed in the current period. The
    /// PI code adjusts this, rather than `time_slice_ns`, when it transfers
    /// remaining bandwidth between nodes so that `time_slice_ns` continues to
    /// reflect the capacity of the effective profile.
    fn time_slice_used_ns(&self) -> SchedDuration;
    fn set_time_slice_used_ns(&mut self, val: SchedDuration);

    /// Time slice remaining in the current period. Never negative after a PI
    /// update; see the clamping in `join_node_to_pi_graph_impl`.
    fn remaining_time_slice_ns(&self) -> SchedDuration {
        self.time_slice_ns() - self.time_slice_used_ns()
    }
}

/// Panics with the diagnostic state logged by the C++ `DEBUG_ASSERT_MSG`s in
/// the PI paths. Kept out of line and cold so that formatting state does not
/// occupy the stack frames of the PI hot paths.
#[cold]
#[inline(never)]
fn panic_pi_start_not_before_finish(
    label: &str,
    start_time: SchedTime,
    finish_time: SchedTime,
    deadline_ns: SchedDuration,
) -> ! {
    panic!(
        "{label}: start_time={} finish_time={} deadline_ns={}",
        start_time.raw_value(),
        finish_time.raw_value(),
        deadline_ns.raw_value()
    );
}

/// Asserts `node.start_time() < node.finish_time()` in debug builds, reporting
/// the node's times and `deadline_ns` on failure.
#[inline]
fn debug_assert_pi_start_before_finish<N: PiNodeState>(
    label: &str,
    node: &N,
    deadline_ns: SchedDuration,
) {
    if cfg!(debug_assertions) && !(node.start_time() < node.finish_time()) {
        panic_pi_start_not_before_finish(label, node.start_time(), node.finish_time(), deadline_ns);
    }
}

#[cold]
#[inline(never)]
fn panic_pi_negative_finish_time(finish_time: SchedTime) -> ! {
    panic!("finish_time {}", finish_time.raw_value());
}

#[cold]
#[inline(never)]
fn panic_pi_unexpected_thread_state(thread_state: ThreadState, tid: u64) -> ! {
    panic!("Unexpected target_ state {} for tid {tid}", thread_state as u32);
}

#[cold]
#[inline(never)]
fn panic_pi_unassociated_thread(
    name: &ThreadName,
    tid: u64,
    thread_state: ThreadState,
    curr_cpu: CpuNum,
) -> ! {
    panic!(
        "Found unassociated thread {name} with tid {tid} in state {}, curr_cpu is: {curr_cpu}",
        thread_state as u32
    );
}

#[cold]
#[inline(never)]
fn panic_pi_negative_remaining_time_slice(
    capacity_ns: SchedDuration,
    remaining_time_slice: SchedDuration,
    remaining_period: SchedDuration,
) -> ! {
    panic!(
        "capacity={} remaining_time_slice={} remaining_period={}",
        capacity_ns.raw_value(),
        remaining_time_slice.raw_value(),
        remaining_period.raw_value()
    );
}

#[cold]
#[inline(never)]
fn panic_pi_deadline_param_mismatch(
    what: &str,
    target_old: SchedDuration,
    upstream: SchedDuration,
) -> ! {
    panic!(
        "toep.deadline.{what}={} uep.deadline.{what}={}",
        target_old.raw_value(),
        upstream.raw_value()
    );
}

impl PiNodeState for SchedulerState {
    fn effective_profile(&self) -> EffectiveProfile {
        self.effective_profile
    }
    fn start_time(&self) -> SchedTime {
        self.start_time
    }
    fn set_start_time(&mut self, val: SchedTime) {
        self.start_time = val;
    }
    fn finish_time(&self) -> SchedTime {
        self.finish_time
    }
    fn set_finish_time(&mut self, val: SchedTime) {
        self.finish_time = val;
    }
    fn time_slice_ns(&self) -> SchedDuration {
        self.time_slice_ns
    }
    fn set_time_slice_ns(&mut self, val: SchedDuration) {
        self.time_slice_ns = val;
    }
    fn time_slice_used_ns(&self) -> SchedDuration {
        self.time_slice_used_ns
    }
    fn set_time_slice_used_ns(&mut self, val: SchedDuration) {
        self.time_slice_used_ns = val;
    }
}

// OwnedWaitQueues do not need to bother to track the dirty or clean state of
// their implied effective profile. They have no base profile (only inherited
// values) which gets turned into an effective profile by `get_effective_profile`
// during a PI interaction. We can get away with this because OWQs:
//
// 1) Cannot exist in any collections where their position is determined by
//    effective profile (otherwise we would need to remove and re-insert the
//    node in the collection during an update).
// 2) Cannot contribute to a scheduler's bookkeeping (because OWQs are not
//    things which get scheduled).
impl PiNodeState for WaitQueueInheritedSchedulerState {
    fn effective_profile(&self) -> EffectiveProfile {
        self.get_effective_profile()
    }
    fn start_time(&self) -> SchedTime {
        self.start_time
    }
    fn set_start_time(&mut self, val: SchedTime) {
        self.start_time = val;
    }
    fn finish_time(&self) -> SchedTime {
        self.finish_time
    }
    fn set_finish_time(&mut self, val: SchedTime) {
        self.finish_time = val;
    }
    fn time_slice_ns(&self) -> SchedDuration {
        self.time_slice_ns
    }
    fn set_time_slice_ns(&mut self, val: SchedDuration) {
        self.time_slice_ns = val;
    }
    fn time_slice_used_ns(&self) -> SchedDuration {
        self.time_slice_used_ns
    }
    fn set_time_slice_used_ns(&mut self, val: SchedDuration) {
        self.time_slice_used_ns = val;
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

// Threads contain internal storage which holds their "effective profile", the
// combination of their base profile and all of their inherited profile
// pressure, as well as a set of dirty/clean flags.
//
// Owned wait queues don't have quite the same arrangement. They themselves
// have no base profile, and their effective profile is really only their
// inherited deadline profile (if any), or the total of their inherited fair
// weight (if there is no inherited deadline). They do not explicitly maintain
// storage for their effective profile.
//
// When we get to this point in profile propagation, however, we need to be able
// to compute 3 things:
//
// 1) The effective profile of the target node before recomputing it because of
//    the change in profile pressure.
// 2) The effective profile of the target node after recomputing it because of
//    the change in profile pressure (note that this is the same for OWQs, but
//    not threads).
// 3) The effective profile of the upstream node which gave rise to the change
//    in target profile pressure.
//
// `owq_get_effective_profile` and `PiNodeState::effective_profile` provide a uniform
// way to compute an OWQ's effective profile, based on its IPVs, or to retrieve
// a thread's existing effective profile.
/// Computes an `OwnedWaitQueue`'s effective profile from its inherited
/// scheduler state.
///
/// # Safety
/// `owq` must point to a valid `OwnedWaitQueue` whose lock is held by the
/// caller (`TA_REQ(owq->get_lock())`) with non-null
/// `inherited_scheduler_state_storage()`.
unsafe fn owq_get_effective_profile(owq: *const OwnedWaitQueue) -> EffectiveProfile {
    // SAFETY: `owq` is valid and its lock is held by the caller.
    let iss_ptr = unsafe { cpp_owq_get_inherited_state_const(owq) };
    debug_assert!(!iss_ptr.is_null());
    // SAFETY: `iss_ptr` is non-null and synchronized by `owq`'s lock.
    let iss = unsafe { &*iss_ptr };
    iss.get_effective_profile()
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

/// Common priority inheritance handler for a target `Thread`.
///
/// # Safety
/// - `thread` must point to a valid, initialized `Thread` whose lock is held
///   exclusively by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, thread->get_lock())`).
/// - Preemption/interrupts must be disabled (`preempt_disabled_token`), and
///   no scheduler `queue_lock_` may be held (`TA_EXCL(queue_lock_)`).
/// - If `thread` is blocked in a wait queue, that wait queue's lock must also
///   be held in the active `ChainLockTransaction`.
unsafe fn pi_common_thread<F>(thread: *mut Thread, update_dynamic_params: F)
where
    F: FnOnce(&mut SchedulerState, &EffectiveProfile, &EffectiveProfile, SchedTime),
{
    let _trace = local_ktrace_begin_scope!(COMMON, "Pi::Common(Thread)");
    // SAFETY: `thread` is valid and its lock is held by the caller.
    let state_ptr = unsafe { cpp_thread_get_scheduler_state(thread) };
    // SAFETY: `thread`'s lock is held exclusively by the caller.
    let (curr_cpu, thread_state) = unsafe { ((*state_ptr).curr_cpu, (*state_ptr).state) };
    if curr_cpu != INVALID_CPU {
        if cfg!(debug_assertions)
            && !(thread_state == ThreadState::Running || thread_state == ThreadState::Ready)
        {
            // SAFETY: `thread` is valid per the caller's contract.
            panic_pi_unexpected_thread_state(thread_state, unsafe { Thread::tid_ptr(thread) });
        }
        // SAFETY: `curr_cpu` is a valid CPU number, interrupts are disabled, and
        // `queue_lock_` is not already held.
        let scheduler = unsafe { cpp_scheduler_get(curr_cpu) };
        let _guard = unsafe { QueueLockGuard::acquire(scheduler) };
        // SAFETY: `scheduler`'s `queue_lock_` and `thread`'s lock are held.
        unsafe {
            (*scheduler).validate_invariants();
            (*scheduler).assert_in_scheduler(thread);
        }

        // Sample the current time after acquiring the queue lock to avoid large
        // skews under contention.
        let mut now =
            SchedMonoTimeAndBootTicks { mono_time: SchedTime::from_raw(0), boot_ticks: 0 };
        // SAFETY: `now` is a valid mutable stack variable.
        unsafe { cpp_current_mono_time_and_boot_ticks(&mut now) };

        // Keep track of the original disposition. See SchedulerQueueState for
        // more details on how the disposition and thread state indicate which
        // operations are permitted on the thread and its associated scheduler
        // bookkeeping.
        // SAFETY: `thread` is valid and both `thread`'s lock and `queue_lock_` are held.
        let disposition = unsafe { cpp_thread_get_disposition(thread) };
        if cfg!(debug_assertions) && disposition == QueueDisposition::Unassociated as i32 {
            // SAFETY: `thread` is valid per the caller's contract.
            let (name, tid) = unsafe { (thread_name(thread), Thread::tid_ptr(thread)) };
            panic_pi_unassociated_thread(&name, tid, thread_state, curr_cpu);
        }

        if thread_state == ThreadState::Ready {
            if disposition == QueueDisposition::Enqueued as i32 {
                // SAFETY: `thread` is enqueued in `scheduler` and both locks are held.
                unsafe { (*scheduler).erase_from_queue(thread) };
            } else {
                COUNTER_UPDATE_IN_TRANSITION.add(1);
            }
        } else {
            let trace_update = local_ktrace_begin_scope!(
                COMMON,
                "Update Timeslice",
                // SAFETY: `scheduler` is valid and initialized.
                "target cpu" => unsafe { (*scheduler).this_cpu() },
            );
            debug_assert!(disposition == QueueDisposition::Associated as i32);

            // Update the time slice before updating other bookkeeping.
            // SAFETY: `thread`'s lock and `scheduler`'s `queue_lock_` are held.
            let actual_runtime_ns = core::cmp::max(
                now.mono_time - unsafe { (*state_ptr).last_started_running },
                SchedDuration::from_raw(0),
            );
            // SAFETY: `thread`'s lock and `scheduler`'s `queue_lock_` are held.
            let scaled_actual_runtime_ns = unsafe {
                if (*state_ptr).effective_profile.is_deadline() {
                    (*scheduler).scale_down(actual_runtime_ns)
                } else {
                    actual_runtime_ns
                }
            };

            // SAFETY: Both `thread`'s lock and `scheduler`'s `queue_lock_` are held.
            unsafe {
                (*state_ptr).runtime_ns += actual_runtime_ns;
                (*state_ptr).time_slice_used_ns += scaled_actual_runtime_ns;
                (*state_ptr).last_started_running = now.mono_time;
                (*(*scheduler).state.get()).start_of_current_time_slice_ns = now.mono_time;
                (*scheduler).update_estimated_energy_consumption(
                    thread,
                    now.mono_time,
                    now.boot_ticks,
                    actual_runtime_ns,
                );
            }
            ktrace_rs::end_scope!(
                trace_update,
                "mono_now" => now.mono_time,
                "boot_ticks" => now.boot_ticks,
                "actual_runtime_ns" => actual_runtime_ns,
            );
        }

        // Copy the original effective profile before updating it to compute the
        // changes to the scheduler bookkeeping.
        // SAFETY: `thread`'s lock and `scheduler`'s `queue_lock_` are held.
        let (old_ep, new_ep) = unsafe {
            let old = (*state_ptr).effective_profile;
            (*state_ptr).recompute_effective_profile();
            (old, (*state_ptr).effective_profile)
        };

        // Update the scheduler bookkeeping, if necessary.
        let mut bandwidth_demand_changed = false;
        if disposition == QueueDisposition::Associated as i32
            || disposition == QueueDisposition::Enqueued as i32
        {
            let mut weight_delta = SchedWeight::from_raw(0);
            let mut utilization_delta = SchedUtilization::from_raw(0);
            let mut critical_utilization_delta = SchedUtilization::from_raw(0);

            if old_ep.is_fair() {
                weight_delta -= old_ep.weight_or(SchedWeight::from_raw(0));
            } else {
                utilization_delta -= old_ep.deadline().utilization;
                if old_ep.critical {
                    critical_utilization_delta -= old_ep.deadline().utilization;
                }
            }
            if new_ep.is_fair() {
                weight_delta += new_ep.weight_or(SchedWeight::from_raw(0));
            } else {
                utilization_delta += new_ep.deadline().utilization;
                if new_ep.critical {
                    critical_utilization_delta += new_ep.deadline().utilization;
                }
            }

            if weight_delta.raw_value() != 0 {
                bandwidth_demand_changed = true;
                // SAFETY: `scheduler`'s `queue_lock_` is held.
                unsafe {
                    (*(*scheduler).state.get()).weight_total += weight_delta;
                    (*scheduler).update_fair_bandwidth_period(now.mono_time);
                }
            }
            if utilization_delta.raw_value() != 0 {
                bandwidth_demand_changed = true;
                // SAFETY: `scheduler`'s `queue_lock_` is held.
                unsafe {
                    (*scheduler).update_total_deadline_utilization(utilization_delta);
                }
            }
            // SAFETY: `scheduler`'s `queue_lock_` is held.
            unsafe {
                (*(*scheduler).state.get()).critical_deadline_utilization +=
                    critical_utilization_delta;
            }
        }

        // SAFETY: `scheduler`'s `queue_lock_` is held.
        debug_assert!(
            unsafe { (*(*scheduler).state.get()).weight_total } >= SchedWeight::from_raw(0)
        );
        // SAFETY: `scheduler`'s `queue_lock_` is held.
        debug_assert!(
            unsafe { (*(*scheduler).state.get()).critical_deadline_utilization }
                >= SchedUtilization::from_raw(0)
        );
        // SAFETY: `scheduler`'s `queue_lock_` is held.
        debug_assert!(
            unsafe { cpp_scheduler_get_normalized_utilization(scheduler) }
                >= SchedUtilization::from_raw(0)
        );

        // SAFETY: Both `thread`'s lock and `scheduler`'s `queue_lock_` are held, and
        // no other reference to `*state_ptr` is active during `update_dynamic_params`.
        update_dynamic_params(unsafe { &mut *state_ptr }, &old_ep, &new_ep, now.mono_time);

        if thread_state == ThreadState::Ready {
            if disposition == QueueDisposition::Enqueued as i32 {
                // SAFETY: `thread`'s lock and `scheduler`'s `queue_lock_` are held.
                unsafe { (*scheduler).queue_thread(thread, Placement::Adjustment, now.mono_time) };
            }
        } else {
            debug_assert!(thread_state == ThreadState::Running);
            // SAFETY: `thread`'s lock and `scheduler`'s `queue_lock_` are held.
            let (remaining, finish_time) =
                unsafe { ((*state_ptr).remaining_time_slice_ns(), (*state_ptr).finish_time) };
            if new_ep.is_fair() {
                // SAFETY: `scheduler`'s `queue_lock_` is held.
                unsafe {
                    let sched_state = (*scheduler).state.get();
                    (*sched_state).target_preemption_time_ns =
                        (*sched_state).start_of_current_time_slice_ns + remaining;
                }
            } else {
                // SAFETY: `scheduler`'s `queue_lock_` is held.
                let scaled_remaining_time_slice_ns = unsafe { (*scheduler).scale_up(remaining) };
                // SAFETY: `scheduler`'s `queue_lock_` is held.
                unsafe {
                    let sched_state = (*scheduler).state.get();
                    (*sched_state).target_preemption_time_ns = core::cmp::min(
                        (*sched_state).start_of_current_time_slice_ns
                            + scaled_remaining_time_slice_ns,
                        finish_time,
                    );
                }
            }

            // Emit a context switch event to and from the same thread to update the
            // visualized scheduling parameters if the bandwidth demand actually
            // changed.
            if bandwidth_demand_changed {
                // SAFETY: `thread` is valid and its lock is held exclusively.
                unsafe { sched_trace_context_switch(thread, thread, curr_cpu) };
            }
        }

        // Check that target is left in the same disposition.
        // SAFETY: `thread` is valid and both `thread`'s lock and `queue_lock_` are held.
        debug_assert!(disposition == unsafe { cpp_thread_get_disposition(thread) });

        // Reschedule to reflect the updated state.
        // SAFETY: `scheduler`'s `queue_lock_` and `thread`'s lock are held.
        unsafe {
            Scheduler::reschedule_mask(cpu_num_to_mask((*state_ptr).curr_cpu));
            (*scheduler).validate_invariants();
        };
    } else {
        // The target thread is not runnable; update its effective profile and the
        // scheduler bookkeeping.
        // SAFETY: `thread` is valid and its lock is held by the caller.
        let old_ep = unsafe { (*state_ptr).effective_profile };
        // SAFETY: `thread` is valid and its lock is held by the caller.
        let wq = unsafe { cpp_thread_get_blocking_wait_queue(thread) };
        if !wq.is_null() {
            // SAFETY: `wq` is non-null, an active `ChainLockTransaction` is held by
            // caller contract, and `wq`'s lock is held in that transaction.
            unsafe {
                let token = ChainLockTransaction::assert_active();
                (*cpp_wait_queue_get_lock(wq)).assert_acquired(&token);
                cpp_wait_queue_update_blocked_thread_effective_profile(wq, thread);
            }
        } else {
            // SAFETY: `thread`'s lock is held exclusively by the caller.
            unsafe { (*state_ptr).recompute_effective_profile() };
        }
        let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
        // SAFETY: `thread`'s lock is held exclusively by the caller.
        let new_ep = unsafe { (*state_ptr).effective_profile };
        // SAFETY: `thread`'s lock is held exclusively and no other reference to
        // `*state_ptr` is active during `update_dynamic_params`.
        update_dynamic_params(unsafe { &mut *state_ptr }, &old_ep, &new_ep, now);
    }

    // SAFETY: `thread`'s lock is held exclusively by the caller.
    let finish_time = unsafe { (*state_ptr).finish_time };
    if cfg!(debug_assertions) && finish_time.raw_value() < 0 {
        panic_pi_negative_finish_time(finish_time);
    }
}

/// Common priority inheritance handler for a target `OwnedWaitQueue`.
///
/// # Safety
/// - `owq` must point to a valid, initialized `OwnedWaitQueue` whose lock is
///   held exclusively by an active `ChainLockTransaction` (`TA_REQ(owq->get_lock())`)
///   and whose `inherited_scheduler_state_storage()` is non-null.
unsafe fn pi_common_owq<F>(
    owq: *mut OwnedWaitQueue,
    update_dynamic_params: F,
    old_target_ep: Option<&EffectiveProfile>,
) where
    F: FnOnce(
        &mut WaitQueueInheritedSchedulerState,
        &EffectiveProfile,
        &EffectiveProfile,
        SchedTime,
    ),
{
    let _trace = local_ktrace_begin_scope!(COMMON, "Pi::Common(OwnedWaitQueue)");
    let old_ep = match old_target_ep {
        Some(ep) => *ep,
        // SAFETY: `owq` is valid and locked by the caller.
        None => unsafe { owq_get_effective_profile(owq) },
    };
    // SAFETY: `owq` is valid and locked by the caller.
    let new_ep = unsafe { owq_get_effective_profile(owq) };
    // SAFETY: `owq` is valid, locked exclusively by the caller, and has non-null
    // inherited scheduler state storage.
    let iss = unsafe { &mut *cpp_owq_get_inherited_state(owq) };
    let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);

    update_dynamic_params(iss, &old_ep, &new_ep, now);

    if cfg!(debug_assertions) && iss.finish_time.raw_value() < 0 {
        panic_pi_negative_finish_time(iss.finish_time);
    }
}

// Called when a new edge is added connecting the target of one PI graph (the
// upstream node) to a different PI graph.
//
// At the point that this method is called, propagation of the static profile
// pressure parameters (weight, deadline utilization, and minimum deadline)
// should have already been propagated to `target`, and its `EffectiveProfile`
// updated. This method updates the dynamic scheduling parameters (start time,
// finish time, time slice remaining) of `target` as needed because of the join
// operation.
fn join_node_to_pi_graph_impl<U: PiNodeState, T: PiNodeState>(
    upstream: &U,
    target: &mut T,
    target_old_ep: &EffectiveProfile,
    target_new_ep: &EffectiveProfile,
    mono_now: SchedTime,
) {
    let upstream_ep = upstream.effective_profile();

    // If our upstream node is fair, then we have nothing more to do in the
    // common path. Our target's effective profile has already been updated
    // appropriately, and no changes to the target's dynamic deadline scheduling
    // parameters needs to be done (since new pressure from a fair thread
    // currently has no effect on deadline utilization). Any scheduler specific
    // side effects will be handled by the active thread path in `pi_common_thread`
    // if the target is an active thread.
    // TODO(https://fxbug.dev/42182908): Implement fair-to-fair priority
    // inheritance.
    if upstream_ep.is_fair() {
        return;
    }

    // Our upstream node is not a fair node, therefore it must be a deadline
    // node. In addition, no matter what it was before, our target node must now
    // be a deadline node.
    debug_assert!(upstream_ep.is_deadline());
    debug_assert!(target_new_ep.is_deadline());

    // Verify the start/finish times of the upstream node have the required
    // relationship.
    // TODO(https://fxbug.dev/448121736): Start time == finish time.
    debug_assert_pi_start_before_finish(
        "upstream_ep",
        upstream,
        upstream_ep.deadline().deadline_ns,
    );

    if target_old_ep.is_fair() {
        // If target has just now become deadline, we can simply transfer the
        // dynamic deadline parameters from upstream to the target.
        target.set_start_time(upstream.start_time());
        target.set_finish_time(upstream.finish_time());
        target.set_time_slice_ns(upstream.time_slice_ns());
        target.set_time_slice_used_ns(upstream.time_slice_used_ns());
    } else {
        // The target was already a deadline thread, then we need to recompute the
        // target's dynamic deadline parameters using the lag equation. Compute
        // the remaining periods of the target and upstream threads.
        let zero = SchedDuration::from_raw(0);
        let target_remaining_period = core::cmp::max(target.finish_time() - mono_now, zero);
        let upstream_remaining_period = core::cmp::max(upstream.finish_time() - mono_now, zero);
        let min_remaining_period =
            core::cmp::min(target_remaining_period, upstream_remaining_period);

        target.set_finish_time(core::cmp::min(target.finish_time(), upstream.finish_time()));
        target.set_start_time(target.finish_time() - target_new_ep.deadline().deadline_ns);

        // Verify the start/finish times of the target node have the required
        // relationship.
        // TODO(https://fxbug.dev/448121736): Start time == finish time.
        debug_assert_pi_start_before_finish(
            "target_new_ep",
            target,
            target_new_ep.deadline().deadline_ns,
        );

        // TODO(eieio): If a period is expired, the full bandwidth contribution of
        // the respective task is available to the target and downstream, if any.
        let new_remaining_time_slice = target.remaining_time_slice_ns()
            + upstream.remaining_time_slice_ns()
            + (target_old_ep.deadline().utilization
                * (min_remaining_period - target_remaining_period))
            + (upstream_ep.deadline().utilization
                * (min_remaining_period - upstream_remaining_period));

        // Limit the TSR. It cannot be less than zero nor can it be more than the
        // time until the absolute deadline of the new combined thread.
        //
        // TODO(johngro): If we did have to clamp the TSR, the amount we clamp by
        // needs to turn into carried lag.
        let clamped_remaining_time_slice =
            new_remaining_time_slice.clamp(zero, min_remaining_period);
        let target_cap = target_new_ep.deadline().capacity_ns;
        target.set_time_slice_ns(target_cap);
        target.set_time_slice_used_ns(target_cap - clamped_remaining_time_slice);
        if cfg!(debug_assertions) && target.remaining_time_slice_ns() < zero {
            panic_pi_negative_remaining_time_slice(
                target_cap,
                new_remaining_time_slice,
                min_remaining_period,
            );
        }
    }
}

// Called when an upstream node has its downstream edge removed, splitting it
// from the PI graph it was a member of and becoming the target of a new graph
// in the process.
//
// At the point that this method is called, propagation of the static profile
// pressure parameters (weight, deadline utilization, and minimum deadline)
// should have already been propagated to `target`, and its `EffectiveProfile`
// updated. This method updates the dynamic scheduling parameters (start time,
// finish time, time slice remaining) in both `target` and `upstream` as needed
// because of the split operation.
fn split_node_from_pi_graph_impl<U: PiNodeState, T: PiNodeState>(
    upstream: &mut U,
    target: &mut T,
    target_old_ep: &EffectiveProfile,
    target_new_ep: &EffectiveProfile,
    mono_now: SchedTime,
) {
    let upstream_ep = upstream.effective_profile();

    // Was the target node a fair node? If so, there is really nothing for us to
    // do here.
    if target_old_ep.is_fair() {
        return;
    }

    debug_assert!(target_old_ep.is_deadline());
    if target_new_ep.is_fair() {
        // If target node is now a fair node, then the upstream node must have
        // been a deadline node. This split operation is what caused the target
        // node to change from deadline to fair, all of the deadline pressure must
        // have been coming from the upstream node. Assert all of this.
        debug_assert!(upstream_ep.is_deadline());
        if cfg!(debug_assertions) {
            let (toep, uep) = (target_old_ep.deadline(), upstream_ep.deadline());
            if toep.capacity_ns != uep.capacity_ns {
                panic_pi_deadline_param_mismatch("capacity", toep.capacity_ns, uep.capacity_ns);
            }
            if toep.deadline_ns != uep.deadline_ns {
                panic_pi_deadline_param_mismatch("deadline", toep.deadline_ns, uep.deadline_ns);
            }
        }

        // Verify the start/finish times of the target node have the required
        // relationship.
        // TODO(https://fxbug.dev/448121736): Start time == finish time.
        debug_assert_pi_start_before_finish(
            "target_old_ep",
            target,
            target_old_ep.deadline().deadline_ns,
        );

        // Give the dynamic deadline parameters over to the upstream node.
        upstream.set_start_time(target.start_time());
        upstream.set_finish_time(target.finish_time());
        upstream.set_time_slice_ns(target.time_slice_ns());
        upstream.set_time_slice_used_ns(target.time_slice_used_ns());

        // TODO(eieio): Just expire the time slice for now. This should actually
        // be split out the same way as for deadline threads.
        target.set_time_slice_used_ns(target.time_slice_ns());
    } else {
        // OK, the target node is still a deadline node. If the upstream node is a
        // fair node, we don't have to do anything at all. A fair node splitting
        // off from a deadline node should not change the deadline node's dynamic
        // parameters. If the upstream fair node is a thread, it is going to
        // arrive in a new scheduler queue Real Soon Now, and have new dynamic
        // parameters computed for it.
        //
        // If both nodes are deadline nodes, then we need to invoke the lag
        // equation in order to figure out what the new time slice remaining and
        // absolute deadlines are.
        if upstream_ep.is_deadline() {
            // Compute the time until absolute deadline of the target and upstream.
            let zero = SchedDuration::from_raw(0);
            let target_remaining_period = core::cmp::max(target.finish_time() - mono_now, zero);
            let upstream_remaining_period = core::cmp::max(upstream.finish_time() - mono_now, zero);

            // Figure out what the uncapped utilization of the combined thread
            // would have been based on the utilizations of the target and
            // upstream nodes after the split. It is important when scaling
            // timeslices to be sure that we divide by a utilization value which
            // is the sum of the two (now separated) utilization values.
            let combined_uncapped_utilization =
                target_new_ep.deadline().utilization + upstream_ep.deadline().utilization;
            let upstream_utilization_ratio = SchedUtilization::from(
                upstream_ep.deadline().utilization / combined_uncapped_utilization,
            );

            let new_upstream_remaining_time_slice = SchedDuration::from(
                upstream_utilization_ratio * target.remaining_time_slice_ns()
                    + upstream_ep.deadline().utilization
                        * (upstream_remaining_period - target_remaining_period),
            );

            // TODO(johngro): This also changes when carried lag comes into play.
            let upstream_cap = upstream_ep.deadline().capacity_ns;
            upstream.set_time_slice_ns(upstream_cap);
            upstream.set_time_slice_used_ns(
                upstream_cap - core::cmp::max(new_upstream_remaining_time_slice, zero),
            );

            // TODO(johngro): Fix this. Logically, it is not correct to preserve the
            // abs deadline of the target after the split. The target's bookkeeping
            // should be equivalent to the values which would be obtained by joining
            // all of the threads which exist upstream of this node together.
            // Because of this, our new target finish time should be equal to the
            // min across all finish times immediately upstream of this node.
            //
            // Now handle the target node. We preserve the absolute deadline of the
            // target node before and after the split, so we need to recompute its
            // start time so that the distance between the absolute deadline and the
            // start time is equal to the new relative deadline of the target node.
            target.set_start_time(target.finish_time() - target_new_ep.deadline().deadline_ns);

            // Verify the start/finish times of the target node have the required
            // relationship.
            // TODO(https://fxbug.dev/448121736): Start time == finish time.
            debug_assert_pi_start_before_finish(
                "target_new_ep",
                target,
                target_new_ep.deadline().deadline_ns,
            );

            // The time till absolute deadline of the pre and post split target
            // remains the same, so the ttad contributions to the timeslice
            // remaining simply drop out of the lag equation.
            //
            // Note that fixed point division takes the precision of the assignee
            // into account to provide headroom in certain situations. Use an
            // intermediate with the same fractional precision as the utilization
            // operands before scaling the non-fractional timeslice.
            let target_utilization_ratio = SchedUtilization::from(
                target_new_ep.deadline().utilization / combined_uncapped_utilization,
            );
            let new_target_remaining_time_slice =
                SchedDuration::from(target.remaining_time_slice_ns() * target_utilization_ratio);

            let target_cap = target_new_ep.deadline().capacity_ns;
            target.set_time_slice_ns(target_cap);
            target.set_time_slice_used_ns(
                target_cap - core::cmp::max(new_target_remaining_time_slice, zero),
            );
        }
    }
}

/// Updates a thread's effective profile and position in its container at the
/// start of a base profile update operation, regardless of whether or not the
/// target thread is currently blocked or currently assigned to a scheduler.
///
/// Later on, if the thread happens to be an upstream member of a PI graph whose
/// target is either an `OwnedWaitQueue` or another thread, the
/// `rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_*` operation
/// will be triggered to handle the downstream target of the graph.
///
/// # Safety
/// - `thread` must point to a valid, initialized `Thread` whose lock is held
///   exclusively by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, thread->get_lock(), preempt_disabled_token)`).
/// - If `thread` is blocked in a wait queue, that wait queue's lock must also
///   be held in the transaction.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_thread_base_profile_changed(thread: *mut Thread) {
    // The base profile of this thread has changed. While there may or may not be
    // something downstream of this thread, we need to start by dealing with
    // updating this thread's static and dynamic scheduling parameters first.
    // SAFETY: `thread` is valid and its lock is held by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(thread))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_BASE_DIRTY);
        pi_common_thread(thread, |target_state, _target_old_ep, target_new_ep, _mono_now| {
            // Make sure the start and finish times are consistent with the bandwidth
            // parameters of the deadline profile. Consistency in fair profiles is
            // handled by `Scheduler::adjust_fair_bandwidth`.
            if target_new_ep.is_deadline() {
                target_state.start_time =
                    target_state.finish_time - target_new_ep.deadline().deadline_ns;
            }
        });
    }
}

/// Called when a thread (`_upstream`) in a graph whose target is a different
/// `Thread` (`target`) changes its base profile in order to update the target's
/// new effective profile, position in container, and dynamic scheduling
/// parameters.
///
/// # Safety
/// - `_upstream` and `target` must point to distinct, valid, initialized `Thread`s
///   whose locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, _upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_thread(
    _upstream: *const Thread,
    target: *mut Thread,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: base profile changed");
    // The base profile of a thread upstream of this target node has changed. We need to
    // do the following:
    //
    // 1) Recompute the target's effective profile.
    // 2) Handle any bookkeeping updates for the scheduler's state, if the target
    //    is a thread which is either RUNNING or READY, and therefore has a
    //    scheduler assigned to it.
    // 3) Handle any updates to the target's dynamic scheduling parameters (e.g.,
    //    start time, finish time, time slice remaining).
    debug_assert!(!core::ptr::eq(_upstream, target));
    // SAFETY: `_upstream` and `target` are distinct, valid, and locked by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(target))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_INHERITED_DIRTY);
        (*cpp_thread_get_scheduler_state(_upstream.cast_mut()))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_CLEAN);
        pi_common_thread(target, |target_state, _target_old_ep, target_new_ep, _mono_now| {
            // Make sure the start and finish times are consistent with the bandwidth
            // parameters of the deadline profile. Consistency in fair profiles is
            // handled by `Scheduler::adjust_fair_bandwidth`.
            if target_new_ep.is_deadline() {
                target_state.start_time =
                    target_state.finish_time - target_new_ep.deadline().deadline_ns;
            }
        });
    }
}

/// Called when a thread (`_upstream`) in a graph whose target is an
/// `OwnedWaitQueue` (`target`) changes its base profile in order to update the
/// target's new effective profile, position in container, and dynamic
/// scheduling parameters.
///
/// # Safety
/// - `_upstream` and `target` must point to valid, initialized objects whose
///   locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, _upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_owq(
    _upstream: *const Thread,
    target: *mut OwnedWaitQueue,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: base profile changed");
    // SAFETY: `_upstream` and `target` are valid and locked by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(_upstream.cast_mut()))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_CLEAN);
        pi_common_owq(
            target,
            |target_iss, _target_old_ep, target_new_ep, _mono_now| {
                // Make sure the start and finish times are consistent with the bandwidth
                // parameters of the deadline profile. Consistency in fair profiles is
                // handled by `Scheduler::adjust_fair_bandwidth`.
                if target_new_ep.is_deadline() {
                    target_iss.start_time =
                        target_iss.finish_time - target_new_ep.deadline().deadline_ns;
                }
            },
            None,
        );
    }
}

/// Joins an upstream thread to a target thread in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to distinct, valid, initialized `Thread`s
///   whose locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_join_thread_thread(
    upstream: *const Thread,
    target: *mut Thread,
    _force_inheritance: super::wait_queue::ForceInheritance,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: join");
    debug_assert!(!core::ptr::eq(upstream, target));
    // SAFETY: `upstream` and `target` are distinct, valid, and locked by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(target))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_INHERITED_DIRTY);
        (*cpp_thread_get_scheduler_state(upstream.cast_mut()))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_CLEAN);
        pi_common_thread(target, |target_state, target_old_ep, target_new_ep, mono_now| {
            let upstream_state = &*cpp_thread_get_scheduler_state(upstream.cast_mut());
            join_node_to_pi_graph_impl(
                upstream_state,
                target_state,
                target_old_ep,
                target_new_ep,
                mono_now,
            );
        });
    }
}

/// Joins an upstream thread to a target OwnedWaitQueue in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to valid, initialized objects whose
///   locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_join_thread_owq(
    upstream: *const Thread,
    target: *mut OwnedWaitQueue,
    _force_inheritance: super::wait_queue::ForceInheritance,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: join");
    // SAFETY: `upstream` and `target` are valid and locked by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(upstream.cast_mut()))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_CLEAN);
        pi_common_owq(
            target,
            |target_iss, target_old_ep, target_new_ep, mono_now| {
                let upstream_state = &*cpp_thread_get_scheduler_state(upstream.cast_mut());
                join_node_to_pi_graph_impl(
                    upstream_state,
                    target_iss,
                    target_old_ep,
                    target_new_ep,
                    mono_now,
                );
            },
            None,
        );
    }
}

/// Joins an upstream OwnedWaitQueue to a target thread in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to valid, initialized objects whose
///   locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_join_owq_thread(
    upstream: *const OwnedWaitQueue,
    target: *mut Thread,
    _force_inheritance: super::wait_queue::ForceInheritance,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: join");
    // SAFETY: `upstream` and `target` are valid and locked by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(target))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_INHERITED_DIRTY);
        pi_common_thread(target, |target_state, target_old_ep, target_new_ep, mono_now| {
            let upstream_iss = &*cpp_owq_get_inherited_state_const(upstream);
            join_node_to_pi_graph_impl(
                upstream_iss,
                target_state,
                target_old_ep,
                target_new_ep,
                mono_now,
            );
        });
    }
}

/// Joins an upstream OwnedWaitQueue to a target OwnedWaitQueue in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to distinct, valid, initialized
///   `OwnedWaitQueue`s whose locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_join_owq_owq(
    upstream: *const OwnedWaitQueue,
    target: *mut OwnedWaitQueue,
    _force_inheritance: super::wait_queue::ForceInheritance,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: join");
    debug_assert!(!core::ptr::eq(upstream, target));
    // SAFETY: `upstream` and `target` are distinct, valid, and locked by the caller.
    unsafe {
        pi_common_owq(
            target,
            |target_iss, target_old_ep, target_new_ep, mono_now| {
                let upstream_iss = &*cpp_owq_get_inherited_state_const(upstream);
                join_node_to_pi_graph_impl(
                    upstream_iss,
                    target_iss,
                    target_old_ep,
                    target_new_ep,
                    mono_now,
                );
            },
            None,
        );
    }
}

/// Splits an upstream thread from a target thread in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to distinct, valid, initialized `Thread`s
///   whose locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_split_thread_thread(
    upstream: *mut Thread,
    target: *mut Thread,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: split");
    debug_assert!(!core::ptr::eq(upstream, target));
    // SAFETY: `upstream` and `target` are distinct, valid, and locked by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(target))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_INHERITED_DIRTY);
        (*cpp_thread_get_scheduler_state(upstream))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_CLEAN);
        pi_common_thread(target, |target_state, target_old_ep, target_new_ep, mono_now| {
            let upstream_state = &mut *cpp_thread_get_scheduler_state(upstream);
            split_node_from_pi_graph_impl(
                upstream_state,
                target_state,
                target_old_ep,
                target_new_ep,
                mono_now,
            );
        });
    }
}

/// Splits an upstream thread from a target OwnedWaitQueue in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to valid, initialized objects whose
///   locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
/// - `old_target_ep` may be null or point to a valid `EffectiveProfile`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_split_thread_owq(
    upstream: *mut Thread,
    target: *mut OwnedWaitQueue,
    old_target_ep: *const EffectiveProfile,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: split");
    // SAFETY: `upstream` and `target` are valid and locked by the caller, and
    // `old_target_ep` is either null or points to a valid `EffectiveProfile`.
    unsafe {
        (*cpp_thread_get_scheduler_state(upstream))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_CLEAN);
        pi_common_owq(
            target,
            |target_iss, target_old_ep, target_new_ep, mono_now| {
                let upstream_state = &mut *cpp_thread_get_scheduler_state(upstream);
                split_node_from_pi_graph_impl(
                    upstream_state,
                    target_iss,
                    target_old_ep,
                    target_new_ep,
                    mono_now,
                );
            },
            old_target_ep.as_ref(),
        );
    }
}

/// Splits an upstream OwnedWaitQueue from a target thread in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to valid, initialized objects whose
///   locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_split_owq_thread(
    upstream: *mut OwnedWaitQueue,
    target: *mut Thread,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: split");
    // SAFETY: `upstream` and `target` are valid and locked by the caller.
    unsafe {
        (*cpp_thread_get_scheduler_state(target))
            .effective_profile
            .assert_dirty_state(EffectiveProfile::DIRTY_FLAG_INHERITED_DIRTY);
        pi_common_thread(target, |target_state, target_old_ep, target_new_ep, mono_now| {
            let upstream_iss = &mut *cpp_owq_get_inherited_state(upstream);
            split_node_from_pi_graph_impl(
                upstream_iss,
                target_state,
                target_old_ep,
                target_new_ep,
                mono_now,
            );
        });
    }
}

/// Splits an upstream OwnedWaitQueue from a target OwnedWaitQueue in the PI graph.
///
/// # Safety
/// - `upstream` and `target` must point to distinct, valid, initialized
///   `OwnedWaitQueue`s whose locks are both held by an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, upstream->get_lock(), target->get_lock(), preempt_disabled_token)`).
/// - `old_target_ep` may be null or point to a valid `EffectiveProfile`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_pi_split_owq_owq(
    upstream: *mut OwnedWaitQueue,
    target: *mut OwnedWaitQueue,
    old_target_ep: *const EffectiveProfile,
) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_pi: split");
    debug_assert!(!core::ptr::eq(upstream, target));
    // SAFETY: `upstream` and `target` are distinct, valid, and locked by the caller,
    // and `old_target_ep` is either null or points to a valid `EffectiveProfile`.
    unsafe {
        pi_common_owq(
            target,
            |target_iss, target_old_ep, target_new_ep, mono_now| {
                let upstream_iss = &mut *cpp_owq_get_inherited_state(upstream);
                split_node_from_pi_graph_impl(
                    upstream_iss,
                    target_iss,
                    target_old_ep,
                    target_new_ep,
                    mono_now,
                );
            },
            old_target_ep.as_ref(),
        );
    }
}

const MIGRATE_STAGE_SAVE: i32 = 0;
const MIGRATE_STAGE_RESTORE: i32 = 1;

/// Reset this CPU's preemption timer to fire at `deadline`.
///
/// `current_cpu` is the current CPU.
///
/// `now` is the time that was latched when the CPU entered the scheduler.
///
/// # Safety
/// Must be called with interrupts disabled on `current_cpu`, and `current_cpu`
/// must be a valid initialized CPU number.
unsafe fn preempt_reset(
    current_cpu: CpuNum,
    now: zx_types::zx_instant_mono_t,
    deadline: zx_types::zx_instant_mono_t,
) {
    let _trace = local_ktrace_begin_scope!(
        DETAILED,
        "preempt_reset",
        "now" => now,
        "deadline" => deadline,
    );
    // Setting a preemption time that's prior to the point at which the CPU
    // entered the scheduler indicates at worst a bug, or at best a wasted
    // reschedule.
    //
    // What do we mean by wasted reschedule? When leaving the scheduler, if the
    // preemption timer was set to a point prior to entering the scheduler, it
    // will immediately fire once we've re-enabled interrupts. When it fires,
    // we'll then re-enter the scheduler (provided that preemption is not
    // disabled) without running the current task for any appreciable amount of
    // time. Instead, we should have set the preemption timer to a point that's
    // after the latched entry time and avoided an unnecessary interrupt and trip
    // through the scheduler.
    //
    // TODO(https://fxbug.dev/42182770): For now, simply count the number of times
    // this happens. Once we have eliminated all causes of "preemption time in
    // the past" (whether they are bugs or simply missed optimization
    // opportunities) replace this counter with a `debug_assert!`.
    //
    // Aside from simple bugs, how can we end up with a preemption time that's
    // earlier than the time at which we last entered the scheduler? Consider the
    // case where the current thread, A, is blocking and the run queue contains
    // just one thread, B, that's deadline scheduled and whose finish time has
    // already elapsed. In other words, B's finish time is earlier than the time
    // at which we last entered the scheduler. In this case, we'll end up
    // scheduling B and setting the preemption timer to B's finish time. Longer
    // term, we should consider resetting or reactivating all eligible tasks whose
    // finish times would be earlier than the point at which we entered the
    // scheduler. See also `COUNTER_DEADLINE_PAST`.
    if deadline < now {
        COUNTER_PREEMPT_PAST.add(1);
    }
    // SAFETY: `current_cpu` is the current CPU with interrupts disabled, so this is the current
    // CPU's timer queue.
    unsafe { PerCpu::get(current_cpu).timer_queue.preempt_reset(deadline) };
}

impl Scheduler {
    /// Updates bandwidth reservation cache and power level control during reschedule.
    ///
    /// # Safety
    /// The caller must ensure `self` is the current CPU's `Scheduler` with
    /// interrupts disabled and `queue_lock_` held (`TA_REQ(queue_lock_)`).
    unsafe fn reschedule_power_control(&self, now: SchedTime, next_is_idle: bool) {
        // SAFETY: `self`'s `queue_lock_` is held by the caller.
        if unsafe { self.power_level_control_is_enabled() } {
            // After the current thread has been potentially removed from this
            // scheduler, clear or prune the bandwidth demand cache and re-evaluate the
            // current power level.
            // SAFETY: `self`'s `queue_lock_` is held.
            let clear_cache = unsafe {
                next_is_idle
                    && rust_kernel_run_queue_is_empty(self.critical_deadline_queue())
                    && rust_kernel_run_queue_is_empty(self.deadline_queue())
            };
            // SAFETY: `self`'s `queue_lock_` is held exclusively, synchronizing access
            // to `bandwidth_reservation_cache`.
            let utilization_to_remove = unsafe {
                let cache = &mut (*self.state.get()).bandwidth_reservation_cache;
                if clear_cache { cache.clear() } else { cache.prune(now) }
            };
            if utilization_to_remove > SchedUtilization::from_raw(0) {
                local_ktrace_instant!(
                    QUEUE,
                    "clear/prune",
                    "utilization_to_remove" => utilization_to_remove,
                );
                // SAFETY: `self`'s `queue_lock_` is held.
                unsafe {
                    self.update_total_deadline_utilization(SchedUtilization::from_raw(
                        -utilization_to_remove.raw_value(),
                    ));
                }
            }

            // SAFETY: `self`'s `queue_lock_` is held with interrupts disabled.
            unsafe {
                // Evaluate changing the processing rate when the required utilization
                // moves outside the bounds of the current power level. All of the
                // processors in the same domain must be re-evaluated to determine whether
                // an actual rate change should occur.
                self.power_level_control_reevaluate_if_needed();

                // Send a pending power level request before updating the processing rate.
                //
                // WARNING: This call temporarily drops the queue lock when calling the
                // fast-path power level controller. Do not split updates to different
                // values across this call that need to be coherently read from another CPU
                // and synchronized using the queue lock. Doing so creates an incomplete
                // critical section that can result in incoherent reads of the values
                // updated in different acquisitions of the queue lock.
                let reschedule_mask = self.send_pending_power_level_request();
                if reschedule_mask != 0 {
                    // Reschedule the other CPUs affected by the power level change. The
                    // current CPU is filtered out by `mp::reschedule`.
                    super::mp::reschedule(reschedule_mask, 0);
                }
            }
        }
    }

    /// Computes the next preemption time for `result.next_thread`.
    ///
    /// # Safety
    /// The caller must ensure `self`'s `queue_lock_` and `next_thread`'s lock are
    /// held (`TA_REQ(queue_lock_, next_thread->get_lock())`), `current_thread` is a
    /// valid `Thread` pointer, and `result` was produced by
    /// `reschedule_common_helper` for this reschedule.
    unsafe fn calculate_preemption_time(
        &self,
        current_thread: *mut Thread,
        result: &RustRescheduleResult,
        now: SchedTime,
        processing_rate_updated: bool,
    ) -> SchedTime {
        let next_thread = result.next_thread;
        // SAFETY: `next_thread` is valid.
        let next_state_ptr = unsafe { cpp_thread_get_scheduler_state(next_thread) };
        let next_is_idle = result.next_is_idle;
        let timeslice_expired = result.timeslice_expired;
        // Each path below must set the preemption time.
        // TODO(eieio): Debug assert that the value set is >= now.
        let mut preemption_time_ns;

        if next_is_idle {
            let _trace_stop_preemption = local_ktrace_begin_scope!(DETAILED, "idle");
            // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
            let target = unsafe {
                (*next_state_ptr).last_started_running = now;

                // If there are no tasks to run in the future or there is idle/power work to
                // perform, disable the preemption timer. Otherwise, set the preemption
                // time to the earliest eligible time.
                let target = if cpp_scheduler_has_pending_power_work(self) {
                    SchedTime::from_raw(zx_types::ZX_TIME_INFINITE)
                } else {
                    self.get_next_eligible_time()
                };
                self.set_target_preemption_time_ns(target);
                target
            };
            preemption_time_ns = target;
        } else if timeslice_expired || current_thread != next_thread {
            let trace_start_preemption = local_ktrace_begin_scope!(DETAILED, "next_slice");

            // Re-compute the time slice and ideal preemption time for the new thread
            // based on the latest state.
            // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
            let target = unsafe {
                let target = self.next_thread_timeslice(next_thread, now);
                self.set_target_preemption_time_ns(target);
                // Update the incoming thread's runtime stats to record the amount of time
                // that it spent in the run queue when switching to a different thread.
                if current_thread != next_thread {
                    cpp_thread_update_runtime_stats(next_thread, (*next_state_ptr).state as i32);
                }
                (*next_state_ptr).last_started_running = now;
                self.set_start_of_current_time_slice_ns(now);
                self.set_scheduled_weight_total(self.weight_total());
                target
            };

            // Adjust the preemption time to account for a deadline thread becoming
            // eligible before the current time slice expires.
            // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
            preemption_time_ns = unsafe {
                if (*next_state_ptr).effective_profile.is_fair() {
                    self.clamp_to_deadline(target)
                } else {
                    self.clamp_to_earlier_deadline(target, next_thread)
                }
            };
            debug_assert!(preemption_time_ns <= target);

            ktrace_rs::end_scope!(
                trace_start_preemption,
                "preemption_time" => preemption_time_ns,
                "target preemption time" => target,
            );

            // Emit a flow end event to match the flow begin event emitted when the
            // thread was enqueued. Emitting in this scope ensures that thread just
            // came from the run queue (and is not the idle thread).
            // SAFETY: `next_thread` is valid and its lock is held.
            let (flow_id, tid) =
                unsafe { ((*next_state_ptr).flow_id, Thread::tid_ptr(next_thread)) };
            local_ktrace_flow_end!(
                FLOW,
                "sched_latency",
                flow_id,
                "tid" => tid,
            );
        } else {
            let trace_continue = local_ktrace_begin_scope!(DETAILED, "continue");
            debug_assert_eq!(current_thread, next_thread);

            // Update the target preemption time for consistency with the updated CPU
            // processing rate.
            // TODO(eieio): Eventually update the effective bandwidth for all threads.
            // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
            unsafe {
                if processing_rate_updated && (*next_state_ptr).effective_profile.is_deadline() {
                    let target = self.next_thread_timeslice(next_thread, now);
                    self.set_target_preemption_time_ns(target);
                }
            }

            // The current thread should continue to run. A throttled deadline thread
            // might become eligible before the current time slice expires. Figure out
            // whether to set the preemption time earlier to switch to the newly
            // eligible thread.
            //
            // The preemption time should be set earlier when either:
            //   * Current is a fair thread and a deadline thread will become eligible
            //     before its time slice expires.
            //   * Current is a deadline thread and a deadline thread with an earlier
            //     deadline will become eligible before its time slice expires.
            //
            // Note that the target preemption time remains set to the ideal
            // preemption time for the current task, even if the preemption timer is set
            // earlier. If a task that becomes eligible is stolen before the early
            // preemption is handled, this logic will reset to the original target
            // preemption time.
            // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
            let target = unsafe { self.target_preemption_time_ns() };
            // SAFETY: `next_thread`'s lock and `queue_lock_` are held.
            preemption_time_ns = unsafe {
                if (*next_state_ptr).effective_profile.is_fair() {
                    self.clamp_to_deadline(target)
                } else {
                    self.clamp_to_earlier_deadline(target, next_thread)
                }
            };
            debug_assert!(preemption_time_ns <= target);

            ktrace_rs::end_scope!(
                trace_continue,
                "preemption_time" => preemption_time_ns,
                "target preemption time" => target,
            );
        }

        // Make sure the bandwidth demand cache will get pruned if there are
        // remaining valid entries.
        // SAFETY: `self`'s `queue_lock_` is held.
        if unsafe { self.power_level_control_is_enabled() } {
            // SAFETY: `self`'s `queue_lock_` is held exclusively.
            preemption_time_ns = unsafe {
                (*self.state.get())
                    .bandwidth_reservation_cache
                    .clamp_to_next_finish_time(preemption_time_ns)
            };
        }

        debug_assert_ne!(preemption_time_ns, SchedTime::min());
        preemption_time_ns
    }

    /// Completes the post-context-switch lock handoff on the current CPU.
    ///
    /// # Safety
    /// - Interrupts must be disabled on the current CPU.
    /// - An active `ChainLockTransaction` on the current CPU must hold both
    ///   `previous_thread`'s lock and `current_thread`'s lock with the reserved
    ///   scheduler token (`TA_REQ(chainlock_transaction_token)`,
    ///   `TA_REL(previous_thread()->get_lock())`).
    /// - `saved_state` must belong to a live, stack-pinned `ChainLockTransaction`
    ///   on `current_thread`'s stack.
    unsafe fn lock_handoff_internal(saved_state: &SavedState, current_thread: *mut Thread) {
        debug_assert!(crate::arch_rs::ints_disabled());
        // SAFETY: Interrupts are disabled, so `curr_cpu_num()` is stable and its
        // `Scheduler` is initialized.
        let scheduler = unsafe { cpp_scheduler_get(crate::arch_rs::curr_cpu_num()) };
        // SAFETY: Interrupts are disabled and `scheduler` is the current CPU's
        // initialized `Scheduler`. `previous_thread` is only accessed on the local
        // CPU with interrupts disabled, so a relaxed load + store suffices without
        // an atomic RMW.
        let previous_thread = unsafe {
            let prev = (*(*scheduler).state.get())
                .previous_thread
                .load(core::sync::atomic::Ordering::Relaxed);
            (*(*scheduler).state.get())
                .previous_thread
                .store(core::ptr::null_mut(), core::sync::atomic::Ordering::Relaxed);
            prev
        };
        debug_assert!(!previous_thread.is_null());

        // SAFETY: The caller guarantees that an active `ChainLockTransaction` holds
        // both `previous_thread`'s lock and `current_thread`'s lock with the
        // reserved token, and that `saved_state` belongs to a live, stack-pinned
        // `ChainLockTransaction` on `current_thread`'s stack.
        unsafe {
            let token = ChainLockTransaction::assert_active();
            let prev_lock = thread_get_lock(previous_thread);
            (*prev_lock).assert_acquired(&token);
            ChainLockTransaction::active_ref(&token).assert_num_locks_held(2);
            ChainLockTransaction::restore_state(*saved_state);
            let curr_lock = thread_get_lock(current_thread);
            (*curr_lock).sync_token(&token);
            (*prev_lock).sync_token(&token);
            (*prev_lock).release(&token);
        }
    }

    /// Performs the architecture and VMM context switch from `current_thread` to
    /// `next_thread` and completes the lock handoff when `current_thread` resumes.
    ///
    /// # Safety
    /// - `self` must be the current CPU's `Scheduler`, with interrupts disabled and
    ///   `queue_lock_` unlocked.
    /// - `current_thread` and `next_thread` must be distinct, valid `Thread`
    ///   pointers whose locks are both held by the active `ChainLockTransaction` on
    ///   the current CPU using the reserved scheduler token (`saved_state`).
    /// - `saved_state` must refer to a `ChainLockTransaction` pinned on
    ///   `current_thread`'s stack across the context switch.
    unsafe fn context_switch(
        &self,
        current_thread: *mut Thread,
        next_thread: *mut Thread,
        saved_state: &SavedState,
        end_inner_trace: Option<ktrace_rs::KTraceScope<'static, 0>>,
        end_outer_trace: Option<&mut dyn ktrace_rs::ErasedScope>,
    ) {
        // SAFETY: Both `current_thread` and `next_thread` are valid and locked by
        // the caller, and interrupts are disabled on `self`'s CPU.
        unsafe {
            // Trace the activation of the next thread before context switching.
            sched_trace_context_switch(current_thread, next_thread, self.this_cpu());

            // We invoke the context switch functions before context switching, so that
            // they have a chance to correctly perform the actions required. Doing so
            // after context switching may lead to an invalid CPU state.
            cpp_thread_call_context_switch_fn_locked(current_thread);
            cpp_thread_call_context_switch_fn_locked(next_thread);

            // Notes about Thread aspace rules:
            //
            // Typically, it is only safe for the current thread to access its aspace
            // member directly, as only a running thread can change its own aspace, and
            // if a thread is running, then its process must also be alive and therefore
            // its aspace must also be alive.
            //
            // Context switching is a bit of an edge case. The current thread is
            // becoming the next thread. Both aspaces must still be alive (even if the
            // current thread is in the process of becoming rescheduled for the very
            // last time as it exits), and neither one can change its own aspace right
            // now (they are not really running).
            //
            // Because of this, it should be OK for us to directly access the aspaces
            // during the context switch, without needing to either check the thread's
            // state, or add any references to the VmAspace object.
            let current_aspace = cpp_thread_get_aspace(current_thread);
            let next_aspace = cpp_thread_get_aspace(next_thread);
            if current_aspace != next_aspace {
                cpp_vmm_context_switch(current_aspace, next_aspace);
            }
        }

        super::stats::inc_context_switches();

        // Prevent the scheduler durations from spanning the context switch.
        // Some context switches do not resume within this method on the other
        // thread, which results in unterminated durations. All of the callers
        // with durations tail-call this method, so terminating the duration
        // here should not cause significant inaccuracy of the outer duration.
        // The inner scope is owned and ends on drop; the outer scope is owned by the
        // C++ caller and ended through its erased callback.
        drop(end_inner_trace);
        if let Some(scope) = end_outer_trace {
            scope.end();
        }

        if SCHEDULER_QUEUE_TRACING_ENABLED {
            let arg0: u64 = 0;
            // SAFETY: `self` is valid and `this_cpu` is initialized during early boot.
            let arg1: u64 = core::cmp::min(unsafe { self.this_cpu() } as u64, 0xF) << 16;
            ktrace_rs::instant!(
                "kernel:probe",
                "tqe_afinish",
                ktrace_rs::Context::Cpu,
                "arg0" => arg0,
                "arg1" => arg1,
            );
        }

        // SAFETY: Interrupts are disabled, `self` is the current CPU's `Scheduler`,
        // and both `current_thread` and `next_thread` are locked with the reserved
        // scheduler token saved in `saved_state`. After `cpp_arch_context_switch`
        // returns on `current_thread`'s stack, `Self::lock_handoff_internal` looks
        // up the new CPU's `Scheduler` rather than using `self` or `next_thread`.
        unsafe {
            // Time for the context switch. Before actually performing the switch,
            // record the current thread as the "previous thread" for the next thread
            // becoming scheduled. The next thread needs to know which thread preceded
            // it in order to drop its lock before unwinding, and our current stack is
            // not going to be available to it.
            debug_assert!(
                (*self.state.get())
                    .previous_thread
                    .load(core::sync::atomic::Ordering::Relaxed)
                    .is_null()
            );
            (*self.state.get())
                .previous_thread
                .store(current_thread, core::sync::atomic::Ordering::Relaxed);
            cpp_arch_context_switch(current_thread, next_thread);

            // We made it to the other side of the switch. We have switched stacks, so
            // the local variable meanings have become redefined.
            //
            // ++ The value of `current_thread` after the context switch is the value of
            //    `next_thread` before the switch.
            // ++ The value of `next_thread` after the context switch is the thread that
            //    the new `current_thread` context-switched to the last time it ran
            //    through here. We don't even know if this thread exists now, so its
            //    value is junk.
            // ++ The value of `current_thread` before the switch has been stashed in
            //    this scheduler's `previous_thread` member.

            // If the new current thread has a restartable sequence registered, we
            // need to signal the thread to check for the restartable sequence before
            // it returns to userspace.
            cpp_thread_signal_check_restartable_sequence_if_needed(current_thread);

            // We need to drop the old current thread's lock, while continuing to hold
            // the new current thread's lock as we unwind.
            // `Self::lock_handoff_internal` will take care of dropping the previous
            // `current_thread`'s lock.
            Self::lock_handoff_internal(saved_state, current_thread);
        }
    }

    /// Common logic for reschedule API.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// - `self` is the current CPU's `Scheduler` with interrupts disabled and
    ///   `queue_lock_` not held (`TA_EXCL(queue_lock_)`).
    /// - `current_thread` is the current CPU's active thread (`current_get()`),
    ///   and its lock is the only lock held by an active, finalized
    ///   `ChainLockTransaction`
    ///   (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`).
    unsafe fn reschedule_common(
        &self,
        current_thread: *mut Thread,
        end_outer_trace: Option<&mut dyn ktrace_rs::ErasedScope>,
    ) {
        let mut trace = local_ktrace_begin_scope!(DETAILED, "reschedule_common");
        debug_assert_eq!(current_thread, super::thread::current_get());
        let current_cpu = crate::arch_rs::curr_cpu_num();
        // SAFETY: `self` is valid and `this_cpu` is initialized.
        debug_assert_eq!(current_cpu, unsafe { self.this_cpu() });

        // No spinlocks should be held as we come into a reschedule operation. Only
        // the current thread's lock should be held.
        // SAFETY: Querying per-CPU spinlock count with interrupts disabled.
        debug_assert_eq!(unsafe { cpp_arch_num_spinlocks_held() }, 0);
        debug_assert!(!crate::arch_rs::blocking_disallowed());

        // SAFETY: The caller guarantees an active `ChainLockTransaction` on the
        // current CPU.
        let token = unsafe { ChainLockTransaction::assert_active() };
        ChainLockTransaction::active_ref(&token).assert_num_locks_held(1);
        super::stats::inc_reschedules();

        // Prepare for rescheduling by backing up the chain lock context and switching
        // to the special "sched token" which we use during reschedule operations in
        // order to guarantee that we will win arbitration with other active
        // transactions which are not reschedule operations. We will restore this
        // context at the end of the operation (whether we context switched or not).
        let saved_state = ChainLockTransaction::save_state_and_use_reserved_token(&token);
        // SAFETY: `current_thread` is valid and its lock is held by the active
        // transaction, which now uses the reserved token.
        let curr_lock = unsafe {
            let curr_lock = thread_get_lock(current_thread);
            (*curr_lock).sync_token(&token);
            curr_lock
        };

        // Process all threads in the `save_state_list`. We do this before performing any
        // other reschedule operation.
        // SAFETY: Interrupts are disabled, `queue_lock_` is not held, and the active
        // transaction holds the reserved scheduler token.
        unsafe {
            self.process_save_state_list();
        }

        let sched_ptr = self as *const Scheduler as *mut Scheduler;
        // SAFETY: `sched_ptr` is valid, interrupts are disabled, and `queue_lock_` is
        // not currently held.
        unsafe {
            cpp_scheduler_lock_queue(sched_ptr);
        }

        // Sample the current time after acquiring the queue lock to avoid large skews
        // between accounting based on now and trace events that should occur
        // approximately now.
        let mut now_ticks =
            SchedMonoTimeAndBootTicks { mono_time: SchedTime::from_raw(0), boot_ticks: 0 };
        // SAFETY: `now_ticks` is a valid mutable stack variable.
        unsafe {
            cpp_current_mono_time_and_boot_ticks(&mut now_ticks);
        }
        let now = now_ticks.mono_time;
        let boot_ticks = now_ticks.boot_ticks;

        // SAFETY: `self`'s `queue_lock_` and `current_thread`'s lock are held with the
        // reserved scheduler token.
        let result = unsafe { self.reschedule_common_helper(current_thread, now, boot_ticks) };

        let next_thread = result.next_thread;
        let (curr_is_idle, next_is_idle) = (result.curr_is_idle, result.next_is_idle);
        // SAFETY: `self` is the current CPU's `Scheduler` with interrupts disabled and
        // `queue_lock_` held.
        unsafe {
            self.reschedule_power_control(now, next_is_idle);
        }

        // Update the current processing rate only after any uses in the reschedule
        // path above to ensure the scale is applied consistently over the interval
        // between reschedules (i.e. not earlier than the requested update).
        //
        // Updating the processing rate also results in updating the target preemption
        // time below when the current thread is deadline scheduled.
        //
        // TODO(eieio): Shed load when total utilization is above the processing rate.
        // SAFETY: `self`'s `queue_lock_` is held.
        let (previous_processing_rate, processing_rate_updated) =
            unsafe { (self.processing_rate(), self.update_processing_rate(boot_ticks)) };

        // CRITICAL: Maintain Coherency of Idle and Busy States
        //
        // Apply the following rules:
        //
        //   2. Transition Update: If the CPU's idle state changes, upon re-acquiring
        //      the lock, stats MUST be updated again using FRESH time samples.
        //   3. Processing Rate Update: If the CPU's processing rate changes, upon
        //      re-acquiring the lock, stats MUST be updated again using FRESH time
        //      samples.
        //
        // See `update_cpu_stats_segment` for the full set of coherency rules.
        //
        // The queue lock has been released and re-acquired one or more times, each of
        // which may have been interposed by another CPU sampling the critical values
        // and forward projecting idle or normalized busy time. If there is transition
        // to/from idle or the processing rate changed while non-idle, advance the CPU
        // stats to cover the segment since the initial update.
        // SAFETY: `queue_lock_` is held.
        unsafe {
            self.set_idle(next_is_idle);
        }
        if next_is_idle != curr_is_idle || (!curr_is_idle && processing_rate_updated) {
            // SAFETY: `queue_lock_` is held.
            unsafe {
                self.update_cpu_stats_segment(
                    SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0),
                    Self::to_cpu_stats_segment_type(curr_is_idle),
                    previous_processing_rate,
                );
            }
        }

        // SAFETY: `queue_lock_` and `next_thread`'s lock are held, and the state
        // pointer and idle flag were latched from `next_thread` by the helper.
        let preemption_time_ns = unsafe {
            self.calculate_preemption_time(current_thread, &result, now, processing_rate_updated)
        };
        // SAFETY: Interrupts are disabled on `current_cpu`.
        unsafe {
            preempt_reset(current_cpu, now.raw_value(), preemption_time_ns.raw_value());
        }

        // Assert that there is no path beside running the idle thread can leave the
        // preemption timer unarmed. However, the preemption timer may or may not be
        // armed when running the idle thread.
        // SAFETY: `current_cpu` is the current CPU with interrupts disabled.
        debug_assert!(next_is_idle || unsafe { cpp_timer_queue_preempt_armed(current_cpu) });

        // Almost done, we need to handle the actual context switch (if any).
        if result.need_context_switch {
            // SAFETY: Both `current_thread` and `next_thread` are locked, and
            // `queue_lock_` is held prior to `cpp_scheduler_unlock_queue`.
            unsafe {
                let curr_rem_ts =
                    (*cpp_thread_get_scheduler_state(current_thread)).remaining_time_slice_ns();
                let next_rem_ts =
                    (*cpp_thread_get_scheduler_state(next_thread)).remaining_time_slice_ns();
                local_ktrace_instant!(
                    DETAILED,
                    "switch_threads",
                    "total threads" => self.runnable_task_count(),
                    "total weight" => self.weight_total().raw_value(),
                    "current thread time slice" => curr_rem_ts,
                    "next thread time slice" => next_rem_ts,
                );
                self.trace_thread_queue_event(
                    ktrace_rs::resolve_string!("tqe_astart"),
                    next_thread,
                );
                // Release queue lock before context switching.
                cpp_scheduler_unlock_queue(sched_ptr);

                // Finish the migration of the current thread, if pending.
                if result.current_is_migrating {
                    cpp_thread_call_migrate_fn_locked(current_thread, MIGRATE_STAGE_SAVE);
                    let target_cpu =
                        Self::find_active_scheduler_for_thread(current_thread, |thread, target| {
                            debug_assert!(!core::ptr::eq(target, self));
                            target.insert(now, thread, Placement::Migration);
                        });
                    super::mp::reschedule(cpu_num_to_mask(target_cpu), 0);
                }

                self.context_switch(
                    current_thread,
                    next_thread,
                    &saved_state,
                    trace.take(),
                    end_outer_trace,
                );
            }
        } else {
            // Restore the transaction state and drop the queue lock before unwinding.
            // SAFETY: `saved_state` is valid, `curr_lock` is held, and `queue_lock_` is
            // held on `sched_ptr`.
            unsafe {
                ChainLockTransaction::restore_state(saved_state);
                (*curr_lock).sync_token(&token);
                cpp_scheduler_unlock_queue(sched_ptr);
            }
        }
    }
}

/// Callback for `Scheduler::Block`.
///
/// # Safety
/// `current_thread` must be valid and be the current thread, interrupts must be
/// disabled, an active `ChainLockTransaction` must hold `current_thread`'s lock
/// (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`), and no
/// scheduler's `queue_lock_` may be held (`TA_EXCL(queue_lock_)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_block(current_thread: *mut Thread) {
    let mut trace = local_ktrace_begin_scope!(COMMON, "sched_block");
    debug_assert_eq!(current_thread, super::thread::current_get());
    // SAFETY: `current_thread` is valid and locked by the caller.
    unsafe { Thread::assert_canary_ptr(current_thread) };
    debug_assert!(
        unsafe { (*cpp_thread_get_scheduler_state(current_thread)).state } != ThreadState::Running
    );
    // SAFETY: Interrupts are disabled and `current_thread` is locked on the current
    // CPU with `queue_lock_` not held.
    unsafe {
        let sched = cpp_scheduler_get(crate::arch_rs::curr_cpu_num());
        (*sched).reschedule_common(
            current_thread,
            trace.as_mut().map(|s| s as &mut dyn ktrace_rs::ErasedScope),
        );
    }
}

/// Called when the preemption timer for a CPU has fired.
///
/// This function is logically private and should only be called by `timer.cc`.
///
/// # Safety
/// Must be called from the preemption timer interrupt handler on the current
/// CPU with interrupts disabled, while the current thread is valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_timer_tick(_now: SchedTime) {
    let _trace = local_ktrace_begin_scope!(COMMON, "sched_timer_tick");
    // Called from the local CPU's preemption timer interrupt handler.
    super::thread::preempt_set_pending();
}

/// Releases the lock held by the previous and current threads after a context
/// switch. This must be called by trampoline routines at some point before
/// jumping to the current thread's entry point.
///
/// # Safety
/// Must be called on thread trampoline entry with interrupts disabled and an
/// active `ChainLockTransaction` holding both `previous_thread`'s and
/// `current_thread`'s locks with the reserved scheduler token.
pub(crate) unsafe fn trampoline_lock_handoff() {
    // SAFETY: The thread trampoline runs with the transaction registered by the
    // reschedule that switched to this thread.
    let token = unsafe { ChainLockTransaction::assert_active() };

    // Construct the fake chainlock transaction we will be restoring. It should:
    //
    // 1) Not attempt to register itself with the per-cpu data structure. There
    //    should already be a registered CLT, and attempting to instantiate a new
    //    one using the typical constructor would trigger an ASSERT. This should
    //    look like a transaction which had been sitting on a stack, but swapped
    //    out using `save_state_and_use_reserved_token` at some point in the past.
    // 2) Report that there are currently two locks held (because there are).
    //    We have the previously running thread's lock, and the current (new)
    //    thread's lock held (obtained by `Scheduler::reschedule_common`).
    // 3) Report that the transaction has been finalized, as it would have been
    //    had we come in via `Scheduler::reschedule_common`.
    let tx = ChainLockTransaction::new_unregistered(clt_tag!("Trampoline"), 2, true);

    // Assert that the current thread's lock was "acquired" here. It was
    // actually acquired during the context switch from
    // `Scheduler::reschedule_common`, but we now need to release it before letting
    // the new thread run. The currently registered CLT should be using the
    // scheduler token that was used to acquire the new thread's lock during
    // `Scheduler::reschedule_common`.
    let current_thread = super::thread::current_get();
    // SAFETY: `current_thread` is valid and its lock is held by the active
    // `ChainLockTransaction` with the reserved scheduler token.
    let curr_lock = unsafe { thread_get_lock(current_thread) };
    // SAFETY: `curr_lock` is valid and held by the active transaction.
    unsafe {
        (*curr_lock).assert_acquired(&token);
    }

    // Now perform our lock handoff. This will release our previous
    // thread's lock, and restore the transaction we just instantiated as
    // the active transaction, and replace the current thread's lock's
    // token with the new non-scheduler token.
    let saved_state = tx.save_state_initial(&token);
    // SAFETY: `tx` stays at this stack address (it is not moved after
    // `save_state_initial`), interrupts are disabled, and both `previous_thread`'s
    // and `current_thread`'s locks are held with the reserved token.
    unsafe {
        Scheduler::lock_handoff_internal(&saved_state, current_thread);

        // Finally, we just need to release the current thread's lock and we
        // are finished.
        (*curr_lock).release(&token);
    }
}

/// Forwards C++ `Scheduler::TrampolineLockHandoff`.
///
/// # Safety
/// Must be called on thread trampoline entry with interrupts disabled and an
/// active `ChainLockTransaction` holding both `previous_thread`'s and
/// `current_thread`'s locks with the reserved scheduler token.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_lock_handoff() {
    // SAFETY: Forwarded from C++ `Scheduler::TrampolineLockHandoff`.
    unsafe { trampoline_lock_handoff() };
}

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

/// CPUs to reschedule after changes to active schedulers.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct RescheduleTargets {
    /// The CPU that the thread was scheduled on.
    pub target_cpu: CpuNum,
    /// Other CPUs that should be rescheduled. This is typically due to power
    /// level changes.
    pub cpus_to_reschedule: CpuMask,
}

// RescheduleTargets layout parity
zr::static_assert!(core::mem::size_of::<RescheduleTargets>() == 8);
zr::static_assert!(core::mem::align_of::<RescheduleTargets>() == 4);
zr::static_assert!(core::mem::offset_of!(RescheduleTargets, target_cpu) == 0);
zr::static_assert!(core::mem::offset_of!(RescheduleTargets, cpus_to_reschedule) == 4);

impl RescheduleTargets {
    pub fn mask(&self) -> CpuMask {
        cpu_num_to_mask(self.target_cpu) | self.cpus_to_reschedule
    }
}

/// Common logic for unblock API.
///
/// # Safety
/// The caller must ensure `thread` points to a valid initialized `Thread` whose
/// lock is held exclusively, and that interrupts are disabled.
pub unsafe fn unblock_common(
    thread: *mut Thread,
    now: SchedTime,
    unblock_type: UnblockType,
) -> RescheduleTargets {
    // SAFETY: `thread` is valid and its lock is held by the caller.
    unsafe { Thread::assert_canary_ptr(thread) };
    // SAFETY: `thread` is valid.
    let state_ptr = unsafe { cpp_thread_get_scheduler_state(thread) };

    let mut target_cpu;
    let mut cpus_to_reschedule = 0;
    loop {
        // TODO(rudymathu): This target_cpu should be stashed in the thread prior to
        // adding it to the save_state_list_, as that would allow us to bypass CPU
        // selection when processing the list so long as the CPU is still online.
        // SAFETY: `thread` is valid, its lock is held, and interrupts are disabled.
        target_cpu = unsafe { Scheduler::find_target_cpu(thread, unblock_type) };
        // SAFETY: `thread` is valid and its lock is held, guarding `last_cpu` and the
        // migration function and pending flags.
        let (last_cpu, needs_migration) = unsafe {
            let last_cpu = (*state_ptr).last_cpu;
            let needs_migration = last_cpu != INVALID_CPU
                && target_cpu != last_cpu
                && cpp_thread_has_migrate_fn(thread)
                && !cpp_thread_migrate_pending(thread);
            (last_cpu, needs_migration)
        };

        if needs_migration {
            target_cpu = last_cpu;
        }
        // SAFETY: `target_cpu` is a valid CPU number, interrupts are disabled, and
        // `target`'s `queue_lock_` is not already held.
        let (target, _target_queue_guard) = unsafe {
            let target = cpp_scheduler_get(target_cpu);
            (target, QueueLockGuard::acquire(target))
        };

        // SAFETY: `target` is valid and `_target_queue_guard` holds its `queue_lock_`,
        // while the caller holds `thread`'s lock exclusively.
        if unsafe { (*target).is_scheduler_active_locked() } {
            // SAFETY: `thread` is valid with its lock held exclusively, `target` is valid
            // with `queue_lock_` held by `_target_queue_guard`, and `target` is active.
            unsafe {
                sched_trace_wakeup(thread, target_cpu);
                cpp_thread_set_ready(thread);
                cpp_thread_update_runtime_stats(thread, (*state_ptr).state as i32);
                if needs_migration {
                    cpp_scheduler_save_state_list_push_front(target, thread);
                } else {
                    (*state_ptr).curr_cpu = target_cpu;
                    (*target).assert_in_scheduler(thread);
                    (*target).insert(now, thread, Placement::Insertion);

                    // If a power level change was requested during insertion, attempt to
                    // handle it immediately with the fast path and update any affected
                    // CPUs. If the fast path is not supported, the request will be left
                    // pending on the target CPU and flushed during its reschedule.
                    cpus_to_reschedule = (*target).send_pending_power_level_request();
                }
            }
            break;
        }

        COUNTER_FIND_TARGET_CPU_RETRIES.add(1);
    }

    RescheduleTargets { target_cpu, cpus_to_reschedule }
}

// The threshold for early termination when searching for a CPU to place a
// task. Queues with an estimated runtime below this value are sufficiently
// unloaded. In a performance-balanced system, this tunable value approximates
// the cost of intra-cluster migration due to cache misses, assuming a task
// has high cache affinity with the last CPU it ran on. This tunable may be
// increased to limit intra-cluster spill over.
const INTRA_CLUSTER_THRESHOLD: SchedDuration = sched_us(25);

// The threshold for cross-cluster work stealing. Queues with an estimated
// runtime below this value are not stolen from if the target and destination
// CPUs are in different logical clusters. In a performance-balanced system,
// this tunable value approximates the cost of cross-cluster migration due to
// cache misses, assuming a task has high cache affinity in its current
// cluster. This tunable may be increased to limit cross-cluster spill over.
const INTER_CLUSTER_THRESHOLD: SchedDuration = sched_ms(2);

// Utility type that tracks values related to the thread being placed when
// finding a target CPU for a waking or migrating thread.
struct ThreadDemand {
    woken_thread_utilization: SchedUtilization,
    current_thread_utilization: SchedUtilization,
    woken_thread_finish_time: SchedTime,
    woken_thread_last_cpu: CpuNum,
    current_thread_cpu: CpuNum,
    is_fair: bool,
}

impl ThreadDemand {
    /// Returns the thread demand parameters for the given thread and unblock type.
    /// When the unblock type is synchronous, values from the current thread are
    /// also considered.
    ///
    /// # Safety
    /// `woken_thread` must point to a valid `Thread` whose lock is held by the
    /// caller, and interrupts must be disabled.
    unsafe fn get(woken_thread: *const Thread, unblock_type: UnblockType) -> Self {
        // SAFETY: `woken_thread` is valid and its lock is held by the caller.
        let woken_state = unsafe { &*cpp_thread_get_scheduler_state(woken_thread.cast_mut()) };
        let woken_thread_finish_time = woken_state.finish_time;
        let woken_utilization = if woken_state.effective_profile.is_deadline() {
            woken_state.effective_profile.deadline().utilization
        } else {
            SchedUtilization::from_raw(0)
        };

        // Bandwidth demand is based only on the woken thread for the normal
        // unblock type.
        if unblock_type == UnblockType::Normal {
            let demand = Self {
                woken_thread_utilization: woken_utilization,
                current_thread_utilization: SchedUtilization::from_raw(0),
                woken_thread_finish_time,
                woken_thread_last_cpu: woken_state.last_cpu,
                current_thread_cpu: INVALID_CPU,
                is_fair: woken_state.effective_profile.is_fair(),
            };
            local_ktrace_instant!(
                QUEUE,
                "ThreadDemand::Get",
                "woken_thread_finish_time" => demand.woken_thread_finish_time,
                "woken_thread_last_cpu" => demand.woken_thread_last_cpu,
                "woken_thread_utilization" => demand.woken_thread_utilization,
                "current_thread_utilization" => demand.current_thread_utilization,
            );
            return demand;
        }

        // Bandwidth demand depends on both the woken and current threads for the
        // synchronous unblock type.
        let current_thread = super::thread::current_get();
        // SAFETY: The current CPU's scheduler is initialized.
        let scheduler = unsafe { cpp_scheduler_get(crate::arch_rs::curr_cpu_num()) };

        // Briefly acquire the queue lock for the current scheduler, where the
        // current thread is running, to allow read-only access to the relevant
        // scheduler state.
        // SAFETY: `scheduler` is valid, interrupts are disabled, and `queue_lock_`
        // is not already held. We project `current_thread`'s `SchedulerState` and
        // `EffectiveProfile` via raw pointers because `current_thread`'s lock is not
        // held here.
        let (current_utilization, current_thread_cpu, current_is_fair) = unsafe {
            let _guard = QueueLockGuard::acquire(scheduler);
            (*scheduler).assert_in_scheduler(current_thread);
            let current_state_ptr = cpp_thread_get_scheduler_state(current_thread);
            let current_ep_ptr = &raw const (*current_state_ptr).effective_profile;
            debug_assert_eq!((*current_state_ptr).curr_cpu, (*scheduler).this_cpu());

            let curr_discipline = EffectiveProfile::discipline_raw(current_ep_ptr);
            let util = if curr_discipline == SchedDiscipline::Deadline {
                EffectiveProfile::deadline_raw(current_ep_ptr).utilization
            } else {
                SchedUtilization::from_raw(0)
            };
            let cpu = (*current_state_ptr).curr_cpu;
            let is_fair = curr_discipline == SchedDiscipline::Fair;
            (util, cpu, is_fair)
        };

        let demand = Self {
            woken_thread_utilization: woken_utilization,
            current_thread_utilization: current_utilization,
            woken_thread_finish_time,
            woken_thread_last_cpu: woken_state.last_cpu,
            current_thread_cpu,
            is_fair: woken_state.effective_profile.is_fair() && current_is_fair,
        };
        local_ktrace_instant!(
            QUEUE,
            "ThreadDemand::Get",
            "woken_thread_finish_time" => demand.woken_thread_finish_time,
            "woken_thread_last_cpu" => demand.woken_thread_last_cpu,
            "woken_thread_utilization" => demand.woken_thread_utilization,
            "current_thread_utilization" => demand.current_thread_utilization,
        );
        demand
    }

    /// Returns the effective demand the thread would place on the given CPU,
    /// accounting for cached demand by the woken thread.
    fn get_effective_utilization(
        &self,
        cpu_num: CpuNum,
        power_level_control_enabled: bool,
    ) -> SchedUtilization {
        // If power level control is enabled, and thus demand caching is being
        // performed, the expiration of the woken thread's finish time is considered
        // to avoid double counting demand cached on the last CPU it ran on. Note
        // that the expiration of the finish time is compared with the current time
        // here to maximize the accuracy of the prediction of the cached state,
        // which could change in parallel with the overall CPU placement operation.
        let woken_thread_demand_likely_cached = power_level_control_enabled
            && cpu_num == self.woken_thread_last_cpu
            && self.woken_thread_finish_time
                > SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);

        // Return the additional demand that the woken thread would contribute to
        // the given CPU, accounting for demand caching. The demand of the current
        // thread, if contributing to the total demand, is already accounted for on
        // the CPU it is running on.
        let total_utilization = (if !woken_thread_demand_likely_cached {
            self.woken_thread_utilization
        } else {
            SchedUtilization::from_raw(0)
        }) + (if cpu_num != self.current_thread_cpu {
            self.current_thread_utilization
        } else {
            SchedUtilization::from_raw(0)
        });

        core::cmp::min(total_utilization, K_THREAD_UTILIZATION_MAX)
    }

    fn initial_cpu(&self) -> CpuNum {
        if self.current_thread_cpu != INVALID_CPU {
            self.current_thread_cpu
        } else {
            self.woken_thread_last_cpu
        }
    }
}

/// Latches a potential candidate placement for the thread and essential values
/// for making comparisons with other candidates. Dynamic values are exposed by
/// each `Scheduler` instance as relaxed atomics to avoid queue lock contention,
/// since these values are subject to change as soon as the queue lock is
/// dropped, and the comparisons only require approximate consistency. Latching
/// the values ensures that each comparison of a particular candidate with other
/// candidates uses the same values.
#[derive(Clone, Copy)]
struct CandidatePlacement {
    is_valid: bool,
    cpu: CpuNum,
    cluster: usize,
    queue_time_ns: SchedDuration,
    processing_rate: SchedProcessingRate,
    max_processing_rate: SchedProcessingRate,
    required_processing_rate: SchedProcessingRate,
    deadline_utilization: SchedUtilization,
    estimated_power_delta_nw: u64,
}

impl CandidatePlacement {
    fn empty() -> Self {
        Self {
            is_valid: false,
            cpu: INVALID_CPU,
            cluster: 0,
            queue_time_ns: SchedDuration::from_raw(0),
            processing_rate: SchedProcessingRate::from_raw(0),
            max_processing_rate: SchedProcessingRate::from_raw(0),
            required_processing_rate: SchedProcessingRate::from_raw(0),
            deadline_utilization: SchedUtilization::from_raw(0),
            estimated_power_delta_nw: 0,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn new(
        cpu: CpuNum,
        cluster: usize,
        queue_time_ns: SchedDuration,
        processing_rate: SchedProcessingRate,
        max_processing_rate: SchedProcessingRate,
        required_processing_rate: SchedProcessingRate,
        deadline_utilization: SchedUtilization,
        estimated_power_delta_nw: u64,
    ) -> Self {
        Self {
            is_valid: true,
            cpu,
            cluster,
            queue_time_ns,
            processing_rate,
            max_processing_rate,
            required_processing_rate,
            deadline_utilization,
            estimated_power_delta_nw,
        }
    }

    /// Latches key values from the given CPU, demand, and power domain set.
    ///
    /// # Safety
    /// `cpu_num` must be a valid CPU number, and `local_scheduler` must point to
    /// the current CPU's `Scheduler` with its `queue_lock_` held.
    unsafe fn evaluate(
        cpu_num: CpuNum,
        demand: &ThreadDemand,
        local_scheduler: *const Scheduler,
        power_level_control_enabled: bool,
    ) -> Self {
        // SAFETY: `cpu_num` is a valid CPU number.
        let scheduler = unsafe { cpp_scheduler_get(cpu_num) };
        let sched_state = unsafe { (*scheduler).state.get() };
        // SAFETY: `cluster` is immutable after init, and `exported_*` fields are
        // `RelaxedAtomicI64` safe for concurrent cross-CPU reads without `scheduler`'s lock.
        let (
            cpu_queue_time_ns,
            cpu_processing_rate,
            cpu_max_processing_rate,
            cpu_deadline_utilization,
            cluster,
        ) = unsafe {
            (
                SchedDuration::from_raw((*sched_state).exported_queue_time_ns.load()),
                SchedProcessingRate::from_raw((*sched_state).exported_processing_rate.load()),
                SchedProcessingRate::from_raw((*sched_state).exported_max_processing_rate.load()),
                SchedUtilization::from_raw((*sched_state).exported_deadline_utilization.load()),
                (*sched_state).cluster,
            )
        };

        // TODO(https://fxbug.dev/448195120): Factor in estimated utilization for
        // variable/fair bandwidth workloads, modulated by the variable/fair
        // bandwidth limit, if any. For now, just use constant/deadline bandwidth as
        // the primary input for the requirement.
        let current_required_processing_rate = cpu_deadline_utilization;

        // Start with the basic requirements common to all threads.
        let mut new_required_processing_rate = current_required_processing_rate;
        let mut estimated_power_delta_nw = 0;

        // Add the bandwidth demand for the thread.
        new_required_processing_rate +=
            demand.get_effective_utilization(cpu_num, power_level_control_enabled);

        // Estimated energy cost comparisons are only necessary in heterogeneous /
        // multi-domain systems. In strict SMP systems, where all CPUs have the same
        // power / performance characteristics and operating point changes affect
        // all CPUs simultaneously, only demand and processing rate requirements
        // need to be considered for task placement and admission control. However,
        // this condition may change to track and maintain deeper idle states in the
        // future.
        // TODO(eieio): Account for deeper idle states when making task placement
        // decisions.
        // SAFETY: `local_scheduler` is valid and its `queue_lock_` is held by the caller.
        let power_domain_count =
            unsafe { cpp_scheduler_get_power_domain_set_count(local_scheduler) };
        if power_domain_count > 1 {
            // Compute the power delta if there is a change in the required processing
            // rate. The addition of deadline / constant bandwidth demand will always
            // increase the required processing rate, but fair / variable bandwidth
            // demand may or may not increase the required processing rate.
            let required_processing_rate_delta =
                new_required_processing_rate - current_required_processing_rate;
            if required_processing_rate_delta != SchedProcessingRate::from_raw(0) {
                // The current power cost for each individual processor in the power
                // domain, including this candidate placement.
                // SAFETY: `local_scheduler` is valid with `queue_lock_` held, and `cpu_num` is valid.
                let current_power_cost = unsafe {
                    cpp_scheduler_lookup_power_cost(local_scheduler, cpu_num, cpu_processing_rate)
                };

                // If the task fits on the candidate without changing the processing rate,
                // estimate the power delta based only on the new demand. Otherwise, if the
                // candidate will become oversubscribed at the current rate, a higher
                // compatible rate must be selected, if one exists, and the estimated power
                // delta must include the effect on the entire domain.
                if new_required_processing_rate <= cpu_processing_rate {
                    let power_cost = FastFixed::<i64, 0>::from_raw(current_power_cost as i64);
                    estimated_power_delta_nw =
                        FastFixed::<i64, 0>::from(required_processing_rate_delta * power_cost)
                            .raw_value() as u64;
                } else {
                    // SAFETY: `local_scheduler` is valid with `queue_lock_` held, and `cpu_num` is valid.
                    let domain_utilization = unsafe {
                        cpp_scheduler_lookup_total_normalized_utilization(local_scheduler, cpu_num)
                    };
                    // SAFETY: `local_scheduler` is valid with `queue_lock_` held, and `cpu_num` is valid.
                    let new_power_cost = unsafe {
                        cpp_scheduler_lookup_power_cost(
                            local_scheduler,
                            cpu_num,
                            new_required_processing_rate,
                        )
                    };

                    let new_power_cost = FastFixed::<i64, 0>::from_raw(new_power_cost as i64);
                    let current_power_cost =
                        FastFixed::<i64, 0>::from_raw(current_power_cost as i64);
                    estimated_power_delta_nw = FastFixed::<i64, 0>::from(
                        (domain_utilization + required_processing_rate_delta) * new_power_cost
                            - domain_utilization * current_power_cost,
                    )
                    .raw_value() as u64;
                }
            }
        }

        local_ktrace_instant!(
            QUEUE,
            "evaluate",
            "cpu" => cpu_num,
            "queue time" => cpu_queue_time_ns,
            "actual rate" => cpu_processing_rate,
            "max rate" => cpu_max_processing_rate,
            "current required rate" => current_required_processing_rate,
            "new required rate" => new_required_processing_rate,
            "estimated power delta nw" => estimated_power_delta_nw,
        );

        Self::new(
            cpu_num,
            cluster,
            cpu_queue_time_ns,
            cpu_processing_rate,
            cpu_max_processing_rate,
            new_required_processing_rate,
            cpu_deadline_utilization,
            estimated_power_delta_nw,
        )
    }

    fn is_admissible(&self) -> bool {
        self.required_processing_rate <= self.max_processing_rate
    }

    fn capacity_shortfall(&self) -> SchedProcessingRate {
        if self.is_admissible() {
            SchedProcessingRate::from_raw(0)
        } else {
            self.required_processing_rate - self.max_processing_rate
        }
    }

    /// Returns true if the load balancing objectives and constraints are
    /// sufficiently met that candidate selection can stop on this candidate.
    fn is_sufficient(&self, demand: &ThreadDemand) -> bool {
        // If the estimated power is non-zero (i.e. there is an active energy model),
        // don't stop searching early, even if the other criteria are met. This
        // results in the search converging on the lowest power candidate that also
        // meets the admission control criteria.
        // TODO(https://fxbug.dev/448196664): Consider adding a configurable
        // threshold to terminate the search if the estimated power cost is
        // acceptably low.
        let migration_unnecessary_condition =
            self.estimated_power_delta_nw == 0 && self.queue_time_ns <= INTRA_CLUSTER_THRESHOLD;

        // Bypass the migration condition for synchronous wakeups. For normal
        // wakeups, the current thread's CPU will be INVALID_CPU to indicate that it
        // should not affect the placement decision.
        let synchronous_condition = self.cpu == demand.current_thread_cpu;

        // If this is a synchronous wake up or migration is unnecessary, then stop
        // searching for a better placement if the demand is admissible.
        (synchronous_condition || migration_unnecessary_condition) && self.is_admissible()
    }

    /// Returns true if this candidate is a better alternative than the current target.
    fn is_better_than(&self, current_target: &Self, demand: &ThreadDemand) -> bool {
        local_ktrace_instant!(
            QUEUE,
            "compare",
            "Current queue time" => current_target.queue_time_ns,
            "Current cluster" => current_target.cluster as u64,
            "Current deadline utilization" => current_target.deadline_utilization,
            "Candidate queue time" => self.queue_time_ns,
            "Candidate cluster" => self.cluster as u64,
            "Candidate deadline utilization" => self.deadline_utilization,
        );

        if demand.is_fair {
            // CPUs in the same logical cluster are considered equivalent in terms of
            // cache affinity. Choose the least loaded among the members of a cluster.
            if self.cluster == current_target.cluster {
                return (self.queue_time_ns, self.deadline_utilization)
                    < (current_target.queue_time_ns, current_target.deadline_utilization);
            }

            // Only consider crossing cluster boundaries if the current candidate is
            // above the threshold.
            return current_target.queue_time_ns > INTER_CLUSTER_THRESHOLD
                && self.queue_time_ns < current_target.queue_time_ns;
        }

        let candidate_criteria = (
            self.capacity_shortfall(),
            self.estimated_power_delta_nw,
            self.deadline_utilization,
            self.queue_time_ns,
        );
        let current_criteria = (
            current_target.capacity_shortfall(),
            current_target.estimated_power_delta_nw,
            current_target.deadline_utilization,
            current_target.queue_time_ns,
        );

        candidate_criteria < current_criteria
    }
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
            Thread::tid_ptr(current_thread),
            Thread::tid_ptr(next_thread),
            "outgoing_weight" => if Thread::is_idle_ptr(current_thread) {
                IDLE_WEIGHT
            } else {
                (*cpp_thread_get_scheduler_state(current_thread))
                    .get_weight_or_packed_deadline_params()
            },
            "incoming_weight" => if Thread::is_idle_ptr(next_thread) {
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
        if !Thread::is_idle_ptr(current_thread) {
            ktrace_rs::thread_wakeup!(
                "kernel:sched",
                target_cpu,
                Thread::tid_ptr(thread),
                "weight" => if Thread::is_idle_ptr(thread) {
                    IDLE_WEIGHT
                } else {
                    (*cpp_thread_get_scheduler_state(thread))
                        .get_weight_or_packed_deadline_params()
                },
                "waker" => ktrace_rs::Koid(Thread::tid_ptr(current_thread)),
            );
        } else {
            ktrace_rs::thread_wakeup!(
                "kernel:sched",
                target_cpu,
                Thread::tid_ptr(thread),
                "weight" => if Thread::is_idle_ptr(thread) {
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

    /// # Safety
    /// The caller must ensure interrupts are disabled or preemption state is valid,
    /// and if a `ChainLockTransaction` is active, it holds either 0 locks or only
    /// `current_thread`'s lock.
    pub unsafe fn reschedule_mask(cpus_to_reschedule_mask: CpuMask) {
        // Does the local CPU need to be preempted?
        let local_mask = cpu_num_to_mask(crate::arch_rs::curr_cpu_num());
        let local_cpu = cpus_to_reschedule_mask & local_mask;

        let current_thread = super::thread::current_get();
        // SAFETY: `current_thread` is the current CPU's active thread.
        let ps = unsafe { &*Thread::preemption_state_ptr(current_thread) };

        // First deal with the remote CPUs.
        //
        // Can we reschedule them?
        if ps.eager_resched_disable_count() > 0 {
            // EagerReschedDisabled implies that local preemption is also disabled.
            debug_assert!(!super::thread::preempt_is_enabled());
            // Nope, save them for later.
            ps.preempts_pending_add(cpus_to_reschedule_mask);
            if local_cpu != 0 {
                ps.evaluate_timeslice_extension();
            }
            return;
        }

        // `mp::reschedule` will remove the local CPU from the mask for us.
        super::mp::reschedule(cpus_to_reschedule_mask, 0);

        // Now deal with the local CPU.
        if local_cpu == 0 {
            // Nothing to do.
            return;
        }
        let preempt_enabled = ps.evaluate_timeslice_extension();

        // Can we do it here and now?
        if !preempt_enabled {
            // Nope, can't do it now. Make a note for later.
            ps.preempts_pending_add(local_cpu);
            return;
        }

        // From a chain-lock requirement perspective, there are a few different
        // cases to consider.
        //
        // 1) We currently have no transaction in progress and hold no locks.
        //    In this case, we can simply call `preempt`, which will start a
        //    transaction, lock our current thread, and take care of preemption.
        // 2) We do currently have a transaction in progress, but we hold no
        //    locks. This can happen in the cases such as `unblock(thread)` or
        //    `unblock_list(list)`, where all of the unblocking thread's locks were
        //    dropped after adding them to the proper scheduler queue. We need
        //    to re-use our existing transaction by first restarting it, then
        //    obtaining our current thread's lock, and finally calling
        //    `preempt_locked`.
        // 3) We do currently have a transaction in progress, and we are holding
        //    exactly one lock which is the current thread's lock. In this case,
        //    we simply call `preempt_locked` directly.
        //
        // TODO(johngro): Determine if #1 and #3 are actual possibilities by the
        // time that we hit this stage. #2 may be the only legit case, but I'm
        // not quite sure yet.
        if ChainLockTransaction::active().is_none() {
            // SAFETY: No `ChainLockTransaction` is active on the current CPU.
            unsafe { preempt(PreemptType::Reschedule) };
            return;
        }

        // SAFETY: `ChainLockTransaction::active().is_some()` was verified above.
        let token = unsafe { ChainLockTransaction::mark_active() };
        let active_clt = ChainLockTransaction::active_ref(&token);
        // SAFETY: `current_thread` is the current CPU's active thread.
        let lock = unsafe { &*thread_get_lock(current_thread) };
        if !lock.is_held(&token) {
            active_clt.restart(clt_tag!("Scheduler::RescheduleMask"), &token);
            active_clt.assert_num_locks_held(0);
            let _guard = ChainLockGuard::new(lock, &token);
            ChainLockTransaction::finalize(&token);
            // SAFETY: `current_thread` is valid and `_guard` holds its lock as the
            // sole lock in the finalized `ChainLockTransaction`.
            unsafe { preempt_locked(current_thread, PreemptType::Reschedule) };
            return;
        }

        active_clt.assert_num_locks_held(1);
        lock.assert_acquired(&token);
        // SAFETY: `current_thread` is valid and its lock is the sole lock held in
        // the active `ChainLockTransaction`.
        unsafe { preempt_locked(current_thread, PreemptType::Reschedule) };
    }

    /// A helper function used by `find_active_scheduler_for_thread`. Returns a CPU to
    /// run the given thread on, but does not lock that scheduler, meaning that the
    /// selected scheduler can become deactivated before the caller can lock it.
    ///
    /// Typically, users will want to use `find_active_scheduler_for_thread` instead,
    /// which will supply a locked scheduler which is guaranteed to be active.
    ///
    /// # Safety
    /// The caller must ensure `thread` points to a valid initialized `Thread` whose
    /// lock is held at least shared, that interrupts are disabled, and that no
    /// scheduler's `queue_lock_` is held.
    pub unsafe fn find_target_cpu(thread: *mut Thread, unblock_type: UnblockType) -> CpuNum {
        let trace = local_ktrace_begin_scope!(COMMON, "find_target");

        // Determine the set of CPUs the thread is allowed to run on.
        let current_cpu = crate::arch_rs::curr_cpu_num();
        let active_mask = peek_active_mask();
        // SAFETY: `thread` is valid and its lock is held by the caller.
        let (available_mask, hard_affinity, soft_affinity) = unsafe {
            let thread_state = &*cpp_thread_get_scheduler_state(thread);
            (
                thread_state.get_effective_cpu_mask(active_mask),
                thread_state.hard_affinity,
                thread_state.soft_affinity,
            )
        };

        if cfg!(debug_assertions) && available_mask == 0 {
            // SAFETY: `thread` is valid.
            unsafe { panic_no_available_cpu(thread, hard_affinity, soft_affinity, active_mask) };
        }

        local_ktrace_instant!(
            DETAILED,
            "target_mask",
            "online" => super::mp::get_online_mask(),
            "active" => active_mask,
        );

        // Evaluate the bandwidth demand of the woken thread, which will include the
        // bandwidth of the current thread for synchronous wakeups.
        // SAFETY: `thread` is valid and its lock is held by the caller.
        let demand = unsafe { ThreadDemand::get(thread, unblock_type) };

        // Find a suitable target CPU, usually starting at the last CPU the task ran
        // on, if any. Candidates are considered in order of best to worst potential
        // cache affinity.
        let last_cpu = demand.initial_cpu();
        let starting_cpu = if last_cpu != INVALID_CPU { last_cpu } else { current_cpu };

        // Loop over the search set for CPU the task last ran on to find a suitable
        // target.
        let mut target_queue = CandidatePlacement::empty();

        // Acquire the queue lock of the current CPU to ensure that the power domain
        // set for the current CPU doesn't change until the search is complete.
        // TODO(https://fxbug.dev/448191466): Move the energy model into the ZBI and
        // allocate power domain data structures only during early init, removing
        // the need for locking or snapshotting.
        // SAFETY: `current_cpu` is a valid CPU number.
        let scheduler = unsafe { cpp_scheduler_get(current_cpu) };
        // SAFETY: `scheduler` is valid, interrupts are disabled, and its `queue_lock_`
        // is not already held.
        let guard = unsafe { QueueLockGuard::acquire(scheduler) };

        // SAFETY: `scheduler` is valid and `guard` holds its `queue_lock_`.
        let power_level_control_enabled = unsafe { (*scheduler).power_level_control_is_enabled() };

        // SAFETY: `starting_cpu` is a valid CPU number, and the CPU search set is
        // initialized during boot topology setup.
        let (count, entries_ptr) = unsafe {
            (
                cpp_get_cpu_search_set_count(starting_cpu),
                cpp_get_cpu_search_set_entries(starting_cpu),
            )
        };
        let entries = if count == 0 || entries_ptr.is_null() {
            &[][..]
        } else {
            // SAFETY: `entries_ptr` points to `count` valid `CpuSearchSetEntry` elements.
            unsafe { core::slice::from_raw_parts(entries_ptr, count) }
        };

        for entry in entries {
            let candidate_cpu = entry.cpu;
            let candidate_available = (available_mask & cpu_num_to_mask(candidate_cpu)) != 0;

            if candidate_available {
                // SAFETY: `candidate_cpu` is valid, and `scheduler`'s `queue_lock_` is held by `_guard`.
                let candidate_queue = unsafe {
                    CandidatePlacement::evaluate(
                        candidate_cpu,
                        &demand,
                        scheduler,
                        power_level_control_enabled,
                    )
                };

                if !target_queue.is_valid || candidate_queue.is_better_than(&target_queue, &demand)
                {
                    target_queue = candidate_queue;

                    // Stop searching when the load balancing criteria have been sufficiently met.
                    if target_queue.is_sufficient(&demand) {
                        break;
                    }
                }
            }
        }

        drop(guard);

        debug_assert!(target_queue.cpu != INVALID_CPU);

        let is_local = if unblock_type == UnblockType::Normal {
            last_cpu == target_queue.cpu
        } else {
            demand.current_thread_cpu == target_queue.cpu
        };
        counter_wakeup_add(unblock_type == UnblockType::Synchronous, is_local);

        ktrace_rs::end_scope!(
            trace,
            "last_cpu" => last_cpu,
            "target_cpu" => target_queue.cpu,
        );

        target_queue.cpu
    }

    /// Attempts to steal work from other busy CPUs and move it to the local run
    /// queues. Returns a pointer to the stolen thread that is now associated with
    /// the local `Scheduler` instance, or a null thread pointer if no work was
    /// stolen.
    ///
    /// # Safety
    /// The caller must ensure `self` is the current CPU's `Scheduler`, that
    /// interrupts are disabled, and that `self`'s `queue_lock_` is NOT held.
    pub unsafe fn steal_work(
        &self,
        now: SchedTime,
        processing_rate: SchedProcessingRate,
    ) -> DequeueResult {
        let _trace = local_ktrace_begin_scope!(DETAILED, "StealWork");
        // SAFETY: `this_cpu` is initialized during early boot.
        let current_cpu = unsafe { self.this_cpu() };
        let current_cpu_mask = cpu_num_to_mask(current_cpu);
        let active_cpu_mask = peek_active_mask();

        // SAFETY: `current_cpu` is valid and the CPU search set is initialized during boot.
        let (count, entries_ptr) = unsafe {
            (cpp_get_cpu_search_set_count(current_cpu), cpp_get_cpu_search_set_entries(current_cpu))
        };
        let entries = if count == 0 || entries_ptr.is_null() {
            &[][..]
        } else {
            // SAFETY: `entries_ptr` points to `count` valid `CpuSearchSetEntry` elements.
            unsafe { core::slice::from_raw_parts(entries_ptr, count) }
        };

        for entry in entries {
            if entry.cpu != current_cpu && (active_cpu_mask & cpu_num_to_mask(entry.cpu)) != 0 {
                // SAFETY: `entry.cpu` is a valid CPU number.
                let scheduler = unsafe { cpp_scheduler_get(entry.cpu) };

                // Only steal across clusters if the target is above the load threshold.
                // SAFETY: `cluster` is immutable after init, and `exported_queue_time_ns` is an
                // atomic shadow value readable without `queue_lock_`.
                let (cluster, target_queue_time) = unsafe {
                    (
                        (*self.state.get()).cluster,
                        SchedDuration::from_raw(
                            (*(*scheduler).state.get()).exported_queue_time_ns.load(),
                        ),
                    )
                };
                if cluster != entry.cluster && target_queue_time <= INTER_CLUSTER_THRESHOLD {
                    continue;
                }

                // SAFETY: `scheduler` is valid, interrupts are disabled, and no `queue_lock_` is held.
                let _guard = unsafe { QueueLockGuard::acquire(scheduler) };

                // Check if the scheduler is still active. If it's not, ignore it because
                // `migrate_unpinned_threads` will move the threads off of it.
                // SAFETY: `scheduler` is valid and `_guard` holds its `queue_lock_`.
                if !unsafe { (*scheduler).is_scheduler_active_locked() } {
                    continue;
                }

                // TODO(b/430139320): Disabled until root cause of x64 pessimization is
                // understood and addressed.
                //
                // Only attempt to steal from CPUs that have more than one task, avoiding
                // unnecessary overhead when a task was just added to an idle CPU but the
                // CPU has not started running the new task (i.e. when this CPU wins the
                // race to acquire the target CPU's queue lock after the singular task is
                // queued).
                //
                // if unsafe { (*scheduler).runnable_task_count() } <= 1 {
                //     continue;
                // }

                // Returns true if the given thread can run on this CPU. We only need R/O
                // access to the thread's scheduler state, which we have because we are
                // holding the queue lock for a thread which belongs to that queue.
                let check_affinity = |thread: *mut Thread| -> bool {
                    // SAFETY: `thread` is enqueued in `scheduler`, whose `queue_lock_` is held,
                    // so its affinity masks are stable; the raw accessor avoids forming a
                    // reference because `thread`'s lock is not held.
                    let effective_cpu_mask = unsafe {
                        SchedulerState::get_effective_cpu_mask_raw(
                            cpp_thread_get_scheduler_state(thread),
                            active_cpu_mask,
                        )
                    };
                    (current_cpu_mask & effective_cpu_mask) != 0
                };

                // Common routine for stealing from a run queue.
                let steal_from_queue = |run_queue: *mut RunQueue,
                                        is_fair: bool,
                                        predicate: &dyn Fn(*mut Thread) -> bool|
                 -> Option<DequeueResult> {
                    let _trace_steal = local_ktrace_begin_scope!(COMMON, "sched_steal");

                    // Convert the monotonic eligible time to variable time and potentially
                    // snap the virtual timeline forward when stealing from the fair queue.
                    let mut eligible_time = now;
                    if is_fair {
                        // SAFETY: `scheduler`'s `queue_lock_` is held by `_guard`, which also
                        // synchronizes access to `run_queue`.
                        let front_opt = unsafe {
                            eligible_time = (*scheduler).monotonic_to_variable(eligible_time);
                            (*run_queue).front()
                        };
                        if let Some(earliest_thread) = front_opt {
                            // SAFETY: `earliest_thread` is enqueued in `scheduler`, whose
                            // `queue_lock_` is held. Project `start_time` via raw pointer.
                            let earliest_start = unsafe {
                                (*cpp_thread_get_scheduler_state(
                                    core::ptr::from_ref(earliest_thread).cast_mut(),
                                ))
                                .start_time
                            };
                            if eligible_time < earliest_start {
                                COUNTER_FAIR_TIMELINE_SNAP_COUNT.add(1);
                                let snap_time = earliest_start - eligible_time;
                                COUNTER_FAIR_TIMELINE_SNAP_TIME
                                    .add(snap_time.into_fixed().raw_value());

                                // SAFETY: `scheduler`'s `queue_lock_` is held by `_guard`.
                                unsafe {
                                    (*(*scheduler).state.get())
                                        .fair_affine_transform
                                        .snap(now, earliest_start);
                                }
                                eligible_time = earliest_start;
                            }
                        }
                    }

                    // SAFETY: `run_queue` belongs to `scheduler`, whose `queue_lock_` is held.
                    let thread = unsafe {
                        find_earliest_eligible_thread(&*run_queue, eligible_time, predicate)
                    };
                    if thread.is_null() {
                        return None;
                    }

                    // Remove the thread from the source run queue and record that it was
                    // stolen by this CPU.
                    // SAFETY: `thread` is enqueued in `run_queue` and `scheduler`'s `queue_lock_`
                    // is held by `_guard`.
                    unsafe {
                        debug_assert!(check_affinity(thread));
                        debug_assert!(!cpp_thread_has_migrate_fn(thread));
                        debug_assert_eq!(
                            (*cpp_thread_get_queue_state(thread)).disposition(),
                            QueueDisposition::Enqueued
                        );
                        (*run_queue).erase_raw(thread);
                        (*scheduler).remove(now, thread, current_cpu);
                        debug_assert_eq!(
                            (*cpp_thread_get_queue_state(thread)).disposition(),
                            QueueDisposition::Stolen
                        );
                        (*scheduler).trace_thread_queue_event(
                            ktrace_rs::resolve_string!("tqe_deque_steal_work"),
                            thread,
                        );
                    }

                    if is_fair {
                        // SAFETY: `thread` was just stolen under `scheduler`'s `queue_lock_`.
                        // Project `start_time` and `finish_time` via raw pointer.
                        let (start_time, finish_time) = unsafe {
                            let state_ptr = cpp_thread_get_scheduler_state(thread);
                            ((*state_ptr).start_time, (*state_ptr).finish_time)
                        };
                        // SAFETY: `scheduler`'s `queue_lock_` is held by `_guard`.
                        let (mono_start, mono_finish) = unsafe {
                            (
                                (*scheduler).variable_to_monotonic(start_time),
                                (*scheduler).variable_to_monotonic(finish_time),
                            )
                        };
                        return Some(DequeueResult {
                            thread,
                            mono_start_time_ns: mono_start,
                            mono_finish_time_ns: mono_finish,
                        });
                    }

                    Some(DequeueResult::from_thread(thread))
                };

                // Returns true if the given thread in the run queue meets the criteria to
                // run on this CPU. Don't attempt to steal any threads which are
                // currently in the process of being scheduled.
                let deadline_predicate = |thread: *mut Thread| -> bool {
                    // SAFETY: `thread` is enqueued in `scheduler`, whose `queue_lock_` is held.
                    // We use `EffectiveProfile::deadline_raw` to avoid forming `&EffectiveProfile`
                    // without `thread`'s lock.
                    let is_scheduleable = unsafe {
                        let state_ptr = cpp_thread_get_scheduler_state(thread);
                        let ep_ptr = &raw const (*state_ptr).effective_profile;
                        EffectiveProfile::deadline_raw(ep_ptr).utilization <= processing_rate
                    };
                    // SAFETY: `thread` is valid and `scheduler`'s `queue_lock_` is held.
                    check_affinity(thread)
                        && is_scheduleable
                        && !unsafe { cpp_thread_has_migrate_fn(thread) }
                };

                // Returns true if the given thread in the run queue meets the criteria to
                // run on this CPU.
                let fair_predicate = |thread: *mut Thread| -> bool {
                    // SAFETY: `thread` is valid and `scheduler`'s `queue_lock_` is held.
                    check_affinity(thread) && !unsafe { cpp_thread_has_migrate_fn(thread) }
                };

                // Attempt to find a critical deadline thread that can run on this CPU.
                // SAFETY: `scheduler`'s `queue_lock_` is held by `_guard`.
                if let Some(res) = steal_from_queue(
                    unsafe { (*scheduler).critical_deadline_queue_mut() },
                    false,
                    &deadline_predicate,
                ) {
                    return res;
                }

                // Attempt to find a deadline thread that can run on this CPU.
                // SAFETY: `scheduler`'s `queue_lock_` is held by `_guard`.
                if let Some(res) = steal_from_queue(
                    unsafe { (*scheduler).deadline_queue_mut() },
                    false,
                    &deadline_predicate,
                ) {
                    return res;
                }

                // Attempt to find a fair thread that can run on this CPU.
                // SAFETY: `scheduler`'s `queue_lock_` is held by `_guard`.
                if let Some(res) = steal_from_queue(
                    unsafe { (*scheduler).fair_run_queue_mut() },
                    true,
                    &fair_predicate,
                ) {
                    return res;
                }
            }
        }

        DequeueResult::empty()
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

    /// Finds an appropriate CPU for a thread to run on, then locks that scheduler,
    /// confirms that it is still active, and invokes the user-supplied callback with
    /// the scheduler's `queue_lock` held.
    ///
    /// This method may need to search for a target CPU for the thread more than
    /// once if the initially chosen scheduler becomes deactivated after it was
    /// selected, but before it was locked.
    ///
    /// # Safety
    /// The caller must ensure `thread` points to a valid initialized `Thread` whose
    /// lock is held at least shared, that interrupts are disabled, and that no
    /// scheduler's `queue_lock_` is held.
    pub unsafe fn find_active_scheduler_for_thread<F>(thread: *mut Thread, mut action: F) -> CpuNum
    where
        F: FnMut(*mut Thread, &Scheduler),
    {
        loop {
            // SAFETY: `thread` is valid and locked per caller contract. `target_ptr` points to
            // a valid per-CPU `Scheduler` whose `queue_lock_` is held by `_target_queue_guard`
            // while `target` is borrowed and passed to `action`.
            unsafe {
                let target_cpu = Self::find_target_cpu(thread, UnblockType::Normal);
                let target_ptr = cpp_scheduler_get(target_cpu);
                let _target_queue_guard = QueueLockGuard::acquire(target_ptr);
                let target = &*target_ptr;
                if target.is_scheduler_active_locked() {
                    action(thread, target);
                    return target_cpu;
                }
            }
            COUNTER_FIND_TARGET_CPU_RETRIES.add(1);
        }
    }

    /// Runs the migration functions of threads in the `save_state_list`, placing
    /// the threads in their target `RunQueue`s when done.
    ///
    /// Note that this method is called while holding the special "sched token"
    /// used during reschedule operations. As a result, it is an absolute
    /// requirement that any threads we lock in this method are not discoverable
    /// and lockable from another scheduler that is also holding the "sched token."
    /// Failure to meet this requirement will result in deadlock.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// - Interrupts are disabled and `self`'s `queue_lock_` is not held (`TA_EXCL(queue_lock_)`).
    /// - An active `ChainLockTransaction` is registered on the current CPU using
    ///   the reserved scheduler token (`TA_REQ(chainlock_transaction_token)`).
    #[inline]
    pub unsafe fn process_save_state_list(&self) {
        debug_assert!(crate::arch_rs::ints_disabled());

        // Move the `save_state_list` to a local, stack allocated list. This allows us to relinquish
        // the `queue_lock` for the rest of the method. The lock, emptiness check, and move are
        // combined into a single FFI call so that the common empty case costs one call.
        let mut local_save_state_list = core::mem::MaybeUninit::<SaveStateList>::uninit();
        let local_save_state_list_ptr = local_save_state_list.as_mut_ptr();

        // SAFETY: `self` is valid, interrupts are disabled, and `queue_lock_` is not
        // held. `cpp_scheduler_take_save_state_list` acquires `queue_lock_` and, if it
        // returns true, has initialized `local_save_state_list_ptr`.
        let taken = unsafe {
            cpp_scheduler_take_save_state_list(
                self as *const Scheduler as *mut Scheduler,
                local_save_state_list_ptr,
            )
        };
        if !taken {
            return;
        }

        // SAFETY: `local_save_state_list_ptr` was initialized by
        // `cpp_scheduler_take_save_state_list`, and the caller's requirements are
        // forwarded.
        unsafe { Self::process_save_state_list_slow(local_save_state_list_ptr) };
    }

    /// Slow path of `process_save_state_list`: migrates every thread in the
    /// (non-empty) list `local_save_state_list_ptr` and destroys the list.
    ///
    /// Kept out of line and cold so that the migration loop's stack usage and
    /// spills stay off the hot reschedule path.
    ///
    /// # Safety
    /// Same as `process_save_state_list`, and `local_save_state_list_ptr` must
    /// point to an initialized `Thread::SaveStateList`.
    #[cold]
    #[inline(never)]
    unsafe fn process_save_state_list_slow(local_save_state_list_ptr: *mut SaveStateList) {
        // SAFETY: The caller guarantees an active `ChainLockTransaction` on the
        // current CPU.
        let token = unsafe { ChainLockTransaction::assert_active() };
        ChainLockTransaction::active_ref(&token)
            .restart(clt_tag!("Scheduler::ProcessSaveStateList (restart)"), &token);

        let mut cpus_to_reschedule_mask: CpuMask = 0;
        loop {
            // SAFETY: `local_save_state_list_ptr` is initialized per the caller's
            // contract.
            let to_migrate = unsafe { cpp_save_state_list_pop(local_save_state_list_ptr) };
            if to_migrate.is_null() {
                break;
            }

            // Acquire the thread's lock without backing off using an explicit retry
            // loop because the current thread's lock is held and ChainLock::Acquire
            // asserts that no chain locks are currently held to avoid misuse in the
            // general case. We also currently hold the special "sched token", meaning
            // that we will win arbitration against any other chainlock transaction.
            //
            // This is safe to do here because we can guarantee that no other scheduler
            // holds or is attempting to acquire the locks of any of the threads in this
            // `save_state_list`. The only context in which schedulers attempt to acquire
            // the locks of threads in other schedulers is during the `steal_work`
            // routine. However, that routine only looks for threads in the `RunQueue`s
            // (not `save_state_list`s) and also does not steal threads that have
            // migration functions. Therefore, it is impossible for another scheduler to
            // be fighting over this thread's lock.
            // SAFETY: `to_migrate` is a valid `Thread` popped from `save_state_list`, so
            // its lock is valid; see above for why acquiring it without backoff while
            // holding the current thread's lock cannot deadlock.
            unsafe {
                let to_migrate_lock = thread_get_lock(to_migrate);
                while !(*to_migrate_lock).try_acquire_with_options(
                    AllowFinalized::No,
                    RecordBackoff::No,
                    &token,
                ) {
                    Self::arch_yield();
                }
            }

            // We now have the thread's lock, so run the migrate function.
            // SAFETY: `to_migrate` is valid and its lock is held.
            unsafe { cpp_thread_call_migrate_fn_locked(to_migrate, MIGRATE_STAGE_SAVE) };

            // TODO(rudymathu): Threads placed in the `save_state_list` via an `unblock`
            // operation already performed a `find_target_cpu` call. In a future
            // optimization, we should be able to reuse that value here and avoid
            // another CPU search, provided that the CPU is still online and load
            // balancing constraints still hold.
            // SAFETY: `to_migrate`'s lock is held with interrupts disabled, and inside
            // the closure `target`'s `queue_lock_` is held.
            let target_cpu = unsafe {
                Self::find_active_scheduler_for_thread(to_migrate, |thread, target| {
                    let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
                    target.insert(now, thread, Placement::Migration);
                })
            };

            cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
            // SAFETY: `to_migrate` is valid and its lock was acquired above in this loop
            // iteration.
            unsafe { (*thread_get_lock(to_migrate)).release(&token) };
        }
        ChainLockTransaction::finalize(&token);

        // Issue reschedule IPIs to CPUs with migrated threads.
        if cpus_to_reschedule_mask != 0 {
            super::mp::reschedule(cpus_to_reschedule_mask, 0);
        }

        // SAFETY: The list is initialized and now empty; this ends its lifetime.
        unsafe { cpp_save_state_list_destroy(local_save_state_list_ptr) };
    }

    /// Migrates any threads that can run on other CPUs off of the current CPU.
    ///
    /// # Safety
    /// The caller must ensure that:
    /// - `self` is the current CPU's initialized `Scheduler` (`self.this_cpu() == curr_cpu_num()`).
    /// - Interrupts are disabled.
    /// - No `ChainLockTransaction`, `queue_lock_`, or `Thread::list_lock_` is held
    ///   (`TA_EXCL(chainlock_transaction_token, queue_lock_)`).
    pub unsafe fn migrate_unpinned_threads(&self) {
        let _trace = local_ktrace_begin_scope!(COMMON, "sched_migrate_unpinned");
        let current_cpu = crate::arch_rs::curr_cpu_num();
        // SAFETY: `self` is an initialized `Scheduler`.
        debug_assert_eq!(unsafe { self.this_cpu() }, current_cpu);
        let current_cpu_mask = cpu_num_to_mask(current_cpu);
        let mut cpus_to_reschedule_mask: CpuMask = 0;

        // Flag this scheduler as being in the process de-activating. If a thread
        // with a migration function unblocks and needs to become READY while we are
        // in the middle of `migrate_unpinned_threads`, we will allow it to join this
        // CPU's ready-queue knowing that we are going to immediately call the
        // thread's migration function (on this CPU, as required) before reassigning
        // the thread to a different CPU.
        //
        // Note: Because of the nature of the global thread-lock, this race is not
        // currently possible (Threads who are blocked cannot unblock while we hold
        // the global thread lock, which we currently do). Once the thread lock has
        // been removed, however, this race will become an important edge case to
        // handle.
        let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);

        // SAFETY: `self` is the current CPU's initialized `Scheduler`, interrupts
        // are disabled, and neither `queue_lock_` nor `Thread::list_lock_` is held.
        unsafe {
            (*self.state.get()).cpu_deactivating.store(true, core::sync::atomic::Ordering::Release);

            // Flag this CPU as being inactive. This will prevent new threads from being
            // added to this CPU's scheduler's run queues while we migrate the existing
            // threads off this scheduler.
            //
            // Note that this does _not_ prevent `find_target_cpu` from _selecting_ a given
            // scheduler as potential target for thread placement, but it _does_ guarantee
            // that by the time we attempt to actually add the thread to the chosen
            // scheduler's queue, we will have had a chance to check the flag once again,
            // this time safely from within the chosen scheduler's queue lock.
            Self::set_curr_cpu_active(false);

            // Call all migrate functions for threads last run on the current CPU who are
            // not currently READY. The next time these threads unblock or wake up, they
            // will be assigned to a different CPU, and we will have already called their
            // migrate function for them. When we are finished with this pass, the only
            // threads who have a migrate function still to be called should only exist in
            // this scheduler's READY queue, because:
            //
            // 1)  New threads or threads who have not recently run on this CPU cannot be
            //     assigned to this CPU (because we cleared our active flag)
            // 2a) Threads who were not ready but needed to have their migration function
            //     called were found and had their migration function called during
            //     `cpp_thread_call_migrate_fn_for_cpu` --- OR ---
            // 2b) The non-ready thread with a migration function unblocked and joined the
            //     ready queue of this scheduler while it was being deactivated. We will
            //     call their migration functions next as we move all of the READY threads
            //     off of this scheduler and over to a different one.
            cpp_thread_call_migrate_fn_for_cpu(current_cpu);

            // There should no longer be any non-ready threads who need to run a
            // migration function on this CPU, and there should not be any new one until
            // we set this CPU back to being active. We can clear the transitory
            // `cpu_deactivating` flag now.
            debug_assert!(
                (*self.state.get()).cpu_deactivating.load(core::sync::atomic::Ordering::Relaxed)
            );
            (*self.state.get())
                .cpu_deactivating
                .store(false, core::sync::atomic::Ordering::Release);
        }

        // Now move any READY threads who can be moved over to a temporary list,
        // flagging them as in the process of currently migrating. Also move all
        // of the threads in the `save_state_list` into a temporary list, as they
        // need to be migrated as well.
        let mut migrating_deadline_threads = core::mem::MaybeUninit::<UnblockList>::uninit();
        let migrating_deadline_threads_ptr = migrating_deadline_threads.as_mut_ptr();
        let mut migrating_fair_threads = core::mem::MaybeUninit::<UnblockList>::uninit();
        let migrating_fair_threads_ptr = migrating_fair_threads.as_mut_ptr();
        let mut local_save_state_list = core::mem::MaybeUninit::<SaveStateList>::uninit();
        let local_save_state_list_ptr = local_save_state_list.as_mut_ptr();

        let mut has_save_state = false;
        // SAFETY: `self` is valid, interrupts are disabled, and `queue_lock_` is not
        // already held. `_queue_guard` holds `self`'s `queue_lock_` while inspecting
        // the run queues and moving `save_state_list_`.
        let affine_transform = unsafe {
            let sched_ptr = self as *const Scheduler as *mut Scheduler;
            let _queue_guard = QueueLockGuard::acquire(sched_ptr);

            cpp_unblock_list_init(migrating_deadline_threads_ptr);
            cpp_unblock_list_init(migrating_fair_threads_ptr);

            // Save the current affine transform to update fair threads as they are migrated.
            let state = (*sched_ptr).state.get();
            let affine = (*state).fair_affine_transform;

            if !cpp_scheduler_save_state_list_is_empty(sched_ptr) {
                cpp_scheduler_save_state_list_move(sched_ptr, local_save_state_list_ptr);
                has_save_state = true;
            }

            let collect_migrating_threads =
                |run_queue: *const RunQueue, thread_list: *mut UnblockList| {
                    debug_assert!(
                        core::ptr::eq(run_queue, &raw const (*state).fair_run_queue)
                            || core::ptr::eq(
                                run_queue,
                                &raw const (*state).critical_deadline_run_queue,
                            )
                            || core::ptr::eq(run_queue, &raw const (*state).deadline_run_queue)
                    );
                    let mut iter = rust_kernel_run_queue_iterator_create(run_queue);
                    while rust_kernel_run_queue_iterator_is_valid(&iter) {
                        let consider = rust_kernel_run_queue_iterator_get(&iter);
                        rust_kernel_run_queue_iterator_next(&mut iter);

                        // We are holding the scheduler's queue lock, and iterating over the
                        // scheduler's run queues. All of the threads in those queues are
                        // clearly owned by this scheduler right now. We project `hard_affinity`
                        // via raw pointer to avoid forming `&SchedulerState` without holding
                        // `consider`'s thread lock.
                        // If the thread can run on any other CPU, add it to our list of threads
                        // to migrate.
                        if (*cpp_thread_get_scheduler_state(consider)).hard_affinity
                            != current_cpu_mask
                        {
                            cpp_unblock_list_push_back(thread_list, consider);
                        }
                    }
                };

            collect_migrating_threads(
                &raw const (*state).fair_run_queue,
                migrating_fair_threads_ptr,
            );
            collect_migrating_threads(
                &raw const (*state).critical_deadline_run_queue,
                migrating_deadline_threads_ptr,
            );
            collect_migrating_threads(
                &raw const (*state).deadline_run_queue,
                migrating_deadline_threads_ptr,
            );

            affine
        };

        let is_fair_thread = |thread: *mut Thread| -> bool {
            // SAFETY: Called only when `thread`'s lock is held exclusively.
            let state = unsafe { &*cpp_thread_get_scheduler_state(thread) };
            state.effective_profile.is_fair()
        };

        // OK, now that we have dropped our queue lock, go over our list of migrating
        // threads and finish the migration process. Don't make any assumptions about
        // the fair/deadline nature of the threads as we migrate. Since we dropped
        // the scheduler's queue lock, it is possible that these threads have changed
        // effective priority since being removed from their old scheduler, but before
        // arriving at a new one.
        let migrate_thread = |thread: *mut Thread,
                              update_timeline: bool,
                              trace_tag: &'static ktrace_rs::InternedString|
         -> CpuNum {
            // SAFETY: `thread` is a valid `Thread` popped from a local migration list.
            let thread_lock = unsafe { &*thread_get_lock(thread) };
            single_chain_lock_guard!(
                _guard,
                IRQ_SAVE_OPTION,
                thread_lock,
                clt_tag!("Scheduler::MigrateUnpinnedThreads")
            );

            // SAFETY: `self` is valid, interrupts are disabled, `thread`'s lock is held
            // by `_guard`, and `_queue_guard` acquires and releases `self`'s `queue_lock_`.
            unsafe {
                // Removing a thread from a scheduler requires `queue_lock` acquisition.
                let sched_ptr = self as *const Scheduler as *mut Scheduler;
                let _queue_guard = QueueLockGuard::acquire(sched_ptr);

                // Even though we dropped the queue lock between when we accumulated the list
                // of threads to move, and did not hold the thread lock prior to this point,
                // this thread should still be associated with this scheduler because the
                // scheduler was marked inactive above. This means that any attempt to steal
                // work from this scheduler should fail.
                debug_assert_eq!(
                    (*cpp_thread_get_scheduler_state(thread)).curr_cpu,
                    self.this_cpu()
                );
                self.trace_thread_queue_event(trace_tag, thread);
                self.erase_from_queue(thread);
                self.remove_thread_locked(now, thread);
            }

            // Threads removed from the fair run queue must be transformed to the monotonic
            // timeline before migrating.
            if update_timeline {
                debug_assert!(is_fair_thread(thread));
                // SAFETY: `thread` is valid, removed from `self`'s run queue, and its
                // lock is held exclusively by `_guard`.
                let thread_state = unsafe { &mut *cpp_thread_get_scheduler_state(thread) };
                affine_transform.variable_to_monotonic_in_place(&mut thread_state.start_time);
                affine_transform.variable_to_monotonic_in_place(&mut thread_state.finish_time);
            }

            // Call the Save stage of the thread's migration function as it leaves
            // this CPU.
            // SAFETY: `thread` is valid and its lock is held by `_guard`.
            unsafe {
                cpp_thread_call_migrate_fn_locked(thread, MIGRATE_STAGE_SAVE);
            }

            // SAFETY: `thread` is valid and its lock is held by `_guard` with interrupts
            // disabled. Inside the callback, `target`'s `queue_lock_` is held.
            unsafe {
                Self::find_active_scheduler_for_thread(thread, |thread, target| {
                    // Finish the transition and add the thread to the new target queue. The
                    // Restore stage of the migration function will be called on the new CPU the
                    // next time the thread becomes scheduled.
                    debug_assert!(!core::ptr::eq(target, self));
                    target.insert(now, thread, Placement::Migration);
                })
            }
        };

        // SAFETY: `migrating_fair_threads_ptr` was initialized by `cpp_unblock_list_init`.
        while unsafe { !cpp_unblock_list_is_empty(migrating_fair_threads_ptr) } {
            // SAFETY: `migrating_fair_threads_ptr` is initialized and non-empty.
            let thread = unsafe { cpp_unblock_list_pop_front(migrating_fair_threads_ptr) };
            let target_cpu = migrate_thread(
                thread,
                true,
                ktrace_rs::resolve_string!("tqe_deque_migrate_unpinned_fair"),
            );
            cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
        }

        // SAFETY: `migrating_deadline_threads_ptr` was initialized by `cpp_unblock_list_init`.
        while unsafe { !cpp_unblock_list_is_empty(migrating_deadline_threads_ptr) } {
            // SAFETY: `migrating_deadline_threads_ptr` is initialized and non-empty.
            let thread = unsafe { cpp_unblock_list_pop_front(migrating_deadline_threads_ptr) };
            let target_cpu = migrate_thread(
                thread,
                false,
                ktrace_rs::resolve_string!("tqe_deque_migrate_unpinned_deadline"),
            );
            cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
        }

        if has_save_state {
            loop {
                // SAFETY: `local_save_state_list_ptr` was initialized by
                // `cpp_scheduler_save_state_list_move` when `has_save_state` was set.
                let thread = unsafe { cpp_save_state_list_pop(local_save_state_list_ptr) };
                if thread.is_null() {
                    break;
                }
                let target_cpu = migrate_thread(
                    thread,
                    false,
                    ktrace_rs::resolve_string!("tqe_deque_migrate_unpinned_save_state"),
                );
                cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu);
            }
            // SAFETY: `local_save_state_list_ptr` was initialized and is now empty.
            unsafe { cpp_save_state_list_destroy(local_save_state_list_ptr) };
        }

        // SAFETY: Both `UnblockList`s were initialized by `cpp_unblock_list_init` and
        // drained above.
        unsafe {
            cpp_unblock_list_destroy(migrating_fair_threads_ptr);
            cpp_unblock_list_destroy(migrating_deadline_threads_ptr);
        }

        drop(_trace);
        if cpus_to_reschedule_mask != 0 {
            // SAFETY: Interrupts are disabled and no locks are held.
            unsafe { Self::reschedule_mask(cpus_to_reschedule_mask) };
        }
    }

    /// Sets the mask of valid CPUs that `thread` may run on. If a new mask
    /// is set, the thread will be migrated to satisfy the new constraint.
    ///
    /// # Safety
    /// The caller must ensure `thread` points to a valid, initialized `Thread`, that
    /// no `ChainLockTransaction` is active, and that `thread`'s lock is not held
    /// (`TA_EXCL(chainlock_transaction_token, thread.get_lock())`).
    pub unsafe fn set_cpu_affinity(
        thread: *mut Thread,
        affinity: CpuMask,
        affinity_type: AffinityType,
    ) -> CpuMask {
        if cfg!(debug_assertions) {
            let active_mask = peek_active_mask();
            debug_assert!(
                affinity_type == AffinityType::Soft || (affinity & active_mask) != 0,
                "Attempted to set affinity mask to {:#x}, which has no overlap of active CPUs {:#x}.",
                affinity,
                active_mask
            );
        }

        // SAFETY: `thread` is a valid initialized `Thread` per caller contract.
        let thread_lock = unsafe { thread_get_lock(thread) };
        // SAFETY: `thread` is a valid initialized `Thread` per caller contract.
        let state_ptr = unsafe { cpp_thread_get_scheduler_state(thread) };

        ChainLockTransaction::until_done(
            IRQ_SAVE_OPTION,
            clt_tag!("Scheduler::SetCpuAffinity"),
            |token| -> TransactionResult<CpuMask> {
                // SAFETY: `thread_lock` is valid and this is the first lock acquired in `token`.
                unsafe { (*thread_lock).acquire_first_in_chain(token) };

                // Mutating the affinity mask in the scheduler state requires holding both
                // the thread lock and the container lock, if any.
                //
                // TODO(johngro): It is really easy to mess this requirement up. Is there a
                // better way to statically assert these requirements?
                //
                // TODO(eieio): Why does the wait queue lock need to be held when updating
                // the affinity mask of a blocked thread? Only scheduling needs to read the
                // affinity mask and make decisions based on it, and even then only tracing
                // needs to read the affinity mask while holding only the queue lock. How
                // are wait queues different than the no container case wrt affinity?
                // SAFETY: `thread_lock` is held exclusively.
                let thread_state = unsafe { (*state_ptr).state };

                // The thread container is a scheduler.
                if (thread_state == ThreadState::Running) || (thread_state == ThreadState::Ready) {
                    let trace = local_ktrace_begin_scope!(COMMON, "sched_migrate");
                    ChainLockTransaction::finalize(token);

                    // SAFETY: `thread_lock` is held exclusively.
                    let curr_cpu = unsafe { (*state_ptr).curr_cpu };
                    debug_assert_ne!(curr_cpu, INVALID_CPU);
                    let thread_cpu_mask = cpu_num_to_mask(curr_cpu);
                    let mut cpus_to_reschedule_mask = 0;
                    let mut find_new_target_cpu = false;
                    let previous_affinity;

                    {
                        // SAFETY: `curr_cpu` is a valid CPU number, interrupts are disabled by
                        // `IRQ_SAVE_OPTION`, and `queue_lock_` is not already held.
                        let (scheduler_ptr, _queue_guard, scheduler, disposition, stolen_by) = unsafe {
                            let scheduler_ptr = cpp_scheduler_get(curr_cpu);
                            let queue_guard = QueueLockGuard::acquire(scheduler_ptr);
                            let scheduler = &*scheduler_ptr;
                            scheduler.assert_in_scheduler(thread);
                            let queue_state = &*cpp_thread_get_queue_state(thread);
                            (
                                scheduler_ptr,
                                queue_guard,
                                scheduler,
                                queue_state.disposition(),
                                queue_state.stolen_by,
                            )
                        };

                        // Set the affinity before determining if the thread needs to move.
                        // SAFETY: Both `thread_lock` and `scheduler`'s `queue_lock_` are held.
                        let (prev_aff, effective_cpu_mask) = unsafe {
                            let prev = set_affinity(&mut *state_ptr, affinity, affinity_type);
                            let eff = (*state_ptr).get_effective_cpu_mask(peek_active_mask());
                            (prev, eff)
                        };
                        previous_affinity = prev_aff;
                        let stale_curr_cpu = (thread_cpu_mask & effective_cpu_mask) == 0;

                        if thread_state == ThreadState::Running {
                            debug_assert_eq!(disposition, QueueDisposition::Associated);
                            if stale_curr_cpu {
                                cpus_to_reschedule_mask |= thread_cpu_mask;
                            }
                        } else {
                            debug_assert_eq!(thread_state, ThreadState::Ready);
                            debug_assert_ne!(disposition, QueueDisposition::Unassociated);

                            if disposition == QueueDisposition::Associated {
                                // The thread is transitioning to RUNNING: `curr_cpu` is correct.
                                COUNTER_UPDATE_IN_TRANSITION.add(1);
                                if stale_curr_cpu {
                                    cpus_to_reschedule_mask |= thread_cpu_mask;
                                }
                            } else if disposition == QueueDisposition::Stolen {
                                // The thread is being stolen: `curr_cpu` is incorrect, use
                                // `stolen_by`.
                                COUNTER_UPDATE_IN_TRANSITION.add(1);
                                debug_assert_ne!(stolen_by, INVALID_CPU);
                                let stealing_cpu_mask = cpu_num_to_mask(stolen_by);
                                let stale_stealing_cpu =
                                    (stealing_cpu_mask & effective_cpu_mask) == 0;
                                if stale_stealing_cpu {
                                    cpus_to_reschedule_mask |= stealing_cpu_mask;
                                }
                            } else {
                                // The READY thread is waiting in the run queue: `curr_cpu` is
                                // correct.
                                debug_assert_eq!(disposition, QueueDisposition::Enqueued);
                                if stale_curr_cpu {
                                    let now = SchedTime::from_raw(
                                        crate::platform_rs::timer::current_mono_time().0,
                                    );
                                    // SAFETY: Both `thread_lock` and `scheduler`'s `queue_lock_`
                                    // are held while `thread` is enqueued on `scheduler`.
                                    unsafe {
                                        scheduler.erase_from_queue(thread);
                                        scheduler.remove_thread_locked(now, thread);
                                    }

                                    // Execute the migrate function inline if the thread is on the
                                    // local CPU. Otherwise, add it to the save state list of the
                                    // last CPU it ran on and reschedule that CPU to process the
                                    // list.
                                    let current_cpu = crate::arch_rs::curr_cpu_num();
                                    // SAFETY: `thread_lock` and `scheduler`'s `queue_lock_` are
                                    // held.
                                    unsafe {
                                        let last_cpu = (*state_ptr).last_cpu;
                                        if cpp_thread_has_migrate_fn(thread)
                                            && !cpp_thread_migrate_pending(thread)
                                            && last_cpu != INVALID_CPU
                                            && last_cpu != current_cpu
                                        {
                                            cpp_scheduler_save_state_list_push_front(
                                                scheduler_ptr,
                                                thread,
                                            );
                                            cpus_to_reschedule_mask |= thread_cpu_mask;
                                        } else {
                                            find_new_target_cpu = true;
                                        }
                                    }
                                }
                            }
                        }
                    }

                    // Find a new target CPU for the thread.
                    if find_new_target_cpu {
                        // SAFETY: `thread_lock` is held exclusively.
                        debug_assert_eq!(unsafe { (*state_ptr).state }, ThreadState::Ready);
                        // Call the migration function if the thread is migrating away from this
                        // CPU. This is a no-op if there is no migration function or the Save
                        // state has already been invoked.
                        let current_cpu = crate::arch_rs::curr_cpu_num();
                        // SAFETY: `thread_lock` is held exclusively.
                        if unsafe { (*state_ptr).last_cpu } == current_cpu {
                            // SAFETY: `thread` is valid and `thread_lock` is held.
                            unsafe {
                                cpp_thread_call_migrate_fn_locked(thread, MIGRATE_STAGE_SAVE);
                            }
                        }

                        // Find a new CPU for the thread and add it to the queue.
                        // SAFETY: `thread` is valid with `thread_lock` held and interrupts
                        // disabled. Inside the callback, `target`'s `queue_lock_` is held.
                        let target_cpu = unsafe {
                            Self::find_active_scheduler_for_thread(thread, |thread, target| {
                                let now = SchedTime::from_raw(
                                    crate::platform_rs::timer::current_mono_time().0,
                                );
                                target.insert(now, thread, Placement::Migration);
                            })
                        };
                        // Reschedule both CPUs to handle the run queue changes.
                        cpus_to_reschedule_mask |= cpu_num_to_mask(target_cpu) | thread_cpu_mask;
                    }

                    ktrace_rs::end_scope!(trace);
                    // SAFETY: `thread_lock` was acquired above in `token`.
                    unsafe { (*thread_lock).release(token) };
                    // SAFETY: Interrupts are disabled and `token` now holds 0 locks.
                    unsafe { Self::reschedule_mask(cpus_to_reschedule_mask) };
                    return TransactionResult::Value(previous_affinity);
                }

                // The thread container is a wait queue.
                // TODO(eieio): Why do we need to synchronize the affinity mask with the
                // wait queue?
                if thread_state == ThreadState::Blocked {
                    // SAFETY: `thread` is in `ThreadState::Blocked` and `thread_lock` is held.
                    let wq_lock = unsafe {
                        let wq = cpp_thread_get_blocking_wait_queue(thread);
                        debug_assert!(!wq.is_null());
                        cpp_wait_queue_get_lock(wq)
                    };

                    // SAFETY: `wq_lock` is valid and `token` is active and unfinalized.
                    if !unsafe { (*wq_lock).acquire_or_backoff(token) } {
                        // SAFETY: `thread_lock` was acquired above in `token`.
                        unsafe { (*thread_lock).release(token) };
                        return Action::Backoff.into();
                    }

                    ChainLockTransaction::finalize(token);
                    // SAFETY: Both `thread_lock` and `wq_lock` are held exclusively.
                    let previous_affinity =
                        unsafe { set_affinity(&mut *state_ptr, affinity, affinity_type) };

                    // SAFETY: `wq_lock` and `thread_lock` were acquired above in `token`.
                    unsafe {
                        (*wq_lock).release(token);
                        (*thread_lock).release(token);
                    }
                    return TransactionResult::Value(previous_affinity);
                }

                // The thread has no container (e.g. SLEEPING, SUSPENDED).
                ChainLockTransaction::finalize(token);
                // SAFETY: `thread_lock` is held exclusively and `thread` has no container.
                let previous_affinity =
                    unsafe { set_affinity(&mut *state_ptr, affinity, affinity_type) };
                // SAFETY: `thread_lock` was acquired above in `token`.
                unsafe { (*thread_lock).release(token) };
                TransactionResult::Value(previous_affinity)
            },
        )
    }
}

/// Utility to set the correct affinity mask based on the affinity type.
/// Callers must hold the thread's lock and its container lock (if any) when
/// forming `state: &mut SchedulerState`.
fn set_affinity(
    state: &mut SchedulerState,
    mut affinity: CpuMask,
    affinity_type: AffinityType,
) -> CpuMask {
    match affinity_type {
        AffinityType::Hard => {
            core::mem::swap(&mut state.hard_affinity, &mut affinity);
        }
        AffinityType::Soft => {
            core::mem::swap(&mut state.soft_affinity, &mut affinity);
        }
    }
    affinity
}

/// Sets the mask of valid CPUs that `thread` may run on. If a new mask
/// is set, the thread will be migrated to satisfy the new constraint.
///
/// # Safety
/// The caller must ensure `thread` points to a valid, initialized `Thread`, that
/// no `ChainLockTransaction` is active, that `thread`'s lock is not held
/// (`TA_EXCL(chainlock_transaction_token, thread.get_lock())`), and that no
/// scheduler `queue_lock_` is held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_set_cpu_affinity(
    thread: *mut Thread,
    affinity: CpuMask,
    affinity_type: i32,
) -> CpuMask {
    let affinity_type = match affinity_type {
        0 => AffinityType::Hard,
        1 => AffinityType::Soft,
        _ => panic!("invalid affinity type"),
    };
    // SAFETY: Forwarded from the caller.
    unsafe { Scheduler::set_cpu_affinity(thread, affinity, affinity_type) }
}

/// Indicates the type of preemption for diagnostic/tracing purposes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum PreemptType {
    Irq = 0,
    FlushPending = 1,
    Reschedule = 2,
}

#[inline]
fn preempt_type_from_raw(raw: u8) -> PreemptType {
    match raw {
        0 => PreemptType::Irq,
        1 => PreemptType::FlushPending,
        2 => PreemptType::Reschedule,
        _ => unreachable!(),
    }
}

/// Finishes unblocking a thread. This function expects the thread to be locked
/// when called. It will:
///
/// 1) Select a scheduler compatible with the thread.
/// 2) Place the thread into that scheduler's run queue.
/// 3) Drop the thread's lock.
/// 4) Reschedule if needed/permitted.
///
/// # Safety
/// - `thread` must point to a valid initialized `Thread` whose lock is held by
///   an active, finalized `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token)`,
///   `TA_REL(thread->get_lock())`).
/// - Interrupts must be disabled.
unsafe fn unblock_raw(thread: *mut Thread) {
    let mut trace = local_ktrace_begin_scope!(COMMON, "sched_unblock");
    // SAFETY: The caller guarantees an active `ChainLockTransaction` on the current CPU.
    let token = unsafe { ChainLockTransaction::assert_active() };
    ChainLockTransaction::active_ref(&token).assert_finalized();
    let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
    // SAFETY: `thread` is a valid `Thread` pointer and its lock is held by the caller.
    let targets = unsafe { unblock_common(thread, now, UnblockType::Normal) };
    // SAFETY: `thread` is a valid pointer to `Thread`.
    let lock = unsafe { thread_get_lock(thread) };
    // SAFETY: `lock` is a valid pointer to `ChainLock` and was held by the caller.
    unsafe { (*lock).release(&token) };
    if let Some(t) = trace.as_mut() {
        t.end();
    }
    // SAFETY: `Scheduler::reschedule_mask` is safe to call after releasing `thread`'s lock.
    unsafe { Scheduler::reschedule_mask(targets.mask()) };
}

/// Expects to receive a list of threads, all of whose locks are currently
/// held. It will drop each thread's lock after successfully assigning it to a
/// scheduler.
///
/// # Safety
/// - `list` must point to a valid `Thread::UnblockList` whose contained threads
///   are all valid, non-idle, and have their locks held by an active, finalized
///   `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token)`).
/// - Interrupts must be disabled.
unsafe fn unblock_list_raw(list: *mut UnblockList) {
    let mut trace = local_ktrace_begin_scope!(COMMON, "sched_unblock_list");
    // SAFETY: The caller guarantees an active `ChainLockTransaction` on the current CPU.
    let token = unsafe { ChainLockTransaction::assert_active() };
    ChainLockTransaction::active_ref(&token).assert_finalized();
    let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
    let mut reschedule_mask = 0;
    loop {
        // SAFETY: `list` is a valid pointer to a C++ `Thread::UnblockList`.
        let thread = unsafe { cpp_unblock_list_pop_back(list) };
        if thread.is_null() {
            break;
        }
        // SAFETY: `thread` is a valid `Thread` popped from `list`.
        debug_assert!(!unsafe { Thread::is_idle_ptr(thread) });
        // SAFETY: `thread` is valid and its lock is held in `token`.
        let lock = unsafe { thread_get_lock(thread) };
        unsafe { (*lock).assert_acquired(&token) };
        // SAFETY: `thread` is valid and its lock is held.
        let targets = unsafe { unblock_common(thread, now, UnblockType::Normal) };
        reschedule_mask |= targets.mask();
        // SAFETY: `lock` is valid and held in `token`.
        unsafe { (*lock).release(&token) };
    }
    if let Some(t) = trace.as_mut() {
        t.end();
    }
    // SAFETY: All unblocked threads' locks have been released.
    unsafe { Scheduler::reschedule_mask(reschedule_mask) };
}

/// Similar to `unblock`, except that it uses different target CPU selection
/// criteria to enable a "synchronous wake", if possible.
///
/// A synchronous wake is a sequence of operations where the current thread
/// wakes another thread, updates the woken thread's scheduling parameters, and
/// then blocks, such that, under compatible conditions, the woken thread
/// replaces the current thread as the actively running thread on the CPU in
/// the _same reschedule operation_. A correctly executed synchronous wake
/// optimizes queuing latency in synchronous wake-and-wait operations, such as
/// channel calls.
///
/// Due to the structure of run queues and wait queues, and their respective
/// locking protocols, a synchronous wake cannot be implemented as a single
/// scheduler operation. Instead, some variation of the following sequence of
/// operations must be executed to achieve the same effect:
///
/// 1. Prevent immediate rescheduling in step 2 using an appropriate mechanism,
///    or combination of mechanisms, for the use case (i.e. spinlock + irq
///    disable, preempt disable, eager resched disable).
/// 2. Unblock the target thread and place it on the same CPU as the current
///    thread, if possible.
/// 3. Update the target thread's scheduling parameters to transfer bandwidth
///    and/or urgency from the current to the target thread, if possible.
/// 4. Block the current thread and reschedule the local CPU, and remote CPU if
///    applicable.
///
/// This function implements step 2 of the synchronous wake sequence. Note that
/// the normal `unblock` may be used in place of this function without impacting
/// correctness. However, suboptimal task placement can occur at step 2 and
/// multiple reschedules may be necessary in steps 3 and 4.
///
/// # Safety
/// - `thread` must point to a valid initialized `Thread` whose lock is held by
///   an active, finalized `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token)`,
///   `TA_REL(thread->get_lock())`).
/// - Interrupts must be disabled.
unsafe fn unblock_synchronous_raw(thread: *mut Thread) {
    let mut trace = local_ktrace_begin_scope!(COMMON, "sched_unblock_sync");
    // SAFETY: The caller guarantees an active `ChainLockTransaction` on the current CPU.
    let token = unsafe { ChainLockTransaction::assert_active() };
    ChainLockTransaction::active_ref(&token).assert_finalized();
    let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
    // SAFETY: `thread` is valid and locked by the caller.
    let targets = unsafe { unblock_common(thread, now, UnblockType::Synchronous) };
    // SAFETY: `thread` is valid.
    let lock = unsafe { thread_get_lock(thread) };
    // SAFETY: `lock` is valid and held by the caller.
    unsafe { (*lock).release(&token) };
    if let Some(t) = trace.as_mut() {
        t.end();
    }

    // If there was a power level change we need to account for, reschedule those CPUs. But do not
    // reschedule the target CPU, if it is the current CPU.
    let current_cpu = crate::arch_rs::curr_cpu_num();
    if targets.target_cpu == current_cpu {
        // SAFETY: `Scheduler::reschedule_mask` is safe to call after releasing `thread`'s lock.
        unsafe { Scheduler::reschedule_mask(targets.cpus_to_reschedule) };
    } else {
        // SAFETY: `Scheduler::reschedule_mask` is safe to call after releasing `thread`'s lock.
        unsafe { Scheduler::reschedule_mask(targets.mask()) };
    }
}

/// Used in the process of creation of the idle thread. It simply asserts that
/// the thread is (in fact) flagged as the idle thread, and that it has hard
/// affinity for exactly one CPU (its CPU). It then sets the thread to be ready,
/// and ensures that its `curr_cpu` state is correct.
///
/// # Safety
/// - `thread` must point to a valid idle `Thread` whose lock is held by an
///   active `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token, idle_thread->get_lock())`).
/// - Interrupts must be disabled and `target_scheduler`'s `queue_lock_` must not
///   be held.
unsafe fn unblock_idle(thread: *mut Thread) {
    // SAFETY: `thread` is valid.
    debug_assert!(unsafe { Thread::is_idle_ptr(thread) });
    // SAFETY: `thread` is valid and its lock is held by the caller.
    let state_ptr = unsafe { cpp_thread_get_scheduler_state(thread) };
    // SAFETY: `thread`'s lock is held.
    let hard_affinity = unsafe { (*state_ptr).hard_affinity };
    debug_assert!(hard_affinity.count_ones() == 1);
    let target_cpu = hard_affinity.trailing_zeros() as CpuNum;
    // SAFETY: `target_cpu` is a valid CPU number from `hard_affinity`.
    let target_scheduler = unsafe { cpp_scheduler_get(target_cpu) };

    // SAFETY: `target_scheduler` is valid, interrupts are disabled, and
    // `queue_lock_` is not already held.
    let _guard = unsafe { QueueLockGuard::acquire(target_scheduler) };
    // SAFETY: `thread` is valid and both `thread`'s lock and `target_scheduler`'s
    // `queue_lock_` are held.
    unsafe {
        cpp_thread_set_ready(thread);
        (*state_ptr).curr_cpu = target_cpu;
        (*target_scheduler).assert_in_scheduler(thread);
        let queue_state = &mut *cpp_thread_get_queue_state(thread);
        queue_state.on_insert();
    }
}

/// Yields the current thread's remaining time slice if it is a fair thread
/// running in an eligible activation.
///
/// # Safety
/// - `current_thread` must be the current CPU's active thread (`current_get()`),
///   and its lock must be the sole lock held by an active, finalized
///   `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`).
/// - Interrupts must be disabled and the current CPU's `queue_lock_` must not be held.
unsafe fn yield_thread(current_thread: *mut Thread) {
    let mut trace = local_ktrace_begin_scope!(COMMON, "sched_yield");
    debug_assert_eq!(current_thread, super::thread::current_get());
    // SAFETY: `current_thread` is valid.
    debug_assert!(!unsafe { Thread::is_idle_ptr(current_thread) });
    // SAFETY: `current_thread` is valid and its lock is held by the caller.
    let state_ptr = unsafe { cpp_thread_get_scheduler_state(current_thread) };
    // SAFETY: `current_thread`'s lock is held.
    let is_fair = unsafe { (*state_ptr).effective_profile.is_fair() };
    if is_fair {
        let current_cpu = crate::arch_rs::curr_cpu_num();
        // SAFETY: `current_cpu` is a valid CPU number.
        let sched = unsafe { cpp_scheduler_get(current_cpu) };
        // If the thread is running in an eligible activation, expire the time slice
        // to push it into the next activation, potentially allowing other eligible
        // threads to run. However, if the start time is in the future, this thread
        // is running because there were no other eligible fair threads to run and
        // the eligible time was snapped forward. In that case, avoid pushing the
        // activation further into the future.
        let now = SchedTime::from_raw(crate::platform_rs::timer::current_mono_time().0);
        // SAFETY: `current_thread`'s lock is held.
        if unsafe { (*state_ptr).start_time < now } {
            // SAFETY: `current_thread`'s lock is held.
            unsafe { (*state_ptr).time_slice_used_ns = (*state_ptr).time_slice_ns };
            // SAFETY: `sched` and `current_thread` are valid, and `current_thread`'s
            // lock is the sole lock held in the active `ChainLockTransaction`.
            unsafe {
                (*sched).reschedule_common(
                    current_thread,
                    trace.as_mut().map(|s| s as &mut dyn ktrace_rs::ErasedScope),
                );
            }
        }
    }
}

/// No chainlocks should be held when calling `preempt`. The thread's lock will
/// be obtained unconditionally in the process.
///
/// If holding a spinlock, the preemption will be deferred via self-IPI until
/// all spinlocks are released and interrupts are re-enabled.
///
/// # Safety
/// Must be called with no active `ChainLockTransaction` on the current CPU
/// (`TA_EXCL(chainlock_transaction_token)`).
pub(crate) unsafe fn preempt(preempt_type: PreemptType) {
    let current_thread = super::thread::current_get();
    // SAFETY: `current_thread` is the current CPU's active thread.
    let lock = unsafe { &*thread_get_lock(current_thread) };
    single_chain_lock_guard!(_guard, IRQ_SAVE_OPTION, lock, clt_tag!("Scheduler::Preempt"));
    // SAFETY: `current_thread` is valid and `_guard` holds its lock as the sole
    // lock in the active `ChainLockTransaction`.
    unsafe {
        preempt_locked(current_thread, preempt_type);
    }
}

/// Preempts `current_thread` while its lock is already held.
///
/// # Safety
/// - `current_thread` must be the current CPU's active thread, and its lock
///   must be the sole lock held by an active, finalized `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`).
/// - Interrupts must be disabled and the current CPU's `queue_lock_` must not be held.
unsafe fn preempt_locked(current_thread: *mut Thread, preempt_type: PreemptType) {
    let mut trace = local_ktrace_begin_scope!(
        COMMON,
        "sched_preempt",
        "type" => preempt_type_to_str(preempt_type),
    );
    let current_cpu = crate::arch_rs::curr_cpu_num();
    // If any spinlocks are held, we can't immediately reschedule. Instead, send
    // this CPU a reschedule IPI that will fire once the last spinlock has been
    // released and interrupts have been re-enabled.
    // SAFETY: Querying per-CPU spinlock count with interrupts disabled.
    if unsafe { cpp_arch_num_spinlocks_held() } > 0 {
        // SAFETY: `current_thread` is valid.
        unsafe {
            (*Thread::preemption_state_ptr(current_thread))
                .preempts_pending_add(cpu_num_to_mask(current_cpu));
        }
        super::mp::reschedule_self();
        return;
    }

    {
        // SAFETY: `current_thread` is valid and its lock is held by the caller.
        let state = unsafe { &*cpp_thread_get_scheduler_state(current_thread) };
        debug_assert!(state.curr_cpu == current_cpu);
        debug_assert!(state.last_cpu == state.curr_cpu);
        debug_assert_eq!(state.state, ThreadState::Running);
    }

    // SAFETY: `current_cpu` is a valid CPU number.
    let sched = unsafe { cpp_scheduler_get(current_cpu) };
    // SAFETY: `sched` and `current_thread` are valid, and `current_thread`'s lock
    // is the sole lock held in the active `ChainLockTransaction`.
    unsafe {
        (*sched).reschedule_common(
            current_thread,
            trace.as_mut().map(|s| s as &mut dyn ktrace_rs::ErasedScope),
        );
    }
}

/// Reschedules the current CPU if preemption is enabled and no spinlocks are held.
///
/// # Safety
/// - `current_thread` must be the current CPU's active thread (`current_get()`),
///   and its lock must be the sole lock held by an active, finalized
///   `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`).
/// - Interrupts must be disabled and the current CPU's `queue_lock_` must not be held.
pub(crate) unsafe fn reschedule(current_thread: *mut Thread) {
    let mut trace = local_ktrace_begin_scope!(COMMON, "sched_reschedule");
    debug_assert!(current_thread == super::thread::current_get());
    let current_cpu = crate::arch_rs::curr_cpu_num();
    // SAFETY: `current_thread` is valid.
    let ps = unsafe { &*Thread::preemption_state_ptr(current_thread) };
    let preempt_enabled = ps.evaluate_timeslice_extension();
    // Pend the preemption rather than rescheduling if preemption is disabled.
    if !preempt_enabled {
        ps.preempts_pending_add(cpu_num_to_mask(current_cpu));
        return;
    }

    // If any spinlocks are held, we can't immediately reschedule. Instead, send
    // this CPU a reschedule IPI that will fire once the last spinlock has been
    // released and interrupts have been re-enabled.
    // SAFETY: Querying per-CPU spinlock count with interrupts disabled.
    if unsafe { cpp_arch_num_spinlocks_held() } > 0 {
        ps.preempts_pending_add(cpu_num_to_mask(current_cpu));
        super::mp::reschedule_self();
        return;
    }

    {
        // SAFETY: `current_thread` is valid and its lock is held by the caller.
        let state = unsafe { &*cpp_thread_get_scheduler_state(current_thread) };
        debug_assert!(state.curr_cpu == current_cpu);
        debug_assert!(state.last_cpu == state.curr_cpu);
    }

    // SAFETY: `current_cpu` is a valid CPU number.
    let sched = unsafe { cpp_scheduler_get(current_cpu) };
    // SAFETY: `sched` and `current_thread` are valid, and `current_thread`'s lock
    // is the sole lock held in the active `ChainLockTransaction`.
    unsafe {
        (*sched).reschedule_common(
            current_thread,
            trace.as_mut().map(|s| s as &mut dyn ktrace_rs::ErasedScope),
        );
    }
}

/// Internal reschedule entry point used for initial thread startup and exit.
///
/// # Safety
/// - `current_thread` must be the current CPU's active thread (`current_get()`),
///   and its lock must be the sole lock held by an active, finalized
///   `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`).
/// - Interrupts must be disabled and the current CPU's `queue_lock_` must not be held.
unsafe fn reschedule_internal(current_thread: *mut Thread) {
    let mut trace = local_ktrace_begin_scope!(COMMON, "sched_resched_internal");
    let current_cpu = crate::arch_rs::curr_cpu_num();
    // SAFETY: `current_cpu` is a valid CPU number.
    let sched = unsafe { cpp_scheduler_get(current_cpu) };
    // SAFETY: `sched` and `current_thread` are valid, and `current_thread`'s lock
    // is the sole lock held in the active `ChainLockTransaction`.
    unsafe {
        (*sched).reschedule_common(
            current_thread,
            trace.as_mut().map(|s| s as &mut dyn ktrace_rs::ErasedScope),
        );
    }
}

/// Unblocks `thread`, releasing its lock and rescheduling as needed.
///
/// # Safety
/// `thread` must point to a valid, initialized `Thread` whose lock is held by an
/// active `ChainLockTransaction` (`TA_REQ(chainlock_transaction_token)`); the lock
/// is released on return (`TA_REL(thread->get_lock())`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_unblock(thread: *mut Thread) {
    // SAFETY: Forwarded from the caller.
    unsafe { unblock_raw(thread) };
}

/// Unblocks all threads in `list`, releasing each thread's lock and rescheduling as needed.
///
/// # Safety
/// `list` must point to a valid, initialized `Thread::UnblockList` whose threads'
/// locks are held by an active `ChainLockTransaction`
/// (`TA_REQ(chainlock_transaction_token)`); each lock is released on return.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_unblock_list(list: *mut UnblockList) {
    // SAFETY: Forwarded from the caller.
    unsafe { unblock_list_raw(list) };
}

/// Marks the idle `thread` ready on its assigned CPU.
///
/// # Safety
/// `thread` must point to a valid, initialized idle `Thread` whose lock is held by
/// an active `ChainLockTransaction`
/// (`TA_REQ(chainlock_transaction_token, thread->get_lock())`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_unblock_idle(thread: *mut Thread) {
    // SAFETY: Forwarded from the caller.
    unsafe { unblock_idle(thread) };
}

/// Yields `current_thread`'s remaining time slice.
///
/// # Safety
/// `current_thread` must be the current CPU's active thread, with its lock held by
/// an active `ChainLockTransaction`
/// (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`), and no
/// `queue_lock_` may be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_yield(current_thread: *mut Thread) {
    // SAFETY: Forwarded from the caller.
    unsafe { yield_thread(current_thread) };
}

/// Preempts the current thread on the local CPU.
///
/// # Safety
/// `preempt_type` must be a valid `PreemptType` discriminant, no
/// `ChainLockTransaction` may be active (`TA_EXCL(chainlock_transaction_token)`),
/// and no `queue_lock_` may be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_preempt(preempt_type: u8) {
    let pt = preempt_type_from_raw(preempt_type);
    // SAFETY: Forwarded from the caller.
    unsafe { preempt(pt) };
}

/// Reschedules `current_thread` on the local CPU if preemption is enabled.
///
/// # Safety
/// `current_thread` must be the current CPU's active thread, with its lock held by
/// an active `ChainLockTransaction`
/// (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`), and no
/// `queue_lock_` may be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_reschedule(current_thread: *mut Thread) {
    // SAFETY: Forwarded from the caller.
    unsafe { reschedule(current_thread) };
}

/// Reschedules `current_thread` unconditionally during thread startup or exit.
///
/// # Safety
/// `current_thread` must be the current CPU's active thread, with its lock the sole
/// lock held by an active, finalized `ChainLockTransaction`
/// (`TA_REQ(chainlock_transaction_token, current_thread->get_lock())`); interrupts
/// must be disabled and no `queue_lock_` may be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_reschedule_internal(current_thread: *mut Thread) {
    // SAFETY: Forwarded from the caller.
    unsafe { reschedule_internal(current_thread) };
}

impl Scheduler {
    /// Initializes a thread's scheduler state.
    ///
    /// See the comment in `Thread::CreateEtc` for why `initialize_thread` does not
    /// take a stance on whether or not we should be in an active chain-lock
    /// transaction. On the surface, demanding that we hold a thread's lock without
    /// also demanding that we also be involved in an active chain-lock transaction
    /// is an apparent contradiction of some fundamental invariants (how can a
    /// chain lock be held without being in a transaction?), but when used
    /// exclusively from `Thread::CreateEtc` (or `initialize_first_thread`), it is
    /// sound.
    ///
    /// # Safety
    /// - `thread` must point to a valid `Thread` whose lock is held exclusively by
    ///   the caller (`TA_REQ(thread->get_lock())`) before the thread is associated
    ///   with any scheduler or wait queue.
    #[inline]
    pub unsafe fn initialize_thread(thread: *mut Thread, profile: &BaseProfile) {
        let expected_runtime_ns = if profile.is_fair() {
            K_MINIMUM_FAIR_CAPACITY
        } else {
            profile.deadline().capacity_ns
        };
        // SAFETY: `thread` is valid, so `state_ptr` points to its `SchedulerState`, and
        // the caller holds `thread`'s lock exclusively before the thread is visible
        // elsewhere. Initializing in place avoids a stack temporary of the whole state.
        unsafe {
            let state_ptr = cpp_thread_get_scheduler_state(thread);
            state_ptr.write(SchedulerState::new(*profile));
            (*state_ptr).expected_runtime_ns = expected_runtime_ns;
        }
    }

    /// Initializes the first thread to run on the current CPU. Called from
    /// `thread_construct_first`, this function will initialize the thread's
    /// scheduler state, then mark the thread as being "active" in its CPU's
    /// scheduler.
    ///
    /// # Safety
    /// - `thread` must point to the current CPU's initial `Thread`, and an active
    ///   `ChainLockTransaction` must hold its lock exclusively
    ///   (`TA_REQ(chainlock_transaction_token, thread->get_lock())`).
    /// - Interrupts must be disabled and the current CPU's `queue_lock_` must not
    ///   already be held (`TA_EXCL(queue_lock_)`).
    pub unsafe fn initialize_first_thread(thread: *mut Thread) {
        let current_cpu = crate::arch_rs::curr_cpu_num();

        // Construct our scheduler state and assign a "priority".
        let base_profile = BaseProfile::new_fair(HIGHEST_PRIORITY);
        // SAFETY: `thread` is valid and its lock is held exclusively by the caller.
        unsafe { Self::initialize_thread(thread, &base_profile) };

        // Fill out other details about the thread, making sure to assign it to the
        // current CPU with hard affinity.
        // SAFETY: `thread` is valid and `initialize_thread` initialized its
        // `SchedulerState` above under `thread`'s lock, which is still held.
        let (weight, expected_runtime_ns) = unsafe {
            let state_ptr = cpp_thread_get_scheduler_state(thread);
            (*state_ptr).state = ThreadState::Running;
            (*state_ptr).curr_cpu = current_cpu;
            (*state_ptr).last_cpu = current_cpu;
            (*state_ptr).hard_affinity = cpu_num_to_mask(current_cpu);
            ((*state_ptr).effective_profile.weight, (*state_ptr).expected_runtime_ns)
        };

        // Finally, make sure that the thread is the active thread for the scheduler,
        // and that the `weight_total` bookkeeping is accurate.
        // SAFETY: `current_cpu` is valid, interrupts are disabled, `queue_lock_` is
        // not already held, and `thread`'s lock is held exclusively.
        unsafe {
            let scheduler_ptr = cpp_scheduler_get(current_cpu);
            let _queue_guard = QueueLockGuard::acquire(scheduler_ptr);
            let scheduler = &*scheduler_ptr;

            scheduler.assert_in_scheduler(thread);

            let queue_state = &mut *cpp_thread_get_queue_state(thread);
            queue_state.on_insert();
            scheduler.set_active_thread(thread);

            let mut weight_total = scheduler.weight_total();
            weight_total += weight;
            debug_assert!(weight_total.raw_value() > 0);
            scheduler.set_weight_total(weight_total);

            let runnable_task_count = scheduler.runnable_task_count();
            scheduler.set_runnable_task_count(runnable_task_count + 1);
            scheduler.update_total_expected_runtime(expected_runtime_ns);
        }
    }

    /// Removes the impact of a CPU's first thread from the scheduler's bookkeeping.
    ///
    /// During initial startup, threads are not _really_ being scheduled, yet they
    /// can still do things like obtain locks and block, resulting in profile
    /// inheritance. In order to hold the scheduler's bookkeeping invariants, we
    /// assign these threads a fair weight, and include it in the total fair weight
    /// tracked by the scheduler instance. When the thread either becomes the idle
    /// thread (as the boot CPU first thread does), or exits (as secondary CPU first
    /// threads do), it is important that we remove this weight from the total
    /// bookkeeping. However, this is not as simple as just changing the thread's
    /// weight via `change_weight`, as idle threads are special cases who contribute
    /// no weight to the total.
    ///
    /// So, this small function simply fixes up the bookkeeping before allowing the
    /// thread to move on to become the idle thread (boot CPU), or simply exiting
    /// (secondary CPU).
    ///
    /// # Safety
    /// - `thread` must point to the current CPU's active first `Thread`, and its
    ///   lock must be held in an active `ChainLockTransaction`
    ///   (`TA_REQ(chainlock_transaction_token, thread->get_lock())`).
    /// - Interrupts must be disabled and the current CPU's `queue_lock_` must not
    ///   already be held (`TA_EXCL(queue_lock_)`).
    pub unsafe fn remove_first_thread(thread: *mut Thread) {
        let current_cpu = crate::arch_rs::curr_cpu_num();
        // SAFETY: `current_cpu` and `thread` are valid, and `thread`'s lock is held.
        let (scheduler_ptr, state_ptr) =
            unsafe { (cpp_scheduler_get(current_cpu), cpp_thread_get_scheduler_state(thread)) };

        // Since this is becoming an idle thread, it must have been one of the CPU's
        // first threads. It should already be bound to this core with hard affinity.
        // Assert this.
        // SAFETY: `thread`'s lock is held exclusively by the caller.
        unsafe {
            debug_assert_eq!((*state_ptr).last_cpu, current_cpu);
            debug_assert_eq!((*state_ptr).curr_cpu, current_cpu);
            debug_assert_eq!((*state_ptr).hard_affinity, cpu_num_to_mask(current_cpu));
        }

        // SAFETY: `scheduler_ptr` is valid, interrupts are disabled, `queue_lock_` is
        // not already held, and `thread`'s lock is held exclusively. `queue_state`
        // (`SchedulerQueueState`) and `state` (`SchedulerState`) are disjoint fields
        // of `Thread`.
        unsafe {
            let _queue_guard = QueueLockGuard::acquire(scheduler_ptr);
            let scheduler = &*scheduler_ptr;

            scheduler.assert_in_scheduler(thread);
            let queue_state = &mut *cpp_thread_get_queue_state(thread);

            let state = &mut *state_ptr;
            // We are becoming the idle thread. We should currently be running with a
            // fair (not deadline) profile, and we should not be holding any locks
            // (therefore, we should not be inheriting any profile pressure).
            debug_assert!(state.base_profile.is_fair());
            debug_assert_eq!(state.inherited_profile_values.total_weight.raw_value(), 0);
            debug_assert_eq!(state.inherited_profile_values.uncapped_utilization.raw_value(), 0);

            // We should also be the currently active thread on this core, but no
            // longer. We are about to either exit, or `unblock_idle`.
            debug_assert_eq!((*scheduler.state.get()).active_thread, thread);
            debug_assert!(scheduler.runnable_task_count() > 0);

            queue_state.on_remove(INVALID_CPU); // Set active to false.
            scheduler.set_active_thread(core::ptr::null_mut());

            let mut weight_total = scheduler.weight_total();
            weight_total -= state.effective_profile.weight;
            debug_assert!(weight_total.raw_value() >= 0);
            scheduler.set_weight_total(weight_total);

            let runnable_task_count = scheduler.runnable_task_count();
            scheduler.set_runnable_task_count(runnable_task_count - 1);

            let delta = SchedDuration::from_raw(-state.expected_runtime_ns.raw_value());
            scheduler.update_total_expected_runtime(delta);

            state
                .base_profile
                .set_fair_weight(SchedulerState::convert_priority_to_weight(IDLE_PRIORITY));
            state.effective_profile.mark_base_profile_changed();
            state.recompute_effective_profile();
        }
    }
}

/// Initializes a thread's scheduler state. See `Scheduler::initialize_thread`.
///
/// # Safety
/// - `thread` must point to a valid `Thread` whose lock is held exclusively by
///   the caller (`TA_REQ(thread->get_lock())`) before the thread is associated
///   with any scheduler or wait queue.
/// - `profile` must point to a valid, initialized `BaseProfile`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_initialize_thread(
    thread: *mut Thread,
    profile: *const BaseProfile,
) {
    // SAFETY: Forwarded from the caller.
    unsafe { Scheduler::initialize_thread(thread, &*profile) };
}

/// Initializes the first thread to run on the current CPU. See
/// `Scheduler::initialize_first_thread`.
///
/// # Safety
/// - `thread` must point to the current CPU's initial `Thread`, and an active
///   `ChainLockTransaction` must hold its lock exclusively
///   (`TA_REQ(chainlock_transaction_token, thread->get_lock())`).
/// - Interrupts must be disabled and the current CPU's `queue_lock_` must not
///   already be held (`TA_EXCL(queue_lock_)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_initialize_first_thread(thread: *mut Thread) {
    // SAFETY: Forwarded from the caller.
    unsafe { Scheduler::initialize_first_thread(thread) };
}

/// Removes the impact of a CPU's first thread from the scheduler's bookkeeping.
/// See `Scheduler::remove_first_thread`.
///
/// # Safety
/// - `thread` must point to the current CPU's active first `Thread`, and its
///   lock must be held in an active `ChainLockTransaction`
///   (`TA_REQ(chainlock_transaction_token, thread->get_lock())`).
/// - Interrupts must be disabled and the current CPU's `queue_lock_` must not
///   already be held (`TA_EXCL(queue_lock_)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_remove_first_thread(thread: *mut Thread) {
    // SAFETY: Forwarded from the caller.
    unsafe { Scheduler::remove_first_thread(thread) };
}

/// Migrates unpinned threads off the current CPU's `sched`.
///
/// # Safety
/// `sched` must be the current CPU's valid, initialized `Scheduler`; interrupts
/// must be disabled, and no `ChainLockTransaction` or `queue_lock_` may be held
/// (`TA_EXCL(chainlock_transaction_token, queue_lock_)`).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_kernel_scheduler_migrate_unpinned_threads(sched: *mut Scheduler) {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).migrate_unpinned_threads() };
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
/// `sched` must be valid and its `queue_lock_` must NOT be held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_request_power_level_for_testing(
    sched: *mut Scheduler,
    power_level: u8,
) -> bool {
    // SAFETY: Forwarded from the caller.
    unsafe { (*sched).request_power_level_for_testing(power_level) }
}

/// # Safety
/// `info` must point to `count` valid `zx_cpu_performance_info_t` elements.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_scheduler_update_processing_rates(
    info: *mut zx_types::zx_cpu_performance_info_t,
    count: usize,
) {
    // SAFETY: When `count > 0`, the caller guarantees `info` points to `count` valid elements.
    let slice =
        if count == 0 { &mut [] } else { unsafe { core::slice::from_raw_parts_mut(info, count) } };
    // SAFETY: Per-CPU schedulers are initialized and `queue_lock_` is not held by the caller.
    unsafe { Scheduler::update_processing_rates(slice) };
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
pub unsafe extern "C" fn rust_scheduler_update_processing_limits(
    limits: *const zx_types::zx_cpu_perf_limit_t,
    count: usize,
) {
    // SAFETY: When `count > 0`, the caller guarantees `limits` points to `count` valid elements.
    let slice =
        if count == 0 { &[] } else { unsafe { core::slice::from_raw_parts(limits, count) } };
    // SAFETY: Per-CPU schedulers are initialized and `queue_lock_` is not held by the caller.
    unsafe { Scheduler::update_processing_limits(slice) };
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
#[inline(always)]
pub unsafe fn unblock(thread: OwnerGuard<'_, Thread>, clt_token: &ChainLockTransactionToken) {
    assert_transaction_active(clt_token);
    let thread = thread.into_unguarded();
    // SAFETY: `thread.lock()` is held via the consumed `OwnerGuard` in the active transaction,
    // and the caller guarantees `thread` is ready to be unblocked in the scheduler.
    unsafe {
        unblock_raw(thread.as_raw());
    }
}

/// Like [`unblock`] (consumes the thread's guard, releasing its lock), but uses target CPU
/// selection criteria that enable a "synchronous wake", if possible; local rescheduling is
/// deferred rather than performed here.
///
/// # Safety
///
/// As [`unblock`].
#[inline(always)]
pub unsafe fn unblock_synchronous(
    thread: OwnerGuard<'_, Thread>,
    clt_token: &ChainLockTransactionToken,
) {
    assert_transaction_active(clt_token);
    let thread = thread.into_unguarded();
    // SAFETY: `thread.lock()` is held via the consumed `OwnerGuard` in the active transaction,
    // and the caller guarantees `thread` is ready to be unblocked in the scheduler.
    unsafe {
        unblock_synchronous_raw(thread.as_raw());
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
    // ready to be unblocked in the scheduler, and `unblock_list_raw` pops the threads out of
    // `list` in-place, leaving `list` as a valid empty `Thread::UnblockList`.
    unsafe {
        unblock_list_raw(list.as_raw().cast());
    }
}

/// A reference to a node in a PI graph, which is either a [`Thread`] or an [`OwnedWaitQueue`].
#[derive(Clone, Copy)]
pub enum PiNodeRef<'a> {
    /// A [`Thread`] node.
    Thread(&'a Thread),
    /// An [`OwnedWaitQueue`] node.
    OwnedWaitQueue(&'a OwnedWaitQueue),
}

/// Implemented by the types which may be nodes in a PI graph ([`Thread`] and [`OwnedWaitQueue`]).
///
/// # Safety
///
/// [`PiNode::pi_node_ref`] must return a reference to `self` (and [`PiNode::IS_THREAD`] must be
/// `true` exactly when it returns [`PiNodeRef::Thread`]); unsafe code relies on this to operate
/// on the nodes whose locks its callers hold.
pub unsafe trait PiNode {
    /// `true` if `Self` is [`Thread`].
    const IS_THREAD: bool;

    /// Returns `self` as a [`PiNodeRef`].
    fn pi_node_ref(&self) -> PiNodeRef<'_>;
}

// SAFETY: `pi_node_ref` returns `self` as a `PiNodeRef::Thread`.
unsafe impl PiNode for Thread {
    const IS_THREAD: bool = true;

    #[inline]
    fn pi_node_ref(&self) -> PiNodeRef<'_> {
        PiNodeRef::Thread(self)
    }
}

// SAFETY: `pi_node_ref` returns `self` as a `PiNodeRef::OwnedWaitQueue`.
unsafe impl PiNode for OwnedWaitQueue {
    const IS_THREAD: bool = false;

    #[inline]
    fn pi_node_ref(&self) -> PiNodeRef<'_> {
        PiNodeRef::OwnedWaitQueue(self)
    }
}

/// Updates a thread's effective profile and position in its container at the start of a base
/// profile update operation, regardless of whether or not the target thread is currently blocked
/// or currently assigned to a scheduler.
///
/// # Safety
///
/// The caller must ensure that `thread`'s lock, and every lock in the PI chain downstream of
/// `thread`, is held by the active transaction, that preemption is disabled, and that `thread`'s
/// base profile has just been changed and marked dirty.
#[inline]
pub unsafe fn thread_base_profile_changed(thread: &Thread, clt_token: &ChainLockTransactionToken) {
    assert_transaction_active(clt_token);
    // SAFETY: The caller guarantees `thread`'s PI chain is locked and its base profile is dirty.
    unsafe {
        rust_kernel_scheduler_pi_thread_base_profile_changed(thread.as_raw());
    }
}

/// Called when a thread in a graph whose target is either an [`OwnedWaitQueue`] or a different
/// [`Thread`] changes its base profile in order update the target's new effective profile,
/// position in container, and dynamic scheduling parameters.
///
/// # Safety
///
/// The caller must ensure that the locks of `upstream`, `target`, and every node in the PI chain
/// downstream of `target` are held by the active transaction, and that preemption is disabled.
#[inline]
pub unsafe fn upstream_thread_base_profile_changed<T: PiNode>(
    upstream: &Thread,
    target: &T,
    clt_token: &ChainLockTransactionToken,
) {
    assert_transaction_active(clt_token);
    // SAFETY: The caller guarantees that the required PI chain locks are held. The `*mut` derived
    // from `&OwnedWaitQueue` is only written through `UnsafeCell`-backed fields (the `KCell`
    // collection) or through the thread-owned inherited scheduler state storage.
    unsafe {
        match target.pi_node_ref() {
            PiNodeRef::Thread(t) => {
                rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_thread(
                    upstream,
                    t.as_raw(),
                )
            }
            PiNodeRef::OwnedWaitQueue(t) => {
                rust_kernel_scheduler_pi_upstream_thread_base_profile_changed_owq(
                    upstream,
                    core::ptr::from_ref(t).cast_mut(),
                )
            }
        }
    }
}

/// Called when a new edge is added connecting the target of one PI graph (the upstream node) to a
/// different PI graph.
///
/// # Safety
///
/// The caller must ensure that the locks of `upstream`, `target`, and every node in the PI chain
/// downstream of `target` are held by the active transaction, and that preemption is disabled.
/// (The transaction need not be finalized; `OwnedWaitQueue::assign_owner` propagates before
/// finalizing, as in C++.)
#[inline]
pub unsafe fn join_node_to_pi_graph<U: PiNode, T: PiNode>(
    upstream: &U,
    target: &T,
    force_inheritance: super::wait_queue::ForceInheritance,
    clt_token: &ChainLockTransactionToken,
) {
    assert_transaction_active(clt_token);
    // SAFETY: The caller guarantees that the required PI chain locks are held. Each `*mut`
    // derived from `&OwnedWaitQueue` is only written through `UnsafeCell`-backed fields
    // (the `KCell` collection) or through the thread-owned inherited scheduler state storage.
    unsafe {
        match (upstream.pi_node_ref(), target.pi_node_ref()) {
            (PiNodeRef::Thread(u), PiNodeRef::Thread(t)) => {
                rust_kernel_scheduler_pi_join_thread_thread(u, t.as_raw(), force_inheritance)
            }
            (PiNodeRef::Thread(u), PiNodeRef::OwnedWaitQueue(t)) => {
                rust_kernel_scheduler_pi_join_thread_owq(
                    u,
                    core::ptr::from_ref(t).cast_mut(),
                    force_inheritance,
                )
            }
            (PiNodeRef::OwnedWaitQueue(u), PiNodeRef::Thread(t)) => {
                rust_kernel_scheduler_pi_join_owq_thread(u, t.as_raw(), force_inheritance)
            }
            (PiNodeRef::OwnedWaitQueue(u), PiNodeRef::OwnedWaitQueue(t)) => {
                rust_kernel_scheduler_pi_join_owq_owq(
                    u,
                    core::ptr::from_ref(t).cast_mut(),
                    force_inheritance,
                )
            }
        }
    }
}

/// Called when an upstream node has its downstream edge removed, splitting it from the PI graph it
/// was a member of and becoming the target of a new graph in the process.
///
/// # Safety
///
/// The caller must ensure that the locks of `upstream`, `target`, and every node in the PI chain
/// downstream of `target` are held by the active transaction, and that preemption is disabled.
/// (The transaction need not be finalized; `OwnedWaitQueue::assign_owner` propagates before
/// finalizing, as in C++.)
#[inline]
pub unsafe fn split_node_from_pi_graph<U: PiNode, T: PiNode>(
    upstream: &U,
    target: &T,
    old_target_ep: Option<&EffectiveProfile>,
    clt_token: &ChainLockTransactionToken,
) {
    assert_transaction_active(clt_token);
    let old_target_ep = old_target_ep.map_or(core::ptr::null(), core::ptr::from_ref);
    // SAFETY: The caller guarantees that the required PI chain locks are held. Each `*mut`
    // derived from `&OwnedWaitQueue` is only written through `UnsafeCell`-backed fields
    // (the `KCell` collection) or through the thread-owned inherited scheduler state storage
    // (for example, the upstream queue's dynamic scheduling parameters).
    unsafe {
        match (upstream.pi_node_ref(), target.pi_node_ref()) {
            (PiNodeRef::Thread(u), PiNodeRef::Thread(t)) => {
                debug_assert!(old_target_ep.is_null());
                rust_kernel_scheduler_pi_split_thread_thread(u.as_raw(), t.as_raw())
            }
            (PiNodeRef::Thread(u), PiNodeRef::OwnedWaitQueue(t)) => {
                rust_kernel_scheduler_pi_split_thread_owq(
                    u.as_raw(),
                    core::ptr::from_ref(t).cast_mut(),
                    old_target_ep,
                )
            }
            (PiNodeRef::OwnedWaitQueue(u), PiNodeRef::Thread(t)) => {
                debug_assert!(old_target_ep.is_null());
                rust_kernel_scheduler_pi_split_owq_thread(
                    core::ptr::from_ref(u).cast_mut(),
                    t.as_raw(),
                )
            }
            (PiNodeRef::OwnedWaitQueue(u), PiNodeRef::OwnedWaitQueue(t)) => {
                rust_kernel_scheduler_pi_split_owq_owq(
                    core::ptr::from_ref(u).cast_mut(),
                    core::ptr::from_ref(t).cast_mut(),
                    old_target_ep,
                )
            }
        }
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
