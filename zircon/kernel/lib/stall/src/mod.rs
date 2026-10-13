// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Memory pressure stall accounting.
//!
//! Every CPU owns a [`StallAccumulator`], stored inside its `struct percpu`, which tracks how much
//! time that CPU spent running stalling and progressing threads. `StallAggregator` periodically
//! flushes them and turns them into the system-wide stats reported by `ZX_INFO_MEMORY_STALL`.

use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
use crate::kernel::percpu::PerCpu;
use crate::kernel::thread::{self, Thread, ThreadStallState};
use crate::kernel::types::{INVALID_CPU, cpu_num_t};
use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{DoublyLinkedList, DoublyLinkedListContainable, DoublyLinkedListNode, UniquePtr};
use pin_init::{InPlaceWrite as _, PinInit, pin_data, pin_init};
use zx_status::Status;
use zx_types::{zx_duration_mono_t, zx_status_t};

unsafe extern "C" {
    fn cpp_stall_observer_event_receiver_on_above_threshold(this: *mut c_void);
    fn cpp_stall_observer_event_receiver_on_below_threshold(this: *mut c_void);
}

/// The interval at which the sampling thread aggregates the per-CPU accumulators, i.e. the
/// duration that one sample covers.
pub const STALL_SAMPLE_INTERVAL: DurationMono = DurationMono::from_millis(10);

/// Stall measurements accumulated by a [`StallAccumulator`] since the last flush.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AccumulatorStats {
    /// Monotonic time spent with `num_contributors_stalling > 0`.
    pub total_time_stall_some: DurationMono,

    /// Monotonic time spent with `num_contributors_stalling > 0 &&
    /// num_contributors_progressing == 0`.
    pub total_time_stall_full: DurationMono,

    /// Monotonic time spent with `num_contributors_progressing > 0 ||
    /// num_contributors_stalling > 0`.
    pub total_time_active: DurationMono,
}

/// Maintains per-CPU stall timers in real time.
///
/// Its counters are conceptually continuous. In fact, we update the saved state whenever the
/// conditions change, so we can always compute the current value by extrapolating.
///
/// With respect to stall contributions, threads are always in one of these three states:
///  - Not contributing at all to any accumulator.
///  - Contributing to an accumulator as a progressing thread.
///  - Contributing to an accumulator as a stalling thread.
#[ksync::guarded]
#[pin_data]
#[repr(C)]
pub struct StallAccumulator {
    #[mutex]
    lock: ksync::KMutex<ksync::RawSpinlock>,

    /// Number of progressing threads currently tracked by this structure.
    #[guarded_by(lock)]
    num_contributors_progressing: usize,

    /// Number of stalling threads currently tracked by this structure.
    #[guarded_by(lock)]
    num_contributors_stalling: usize,

    /// Timestamp of the last `consolidate` call.
    #[guarded_by(lock)]
    last_consolidate_time: InstantMono,

    /// Accumulated totals at the time of the last update.
    #[guarded_by(lock)]
    accumulated_stats: AccumulatorStats,
}

fn consolidate(
    num_contributors_progressing: usize,
    num_contributors_stalling: usize,
    last_consolidate_time: &mut InstantMono,
    accumulated_stats: &mut AccumulatorStats,
) {
    let now = current_mono_time();
    let time_delta = now - *last_consolidate_time;

    if num_contributors_stalling > 0 {
        accumulated_stats.total_time_stall_some =
            accumulated_stats.total_time_stall_some + time_delta;
    }

    if num_contributors_stalling > 0 && num_contributors_progressing == 0 {
        accumulated_stats.total_time_stall_full =
            accumulated_stats.total_time_stall_full + time_delta;
    }

    if num_contributors_progressing > 0 || num_contributors_stalling > 0 {
        accumulated_stats.total_time_active = accumulated_stats.total_time_active + time_delta;
    }

    *last_consolidate_time = now;
}

impl StallAccumulator {
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            lock <- ksync::KMutex::init(),
            num_contributors_progressing: 0usize.into(),
            num_contributors_stalling: 0usize.into(),
            last_consolidate_time: InstantMono::ZERO.into(),
            accumulated_stats: AccumulatorStats::default().into(),
        })
    }

    fn update_with_irq_disabled(
        &self,
        op_contributors_progressing: i32,
        op_contributors_stalling: i32,
    ) {
        // Check argument range.
        debug_assert!((-1..=1).contains(&op_contributors_progressing));
        debug_assert!((-1..=1).contains(&op_contributors_stalling));

        ksync::lock!(let mut guard = self.lock_lock_policy::<ksync::NoIrqSavePolicy>());
        let fields = guard.as_mut().fields_mut();
        consolidate(
            *fields.num_contributors_progressing,
            *fields.num_contributors_stalling,
            fields.last_consolidate_time,
            fields.accumulated_stats,
        );

        // Apply variations.
        *fields.num_contributors_progressing = fields
            .num_contributors_progressing
            .wrapping_add_signed(op_contributors_progressing as isize);
        *fields.num_contributors_stalling =
            fields.num_contributors_stalling.wrapping_add_signed(op_contributors_stalling as isize);

        // Check that we are counting correctly and we never decrement below zero.
        debug_assert_ne!(*fields.num_contributors_progressing, usize::MAX);
        debug_assert_ne!(*fields.num_contributors_stalling, usize::MAX);
    }

    /// Alter contributor counts by the given amount.
    ///
    /// Only values between -1 and +1 are accepted.
    pub fn update(&self, op_contributors_progressing: i32, op_contributors_stalling: i32) {
        let _guard = InterruptDisableGuard::new();
        self.update_with_irq_disabled(op_contributors_progressing, op_contributors_stalling);
    }

    /// Reads the current stats and resets them to zero.
    pub fn flush(&self) -> AccumulatorStats {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();
        consolidate(
            *fields.num_contributors_progressing,
            *fields.num_contributors_stalling,
            fields.last_consolidate_time,
            fields.accumulated_stats,
        );

        core::mem::take(fields.accumulated_stats)
    }
}

/// The stall states of the two threads involved in a context switch, after
/// [`apply_context_switch_states`] has accounted for them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ContextSwitchStallStates {
    /// The outgoing thread's stall state.
    current_thread: ThreadStallState,

    /// The incoming thread's stall state.
    next_thread: ThreadStallState,
}

/// Works out how a context switch moves the two threads' stall contributions.
///
/// `next_thread_last_cpu` is the incoming thread's `SchedulerState::last_cpu()`, and
/// `current_cpu` is the CPU the switch is happening on. This is the part of
/// [`apply_context_switch`] that does not touch the threads themselves, so that it can be tested
/// without any.
#[inline]
fn apply_context_switch_states(
    current_thread_state: ThreadStallState,
    next_thread_state: ThreadStallState,
    next_thread_last_cpu: cpu_num_t,
    current_cpu: cpu_num_t,
) -> ContextSwitchStallStates {
    let mut local_op_progressing = 0;
    let mut local_op_stalling = 0;

    let mut updated = ContextSwitchStallStates {
        current_thread: current_thread_state,
        next_thread: next_thread_state,
    };

    // If the current thread is not stalling, remove it from the number of progressing threads.
    if current_thread_state == ThreadStallState::Progressing {
        updated.current_thread = ThreadStallState::Inactive;
        local_op_progressing -= 1;
    }

    // If the next thread has been migrated while stalling, move its stall to the current CPU.
    if next_thread_state == ThreadStallState::Stalling && next_thread_last_cpu != current_cpu {
        // Unmark the thread as stalling on its last CPU.
        debug_assert!(
            next_thread_last_cpu != INVALID_CPU,
            "Stalling threads must have run at least once"
        );
        let last_accumulator = &PerCpu::get(next_thread_last_cpu).memory_stall_accumulator;
        last_accumulator.update_with_irq_disabled(0, -1);

        // We now have new a stalling thread tied to the current_cpu.
        local_op_stalling += 1;
    }

    // If the next thread is not stalling, add it to the local counter.
    if next_thread_state == ThreadStallState::Inactive {
        updated.next_thread = ThreadStallState::Progressing;
        local_op_progressing += 1;
    }

    // Propagate changes (we can skip this to be faster if there are no changes).
    if local_op_progressing != 0 || local_op_stalling != 0 {
        let local_accumulator = &PerCpu::get(current_cpu).memory_stall_accumulator;
        local_accumulator.update_with_irq_disabled(local_op_progressing, local_op_stalling);
    }

    updated
}

/// Moves the stall contributions of the outgoing and incoming threads across a context switch.
///
/// Internally called by the scheduler at every context switch.
///
/// # Safety
///
/// - `current_thread` and `next_thread` must point to valid, initialized, distinct `Thread`
///   instances that remain alive for the duration of the call, and the caller must hold both of
///   their locks (matching `StallAccumulator::ApplyContextSwitch`'s `TA_REQ`).
/// - Interrupts (and thus preemption) must be disabled on the calling CPU, with exclusive access
///   to both threads' `memory_stall_state_`.
/// - `next_thread` must be the thread being switched to on the calling CPU.
#[inline]
pub unsafe fn apply_context_switch(current_thread: *mut Thread, next_thread: *mut Thread) {
    let current_cpu = curr_cpu_num();

    // SAFETY: The caller guarantees `current_thread` and `next_thread` are valid, live `Thread`s,
    // holds both of their locks, and has interrupts/preemption disabled with no concurrent writes
    // to `memory_stall_state_`.
    let (current_thread_state, next_thread_state, next_thread_last_cpu) = unsafe {
        let next_state = thread::scheduler_state(next_thread);
        debug_assert_eq!(next_state.curr_cpu(), current_cpu);
        (
            thread::memory_stall_state(current_thread),
            thread::memory_stall_state(next_thread),
            next_state.last_cpu(),
        )
    };

    let updated = apply_context_switch_states(
        current_thread_state,
        next_thread_state,
        next_thread_last_cpu,
        current_cpu,
    );

    // SAFETY: The caller guarantees `current_thread` and `next_thread` are valid, live, distinct
    // `Thread`s and has interrupts/preemption disabled with exclusive access to both threads'
    // `memory_stall_state_`.
    unsafe {
        if updated.current_thread != current_thread_state {
            thread::set_memory_stall_state(current_thread, updated.current_thread);
        }
        if updated.next_thread != next_thread_state {
            thread::set_memory_stall_state(next_thread, updated.next_thread);
        }
    }
}

/// Accounts for a context switch between two threads. See [`apply_context_switch`].
///
/// # Safety
///
/// See [`apply_context_switch`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_apply_context_switch(
    current_thread: *mut Thread,
    next_thread: *mut Thread,
) {
    // SAFETY: The caller guarantees all safety preconditions of `apply_context_switch`.
    unsafe { apply_context_switch(current_thread, next_thread) };
}

/// Initializes the `StallAccumulator` held by a `struct percpu`.
///
/// # Safety
///
/// `accumulator` must point to writable storage that is correctly sized and aligned for a
/// `StallAccumulator`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_accumulator_init(
    accumulator: *mut MaybeUninit<StallAccumulator>,
) {
    // SAFETY: accumulator is a correctly sized and aligned ptr.
    unsafe {
        let Ok(_) = accumulator.as_mut_unchecked().write_pin_init(StallAccumulator::init());
    }
}

/// Alters the accumulator's contributor counts by the given amount.
#[unsafe(no_mangle)]
pub extern "C" fn rust_stall_accumulator_update(
    accumulator: &StallAccumulator,
    op_contributors_progressing: i32,
    op_contributors_stalling: i32,
) {
    accumulator.update(op_contributors_progressing, op_contributors_stalling);
}

/// Receives notifications when a [`StallObserver`] crosses its configured threshold.
pub trait EventReceiver {
    /// Called when the sum of the stored samples reached or exceeded the threshold.
    fn on_above_threshold(&self);

    /// Called when the sum of the stored samples fell below the threshold.
    fn on_below_threshold(&self);
}

pub struct CppEventReceiver;

impl EventReceiver for CppEventReceiver {
    fn on_above_threshold(&self) {
        unsafe {
            cpp_stall_observer_event_receiver_on_above_threshold(self as *const Self as *mut c_void)
        }
    }
    fn on_below_threshold(&self) {
        unsafe {
            cpp_stall_observer_event_receiver_on_below_threshold(self as *const Self as *mut c_void)
        }
    }
}

/// A stall observer that keeps a circular queue with the last N samples (where N corresponds to
/// the number of samples covering the requested time window).
///
/// Every time a new sample is pushed into it, it tests if the sum of all the stored samples is
/// greater than or equal to the given threshold value and then notifies its callback function
/// accordingly.
///
/// This type doesn't need to be thread safe because all its usages are protected by the
/// [`StallAggregator`]'s `observers_lock`.
#[derive(DoublyLinkedListContainable, fbl::Recyclable)]
#[pin_data]
pub struct StallObserver {
    #[pin]
    #[dll_node]
    node: DoublyLinkedListNode<StallObserver>,

    threshold: DurationMono,
    event_receiver: NonNull<dyn EventReceiver>,

    /// Circular queue of samples.
    samples: fbl::Array<DurationMono>,
    samples_pos: usize,
    /// Cached sum of all `samples` in the queue.
    samples_sum: DurationMono,
}

impl StallObserver {
    /// Creates an observer that fires when the stall time over `window` reaches `threshold`.
    ///
    /// # Safety
    ///
    /// `event_receiver` must outlive the returned observer.
    pub unsafe fn create(
        threshold: DurationMono,
        window: DurationMono,
        event_receiver: NonNull<dyn EventReceiver>,
    ) -> Result<UniquePtr<Self>, Status> {
        if window <= DurationMono::ZERO || threshold <= DurationMono::ZERO || threshold > window {
            return Err(Status::INVALID_ARGS);
        }

        let samples_size = (window.into_nanos() + STALL_SAMPLE_INTERVAL.into_nanos() - 1)
            / STALL_SAMPLE_INTERVAL.into_nanos();
        let samples = fbl::Array::try_new(samples_size as usize).map_err(|_| Status::NO_MEMORY)?;

        UniquePtr::try_new(Self {
            node: DoublyLinkedListNode::new(),
            threshold,
            event_receiver,
            samples,
            samples_pos: 0,
            samples_sum: DurationMono::ZERO,
        })
        .map_err(|_| Status::NO_MEMORY)
    }

    /// Stores `sample`, evicting the oldest one, and notifies the event receiver.
    pub fn push_sample(self: Pin<&mut Self>, sample: DurationMono) {
        let this = self.project();
        *this.samples_sum = *this.samples_sum + (sample - this.samples[*this.samples_pos]);
        this.samples[*this.samples_pos] = sample;
        *this.samples_pos += 1;
        if *this.samples_pos == this.samples.len() {
            *this.samples_pos = 0;
        }

        // SAFETY: the observer's creator guarantees that the receiver outlives it, and removing
        // an observer from the aggregator before dropping it is what keeps the sampling thread
        // from being inside this call when that happens.
        let event_receiver = unsafe { this.event_receiver.as_ref() };
        if *this.samples_sum >= *this.threshold {
            event_receiver.on_above_threshold();
        } else {
            event_receiver.on_below_threshold();
        }
    }
}

/// Aggregated system-wide stall stats.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AggregatorStats {
    /// Total monotonic time spent with at least one memory-stalled thread.
    pub stalled_time_some: DurationMono,

    /// Total monotonic time spent with all threads memory-stalled.
    pub stalled_time_full: DurationMono,
}

/// The two observer lists, which the aggregator's `observers_lock` guards together.
///
/// The lists are non-owning: an observer is owned by whoever created it and must be removed from
/// here before being destroyed. `sample_once` holds `observers_lock` for the whole notification
/// loop, so removal under that lock is also what guarantees the sampling thread is not part way
/// through a callback when the observer dies.
#[pin_data]
struct Observers {
    #[pin]
    some: DoublyLinkedList<*mut StallObserver>,
    #[pin]
    full: DoublyLinkedList<*mut StallObserver>,
}

/// Maintains system-wide stall stats by periodically aggregating measurements from per-CPU
/// [`StallAccumulator`]s.
#[ksync::guarded]
#[pin_data]
pub struct StallAggregator {
    #[mutex]
    stats_lock: ksync::KMutex<ksync::RawCriticalMutex>,
    #[guarded_by(stats_lock)]
    stats: AggregatorStats,

    #[mutex]
    observers_lock: ksync::KMutex<ksync::RawCriticalMutex>,
    #[guarded_by(observers_lock)]
    #[pin]
    observers: Observers,
}

// SAFETY: The observer pointers held by `Observers`' lists are only ever reachable with the
// aggregator's `observers_lock` held, and the observers themselves are owned by whoever registered
// them. Ownership of the lists can therefore be moved between threads, which is what the C++
// implementation does by placing a `StallAggregator` in shared storage and serializing all access
// on its `DECLARE_CRITICAL_MUTEX`.
//
// Note that this makes the sampling thread call `push_sample`, and through it the observer's
// `EventReceiverRef` callbacks, on a thread other than the one that registered the observer. The
// only production receiver is `MemoryStallEventDispatcher`, whose `UpdateState` takes the
// dispatcher's lock, so that is sound.
unsafe impl Send for Observers {}

/// The singleton instance, constructed by `init_stall_aggregator` during boot.
static STALL_AGGREGATOR: lazy_init::LazyInit<StallAggregator> = lazy_init::LazyInit::uninit();

/// Flushes every CPU's accumulator and reports the measurements it collected.
fn iterate_per_cpu_stats(callback: &mut dyn FnMut(&AccumulatorStats)) {
    PerCpu::for_each(|_cpu_num, cpu_data| {
        let stats = cpu_data.memory_stall_accumulator.flush();
        callback(&stats);
    });
}

impl StallAggregator {
    /// Creates a new aggregator with zeroed stats and no observers.
    pub fn init() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            stats_lock <- ksync::KMutex::init(),
            stats: AggregatorStats::default().into(),
            observers_lock <- ksync::KMutex::init(),
            observers <- ksync::kcell_init(pin_init!(Observers {
                some <- DoublyLinkedList::new(),
                full <- DoublyLinkedList::new(),
            })),
        })
    }

    /// Gets this type's singleton instance.
    pub fn get() -> &'static Self {
        &STALL_AGGREGATOR
    }

    /// Returns the values of the aggregated stats.
    pub fn read_stats(&self) -> AggregatorStats {
        ksync::lock!(let guard = self.lock_stats_lock());
        *guard.stats()
    }

    /// Runs `func` on the observer lists with `observers_lock` held.
    fn with_observers<R>(&self, func: impl FnOnce(Pin<&mut Observers>) -> R) -> R {
        ksync::lock!(let mut guard = self.lock_observers_lock());
        func(guard.as_mut().observers_mut())
    }

    /// Registers `observer` to be notified of the "some" stall time.
    ///
    /// # Safety
    ///
    /// `observer` must point at a live observer that is not in any list, and it must be passed to
    /// [`Self::remove_observer_some`] before it is destroyed.
    pub unsafe fn add_observer_some(&self, observer: NonNull<StallObserver>) {
        // SAFETY: Guaranteed by this function's contract.
        self.with_observers(|observers| unsafe {
            observers.project().some.push_back_raw(observer.as_ptr())
        });
    }

    /// Unregisters an observer previously passed to [`Self::add_observer_some`].
    ///
    /// # Safety
    ///
    /// `observer` must currently be registered in the "some" list. Passing one that is registered
    /// in the "full" list instead would unlink it through its own neighbours and leave that list's
    /// head dangling.
    pub unsafe fn remove_observer_some(&self, observer: &StallObserver) {
        // SAFETY: Guaranteed by this function's contract.
        self.with_observers(|observers| unsafe { observers.project().some.erase(observer) });
    }

    /// Registers `observer` to be notified of the "full" stall time.
    ///
    /// # Safety
    ///
    /// `observer` must point at a live observer that is not in any list, and it must be passed to
    /// [`Self::remove_observer_full`] before it is destroyed.
    pub unsafe fn add_observer_full(&self, observer: NonNull<StallObserver>) {
        // SAFETY: Guaranteed by this function's contract.
        self.with_observers(|observers| unsafe {
            observers.project().full.push_back_raw(observer.as_ptr())
        });
    }

    /// Unregisters an observer previously passed to [`Self::add_observer_full`].
    ///
    /// # Safety
    ///
    /// `observer` must currently be registered in the "full" list. Passing one that is registered
    /// in the "some" list instead would unlink it through its own neighbours and leave that list's
    /// head dangling.
    pub unsafe fn remove_observer_full(&self, observer: &StallObserver) {
        // SAFETY: Guaranteed by this function's contract.
        self.with_observers(|observers| unsafe { observers.project().full.erase(observer) });
    }

    /// Aggregates a new set of per-CPU samples. Called periodically by the sampling thread.
    fn sample_once(&self) {
        self.sample_once_with(iterate_per_cpu_stats);
    }

    /// Aggregates a new set of per-CPU samples.
    ///
    /// The `iterate_per_cpu_stats` argument is a function that takes a callback and calls it for
    /// each CPU, passing its per-CPU measurements collected since the previous call. It's always
    /// [`iterate_per_cpu_stats`] at runtime, except for tests that inject a custom fake data
    /// provider.
    fn sample_once_with(
        &self,
        iterate_per_cpu_stats: impl FnOnce(&mut dyn FnMut(&AccumulatorStats)),
    ) {
        // Aggregate stats from all CPUs.
        //
        // The arithmetic saturates rather than wrapping: a CPU that goes more than a few seconds
        // between samples - which the low priority sampling thread can, under load - overflows the
        // i64 product. The C++ wrapped around silently, which turns a large stall into a negative
        // one, and wrapping is a panic in a kernel built with overflow checks. Saturating keeps
        // the reported stall large instead. A 128-bit intermediate is not an option because the
        // kernel does not link compiler-rt's 128-bit division helpers.
        let mut weighted_some: i64 = 0;
        let mut weighted_full: i64 = 0;
        let mut total_weight: i64 = 0;
        iterate_per_cpu_stats(&mut |stats: &AccumulatorStats| {
            let active = stats.total_time_active.into_nanos();
            weighted_some = weighted_some
                .saturating_add(stats.total_time_stall_some.into_nanos().saturating_mul(active));
            weighted_full = weighted_full
                .saturating_add(stats.total_time_stall_full.into_nanos().saturating_mul(active));
            total_weight = total_weight.saturating_add(active);
        });

        // Compute weighted average.
        let (delta_some, delta_full) = if total_weight != 0 {
            (
                DurationMono::from_nanos(weighted_some / total_weight),
                DurationMono::from_nanos(weighted_full / total_weight),
            )
        } else {
            (DurationMono::ZERO, DurationMono::ZERO)
        };

        // Update stored stats.
        {
            ksync::lock!(let mut guard = self.lock_stats_lock());
            let stats = guard.as_mut().stats_mut();
            stats.stalled_time_some = stats.stalled_time_some + delta_some;
            stats.stalled_time_full = stats.stalled_time_full + delta_full;
        }

        // Notify observers.
        self.with_observers(|observers| {
            let observers = observers.project();
            for observer in observers.some.iter_mut() {
                observer.push_sample(delta_some);
            }
            for observer in observers.full.iter_mut() {
                observer.push_sample(delta_full);
            }
        });
    }
}

/// Constructs the singleton aggregator.
///
/// This has to run before anything can register an observer or read the stats. The earliest such
/// caller is the object layer, whose own `libobject` hook runs at LK_INIT_LEVEL_THREADING, so
/// running one level earlier keeps the ordering independent of link order within a level.
fn init_stall_aggregator(_level: init::LkInitLevel) {
    // SAFETY: Runs once from an init hook on the primary CPU, before any other code can reach the
    // singleton, so it cannot race with a concurrent read or initialization.
    let Ok(_) = unsafe { Pin::static_ref(&STALL_AGGREGATOR).init_pin(StallAggregator::init()) };
}

init::lk_init_hook!(stall_aggregator, init_stall_aggregator, init::LK_INIT_LEVEL_KERNEL);

/// Body of the "stall-aggregator" thread: samples the per-CPU accumulators forever.
extern "C" fn sampling_thread(_arg: *mut c_void) -> i32 {
    let aggregator = StallAggregator::get();
    let mut deadline = current_mono_time();

    loop {
        aggregator.sample_once();

        deadline = deadline + STALL_SAMPLE_INTERVAL;
        let _ = thread::sleep(deadline);
    }
}

/// Starts the sampling thread on the singleton instance.
fn start_sampling_thread(_level: init::LkInitLevel) {
    // The C++ implementation used DetachAndResume(). The thread loops forever and is never
    // joined, so a plain resume is equivalent.
    //
    // SAFETY: `sampling_thread` ignores its argument and borrows nothing.
    let thread = unsafe {
        thread::create_with_priority(
            c"stall-aggregator".as_ptr(),
            sampling_thread,
            core::ptr::null_mut(),
            thread::LOW_PRIORITY,
        )
    }
    .expect("failed to create the stall-aggregator thread");

    // SAFETY: The thread was just created and has not been started yet.
    unsafe { thread.resume() };
}

init::lk_init_hook!(stall, start_sampling_thread, init::LK_INIT_LEVEL_USER);

/// Creates a stall observer for a C++ `StallObserver::EventReceiver`.
///
/// # Safety
///
/// `event_receiver` must outlive the returned observer. On success `*out_observer` receives an
/// observer that the caller owns and must eventually pass to [`rust_stall_observer_destroy`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_observer_create(
    threshold: zx_duration_mono_t,
    window: zx_duration_mono_t,
    event_receiver: NonNull<CppEventReceiver>,
    out_observer: &mut *mut StallObserver,
) -> zx_status_t {
    // SAFETY: Caller asserts that event_Receiver lives long enough.
    match unsafe {
        StallObserver::create(
            DurationMono::from_nanos(threshold),
            DurationMono::from_nanos(window),
            event_receiver,
        )
    } {
        Ok(observer) => {
            *out_observer = UniquePtr::into_raw(observer);
            zx_types::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Destroys an observer created by [`rust_stall_observer_create`].
///
/// # Safety
///
/// `observer` must have come from [`rust_stall_observer_create`], must not have been destroyed
/// already, and must have been unregistered from the aggregator first.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_observer_destroy(observer: *mut StallObserver) {
    // SAFETY: Guaranteed by this function's contract.
    drop(unsafe { UniquePtr::from_raw(observer) });
}

/// Reads the aggregated stall stats into `out_stats`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_stall_aggregator_read_stats(out_stats: &mut AggregatorStats) {
    *out_stats = StallAggregator::get().read_stats();
}

/// Registers `observer` to be notified of the "some" stall time.
///
/// # Safety
///
/// See [`StallAggregator::add_observer_some`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_aggregator_add_observer_some(observer: NonNull<StallObserver>) {
    // SAFETY: Guaranteed by this function's contract.
    unsafe { StallAggregator::get().add_observer_some(observer) };
}

/// Unregisters an observer from the "some" list.
///
/// # Safety
///
/// See [`StallAggregator::remove_observer_some`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_aggregator_remove_observer_some(observer: &StallObserver) {
    // SAFETY: Guaranteed by this function's contract.
    unsafe { StallAggregator::get().remove_observer_some(observer) };
}

/// Registers `observer` to be notified of the "full" stall time.
///
/// # Safety
///
/// See [`StallAggregator::add_observer_full`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_aggregator_add_observer_full(observer: NonNull<StallObserver>) {
    // SAFETY: Guaranteed by this function's contract.
    unsafe { StallAggregator::get().add_observer_full(observer) };
}

/// Unregisters an observer from the "full" list.
///
/// # Safety
///
/// See [`StallAggregator::remove_observer_full`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_stall_aggregator_remove_observer_full(observer: &StallObserver) {
    // SAFETY: Guaranteed by this function's contract.
    unsafe { StallAggregator::get().remove_observer_full(observer) };
}

/// Tests for memory pressure stall accounting.
#[cfg(ktest)]
#[unittest::suite(name = "stall")]
mod stall_tests {
    use super::{
        AccumulatorStats, EventReceiver, STALL_SAMPLE_INTERVAL, StallAccumulator, StallAggregator,
        StallObserver, apply_context_switch_states,
    };
    use crate::arch_rs::{InterruptDisableGuard, curr_cpu_num};
    use crate::kernel::thread::ThreadStallState;
    use crate::platform_rs::timer::DurationMono;
    use crate::top::debug::spin_usecs;
    use core::cell::Cell;
    use core::ptr::NonNull;
    use pin_init::stack_pin_init;
    use unittest::{assert_ge, expect_eq, expect_false, expect_ge, expect_le, expect_true};

    /// Verifies that non-stalling threads only generate `active` time while running.
    #[test]
    fn non_stalling_time() {
        stack_pin_init!(let accumulator = StallAccumulator::init());

        // Progressing non-stalling time should make only the `active` timer grow.
        accumulator.update(1, 0);
        spin_usecs(1_000);
        accumulator.update(-1, 0);
        let stats1 = accumulator.flush();
        expect_le!(
            DurationMono::from_millis(1).into_nanos(),
            stats1.total_time_active.into_nanos()
        );
        expect_eq!(0, stats1.total_time_stall_some.into_nanos());
        expect_eq!(0, stats1.total_time_stall_full.into_nanos());

        // Non-progressing non-stalling time should not make any timer grow.
        spin_usecs(1_000);
        let stats2 = accumulator.flush();
        expect_eq!(0, stats2.total_time_active.into_nanos());
        expect_eq!(0, stats2.total_time_stall_some.into_nanos());
        expect_eq!(0, stats2.total_time_stall_full.into_nanos());
    }

    /// Verifies that the `some` and `full` stall timers grow with stalling threads.
    #[test]
    fn stalling_time() {
        stack_pin_init!(let accumulator = StallAccumulator::init());

        // Verifies that a single stalling thread makes all the timers grow.
        // Checks: active == some == full >= 1 ms.
        accumulator.update(0, 1);
        spin_usecs(1_000);
        let stats1 = accumulator.flush();
        expect_eq!(
            stats1.total_time_active.into_nanos(),
            stats1.total_time_stall_some.into_nanos()
        );
        expect_eq!(
            stats1.total_time_stall_some.into_nanos(),
            stats1.total_time_stall_full.into_nanos()
        );
        expect_ge!(
            stats1.total_time_stall_full.into_nanos(),
            DurationMono::from_millis(1).into_nanos()
        );

        // Verifies that adding a second non-stalling thread makes `full` stop growing, but not the
        // others. Checks: active == some >= 1 ms && (some - 1 ms) >= full
        accumulator.update(1, 0);
        spin_usecs(1_000);
        let stats2 = accumulator.flush();
        expect_eq!(
            stats2.total_time_active.into_nanos(),
            stats2.total_time_stall_some.into_nanos()
        );
        assert_ge!(
            stats2.total_time_stall_some.into_nanos(),
            DurationMono::from_millis(1).into_nanos()
        );
        expect_ge!(
            (stats2.total_time_stall_some - DurationMono::from_millis(1)).into_nanos(),
            stats2.total_time_stall_full.into_nanos()
        );

        // Cleanup.
        accumulator.update(-1, -1);
    }

    /// An implementation of the `EventReceiver` trait that simply remembers the last state it
    /// received.
    struct StallObserverTestEventReceiver {
        is_above_threshold: Cell<bool>,
    }

    impl StallObserverTestEventReceiver {
        fn new() -> Self {
            Self { is_above_threshold: Cell::new(false) }
        }

        fn is_above_threshold(&self) -> bool {
            self.is_above_threshold.get()
        }
    }

    impl EventReceiver for StallObserverTestEventReceiver {
        fn on_above_threshold(&self) {
            self.is_above_threshold.set(true);
        }

        fn on_below_threshold(&self) {
            self.is_above_threshold.set(false);
        }
    }

    /// Creates an observer for `event_receiver`.
    ///
    /// # Safety
    ///
    /// `event_receiver` must outlive the returned observer, which every caller here arranges by
    /// unregistering the observer before the receiver goes out of scope.
    unsafe fn create_stall_observer_for_test(
        threshold: DurationMono,
        window: DurationMono,
        event_receiver: &StallObserverTestEventReceiver,
    ) -> UniquePtr<StallObserver> {
        // SAFETY: Guaranteed by this function's contract.
        unsafe { StallObserver::create(threshold, window, NonNull::from_ref(event_receiver)) }
            .expect("failed to create the test stall observer")
    }

    /// Verifies that the aggregated stats are the `active`-weighted average of per-CPU stats.
    #[test]
    fn multi_cpu_merge() {
        // Simulate a two-CPU system in which the second CPU has 3 times the `active` time of the
        // first one.
        stack_pin_init!(let aggregator = StallAggregator::init());
        aggregator.sample_once_with(|callback| {
            callback(&AccumulatorStats {
                total_time_stall_some: DurationMono::from_nanos(3000),
                total_time_stall_full: DurationMono::from_nanos(300),
                total_time_active: DurationMono::from_nanos(10000),
            });
            callback(&AccumulatorStats {
                total_time_stall_some: DurationMono::from_nanos(4000),
                total_time_stall_full: DurationMono::from_nanos(400),
                total_time_active: DurationMono::from_nanos(30000),
            });
        });
        let stats = aggregator.read_stats();
        // The weights are the two `active` times, 10000 and 30000, i.e. a 1:3 ratio.
        expect_eq!((3000 + 4000 * 3) / 4, stats.stalled_time_some.into_nanos());
        expect_eq!((300 + 400 * 3) / 4, stats.stalled_time_full.into_nanos());
    }

    /// Verifies that the global stats are the running total of what a single CPU reports.
    #[test]
    fn timers_do_not_restart() {
        stack_pin_init!(let aggregator = StallAggregator::init());

        aggregator.sample_once_with(|callback| {
            callback(&AccumulatorStats {
                total_time_stall_some: DurationMono::from_nanos(200),
                total_time_stall_full: DurationMono::from_nanos(150),
                total_time_active: DurationMono::from_nanos(1000),
            });
        });
        let stats1 = aggregator.read_stats();
        expect_eq!(200, stats1.stalled_time_some.into_nanos());
        expect_eq!(150, stats1.stalled_time_full.into_nanos());

        aggregator.sample_once_with(|callback| {
            callback(&AccumulatorStats {
                total_time_stall_some: DurationMono::from_nanos(500),
                total_time_stall_full: DurationMono::from_nanos(10),
                total_time_active: DurationMono::from_nanos(4000),
            });
        });
        let stats2 = aggregator.read_stats();
        expect_eq!(200 + 500, stats2.stalled_time_some.into_nanos());
        expect_eq!(150 + 10, stats2.stalled_time_full.into_nanos());
    }

    /// Verifies that a zero overall `active` time leaves the stall timers unchanged.
    #[test]
    fn idle_system() {
        stack_pin_init!(let aggregator = StallAggregator::init());

        // Simulate two CPUs, both totally idle.
        aggregator.sample_once_with(|callback| {
            callback(&AccumulatorStats {
                total_time_stall_some: DurationMono::ZERO,
                total_time_stall_full: DurationMono::ZERO,
                total_time_active: DurationMono::ZERO,
            });
            callback(&AccumulatorStats {
                total_time_stall_some: DurationMono::ZERO,
                total_time_stall_full: DurationMono::ZERO,
                total_time_active: DurationMono::ZERO,
            });
        });
        let stats = aggregator.read_stats();
        expect_eq!(0, stats.stalled_time_some.into_nanos());
        expect_eq!(0, stats.stalled_time_full.into_nanos());
    }

    /// Which of the two stall counters `observer_threshold` exercises.
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum ObserverThresholdTestSelector {
        Some,
        Full,
    }

    /// Drives the StallObserver through the threshold sequence with data from the StallAggregator,
    /// using either the "some" or the "full" counter, and reports whether the observer was
    /// signalled after each step.
    fn observer_threshold_sequence(test_variant: ObserverThresholdTestSelector) -> [bool; 6] {
        stack_pin_init!(let aggregator = StallAggregator::init());

        // Create an observer with:
        // - threshold = 50% of the duration of one sample
        // - window = 4 samples
        let receiver = StallObserverTestEventReceiver::new();
        let sample_half = DurationMono::from_nanos(STALL_SAMPLE_INTERVAL.into_nanos() / 2);
        let sample_quarter = DurationMono::from_nanos(STALL_SAMPLE_INTERVAL.into_nanos() / 4);
        // SAFETY: `cleanup` unregisters the observer, destroying it, before `receiver` goes out
        // of scope.
        let mut observer = unsafe {
            create_stall_observer_for_test(sample_half, STALL_SAMPLE_INTERVAL * 4, &receiver)
        };
        let observer_ptr = NonNull::from(&mut *observer);

        // Register it either as observing "some" or "full", depending on the `test_variant` we are
        // executing.
        //
        // SAFETY: `observer` is alive, is in no list yet, and `cleanup` unregisters it before it
        // is dropped at the end of the test.
        unsafe {
            match test_variant {
                ObserverThresholdTestSelector::Some => aggregator.add_observer_some(observer_ptr),
                ObserverThresholdTestSelector::Full => aggregator.add_observer_full(observer_ptr),
            }
        }
        // SAFETY: `observer` was just registered in exactly the list we remove it from.
        let cleanup = zr::defer(|| unsafe {
            match test_variant {
                ObserverThresholdTestSelector::Some => aggregator.remove_observer_some(&observer),
                ObserverThresholdTestSelector::Full => aggregator.remove_observer_full(&observer),
            }
        });

        // Prepare two kinds of samples: one reporting no stalls and another one reporting a 25%
        // stall.
        let sample_stall_none = AccumulatorStats {
            total_time_stall_some: DurationMono::ZERO,
            total_time_stall_full: DurationMono::ZERO,
            total_time_active: STALL_SAMPLE_INTERVAL,
        };
        let sample_stall_25_percent = AccumulatorStats {
            total_time_stall_some: sample_quarter,
            total_time_stall_full: if test_variant == ObserverThresholdTestSelector::Full {
                sample_quarter
            } else {
                DurationMono::ZERO
            },
            total_time_active: STALL_SAMPLE_INTERVAL,
        };

        let mut states = [false; 6];

        // The observer's initial state, before any sample has been reported.
        states[0] = receiver.is_above_threshold();

        // Report a sample with a 25% stall time, staying below the threshold.
        aggregator.sample_once_with(|callback| callback(&sample_stall_25_percent));
        states[1] = receiver.is_above_threshold();

        // Report a second sample with 25% stall time. This time, the cumulated stall time crosses
        // the threshold.
        aggregator.sample_once_with(|callback| callback(&sample_stall_25_percent));
        states[2] = receiver.is_above_threshold();

        // Now report two more samples without any stall time. We are still above threshold within
        // the observation window (4 samples).
        aggregator.sample_once_with(|callback| callback(&sample_stall_none));
        states[3] = receiver.is_above_threshold();
        aggregator.sample_once_with(|callback| callback(&sample_stall_none));
        states[4] = receiver.is_above_threshold();

        // Report one final sample without stall. This time, the cumulated stall time goes back to
        // 25% over the last 4 samples.
        aggregator.sample_once_with(|callback| callback(&sample_stall_none));
        states[5] = receiver.is_above_threshold();

        drop(cleanup);
        states
    }

    /// Checks the states recorded by `observer_threshold_sequence`.
    macro_rules! expect_threshold_sequence {
        ($states:expr) => {
            let states = $states;
            // Verify that the observer is initially not signaled.
            expect_false!(states[0]);
            // After one 25% sample we are still below the threshold.
            expect_false!(states[1]);
            // The second 25% sample crosses the threshold, so it became signaled.
            expect_true!(states[2]);
            // It stays signalled while the window still covers the two stalling samples.
            expect_true!(states[3]);
            expect_true!(states[4]);
            // The final sample drops the window back to 25%, unsignalling the observer.
            expect_false!(states[5]);
        };
    }

    /// Exercises the threshold logic against the "some" counter.
    #[test]
    fn observer_threshold_some() {
        expect_threshold_sequence!(observer_threshold_sequence(
            ObserverThresholdTestSelector::Some
        ));
    }

    /// Exercises the threshold logic against the "full" counter.
    #[test]
    fn observer_threshold_full() {
        expect_threshold_sequence!(observer_threshold_sequence(
            ObserverThresholdTestSelector::Full
        ));
    }

    /// Verifies that all registered observers receive the updates.
    #[test]
    fn observers_many() {
        stack_pin_init!(let aggregator = StallAggregator::init());

        // Create 4 observers with the smallest possible threshold.
        let receiver1 = StallObserverTestEventReceiver::new();
        let receiver2 = StallObserverTestEventReceiver::new();
        let receiver3 = StallObserverTestEventReceiver::new();
        let receiver4 = StallObserverTestEventReceiver::new();
        let min_threshold = DurationMono::from_nanos(1);
        // SAFETY: `cleanup` unregisters the observer, destroying it, before `receiver` goes out
        // of scope.
        let mut observer1 = unsafe {
            create_stall_observer_for_test(min_threshold, STALL_SAMPLE_INTERVAL, &receiver1)
        };
        // SAFETY: `cleanup` unregisters the observer, destroying it, before `receiver` goes out
        // of scope.
        let mut observer2 = unsafe {
            create_stall_observer_for_test(min_threshold, STALL_SAMPLE_INTERVAL, &receiver2)
        };
        // SAFETY: `cleanup` unregisters the observer, destroying it, before `receiver` goes out
        // of scope.
        let mut observer3 = unsafe {
            create_stall_observer_for_test(min_threshold, STALL_SAMPLE_INTERVAL, &receiver3)
        };
        // SAFETY: `cleanup` unregisters the observer, destroying it, before `receiver` goes out
        // of scope.
        let mut observer4 = unsafe {
            create_stall_observer_for_test(min_threshold, STALL_SAMPLE_INTERVAL, &receiver4)
        };

        // Register them.
        //
        // SAFETY: all four are alive, are in no list yet, and `cleanup` unregisters them before
        // they are dropped at the end of the test.
        unsafe {
            aggregator.add_observer_some(NonNull::from(&mut *observer1));
            aggregator.add_observer_some(NonNull::from(&mut *observer2));
            aggregator.add_observer_full(NonNull::from(&mut *observer3));
            aggregator.add_observer_full(NonNull::from(&mut *observer4));
        }
        // SAFETY: each observer was just registered in exactly the list we remove it from.
        let cleanup = zr::defer(|| unsafe {
            aggregator.remove_observer_some(&observer1);
            aggregator.remove_observer_some(&observer2);
            aggregator.remove_observer_full(&observer3);
            aggregator.remove_observer_full(&observer4);
        });

        // Verify that the observers are initially not signaled.
        expect_false!(receiver1.is_above_threshold());
        expect_false!(receiver2.is_above_threshold());
        expect_false!(receiver3.is_above_threshold());
        expect_false!(receiver4.is_above_threshold());

        // Verify that all of them become triggered when a stall occurs.
        aggregator.sample_once_with(|callback| {
            callback(&AccumulatorStats {
                total_time_stall_some: STALL_SAMPLE_INTERVAL,
                total_time_stall_full: STALL_SAMPLE_INTERVAL,
                total_time_active: STALL_SAMPLE_INTERVAL,
            });
        });
        expect_true!(receiver1.is_above_threshold());
        expect_true!(receiver2.is_above_threshold());
        expect_true!(receiver3.is_above_threshold());
        expect_true!(receiver4.is_above_threshold());

        drop(cleanup);
    }

    /// Verifies the stall states a context switch assigns to the outgoing and incoming threads.
    #[test]
    fn context_switch_states() {
        // Every case below leaves the current CPU's accumulator exactly as it was found, so this
        // does not perturb the system's real stall accounting. Interrupts stay disabled for the
        // same reason the scheduler has them disabled: this takes the per-CPU
        // accumulator's spinlock with the no-IRQ-save policy.
        let _irqd = InterruptDisableGuard::new();
        let cpu = curr_cpu_num();

        // Two kernel-only threads contribute nothing, so nothing changes.
        let updated = apply_context_switch_states(
            ThreadStallState::IgnoredKernelOnly,
            ThreadStallState::IgnoredKernelOnly,
            cpu,
            cpu,
        );
        expect_eq!(ThreadStallState::IgnoredKernelOnly as i32, updated.current_thread as i32);
        expect_eq!(ThreadStallState::IgnoredKernelOnly as i32, updated.next_thread as i32);

        // A progressing thread stops contributing and an inactive one starts, which cancels out
        // to no net change in the accumulator.
        let updated = apply_context_switch_states(
            ThreadStallState::Progressing,
            ThreadStallState::Inactive,
            cpu,
            cpu,
        );
        expect_eq!(ThreadStallState::Inactive as i32, updated.current_thread as i32);
        expect_eq!(ThreadStallState::Progressing as i32, updated.next_thread as i32);

        // A stalling thread that last ran on this CPU keeps both its state and its contribution.
        let updated = apply_context_switch_states(
            ThreadStallState::IgnoredKernelOnly,
            ThreadStallState::Stalling,
            cpu,
            cpu,
        );
        expect_eq!(ThreadStallState::IgnoredKernelOnly as i32, updated.current_thread as i32);
        expect_eq!(ThreadStallState::Stalling as i32, updated.next_thread as i32);
    }
}
