// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use boot_options::BootOptions;
use core::cell::UnsafeCell;
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, Ordering};
use kprint::{kprint, kprintln};
use object_constants_rs as object_constants;
use pin_init::{InPlaceWrite, PinInit, Wrapper, pin_data, pin_init};
use zx_status::Status;

use super::event_dispatcher::EventDispatcher;
use super::executor::Executor;
use crate::counters::define_kcounter;
use crate::debuglog_rs::dlog_shutdown;
use crate::kernel;
use crate::kernel::deadline::{Deadline, InstantUnknown, TimerSlack};
use crate::kernel::event::AutounsignalEvent;
use crate::kernel::relaxed_atomic::RelaxedAtomicU8;
use crate::kernel::thread::ScopedMemoryAllocationDisabled;
use crate::kernel::timer::Timer;
use crate::platform_rs::HaltToken;
use crate::platform_rs::power::{PlatformHaltAction, ZirconCrashReason, platform_halt};
use crate::platform_rs::timer::{DurationMono, InstantMono, current_mono_time};
use crate::stall::StallAggregator;
use crate::vm::vm::oom_ktrace_duration;
use crate::vm::{evictor, pmm};

const PAGE_SIZE: u64 = page::SIZE as u64;
const MB: u64 = 1024 * 1024;

define_kcounter!(PRESSURE_LEVEL_OOM, "memory_watchdog.pressure.oom", Sum);
define_kcounter!(PRESSURE_LEVEL_IMMINENT_OOM, "memory_watchdog.pressure.imminent_oom", Sum);
define_kcounter!(PRESSURE_LEVEL_CRITICAL, "memory_watchdog.pressure.critical", Sum);
define_kcounter!(PRESSURE_LEVEL_WARNING, "memory_watchdog.pressure.warning", Sum);
define_kcounter!(PRESSURE_LEVEL_NORMAL, "memory_watchdog.pressure.normal", Sum);

define_kcounter!(EVICTION_TRIGGERED, "memory_watchdog.eviction.triggered", Sum);

#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PressureLevel {
    OutOfMemory = 0,
    ImminentOutOfMemory = 1,
    Critical = 2,
    Warning = 3,
    Normal = 4,
}

pub const NUM_LEVELS: usize = 5;
const NUM_WATERMARKS: usize = NUM_LEVELS - 1;

impl TryFrom<u8> for PressureLevel {
    type Error = ();
    fn try_from(val: u8) -> Result<Self, Self::Error> {
        match val {
            0 => Ok(Self::OutOfMemory),
            1 => Ok(Self::ImminentOutOfMemory),
            2 => Ok(Self::Critical),
            3 => Ok(Self::Warning),
            4 => Ok(Self::Normal),
            _ => Err(()),
        }
    }
}

fn pressure_level_to_string(level: PressureLevel) -> &'static str {
    match level {
        PressureLevel::OutOfMemory => "OutOfMemory",
        PressureLevel::ImminentOutOfMemory => "ImminentOutOfMemory",
        PressureLevel::Critical => "Critical",
        PressureLevel::Warning => "Warning",
        PressureLevel::Normal => "Normal",
    }
}

/// OneShot eviction strategy only triggers eviction events at memory pressure state transitions.
/// Continuous eviction strategy enables continuous background eviction as long the system remains
/// under memory pressure (i.e. at a memory pressure level that is eligible for eviction).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EvictionStrategy {
    OneShot = 0,
    Continuous = 1,
}

#[repr(transparent)]
struct RelaxedAtomicPressureLevel(RelaxedAtomicU8);

impl RelaxedAtomicPressureLevel {
    const fn new(level: PressureLevel) -> Self {
        Self(RelaxedAtomicU8::new(level as u8))
    }

    fn load(&self) -> PressureLevel {
        // SAFETY: PressureLevel is a u8, and we only store valid PressureLevel values.
        unsafe { core::mem::transmute(self.0.load()) }
    }

    fn store(&self, level: PressureLevel) {
        self.0.store(level as u8);
    }
}

fn count_pressure_event(level: PressureLevel) {
    match level {
        PressureLevel::OutOfMemory => PRESSURE_LEVEL_OOM.add(1),
        PressureLevel::ImminentOutOfMemory => PRESSURE_LEVEL_IMMINENT_OOM.add(1),
        PressureLevel::Critical => PRESSURE_LEVEL_CRITICAL.add(1),
        PressureLevel::Warning => PRESSURE_LEVEL_WARNING.add(1),
        PressureLevel::Normal => PRESSURE_LEVEL_NORMAL.add(1),
    }
}

fn handle_on_oom_reboot() {
    // Notify the pmm that although we are out of memory, we would like to never wait for memory.
    // This ensures that if userspace needs to allocate to do a graceful shutdown it is able to.
    pmm::node().stop_returning_should_wait();

    if !HaltToken::get().take() {
        // We failed to acquire the token.  Someone else must have it.  That's OK.  We'll rely on
        // them to halt/reboot.  Nothing left for us to do but wait.
        kprintln!("memory-pressure: halt/reboot already in progress; sleeping forever");
        let _ = kernel::thread::sleep(InstantMono::INFINITE);
    }
    // We now have the halt token so we're committed.  To ensure we record the true cause of the
    // reboot, we must ensure nothing (aside from a panic) prevents us from halting with reason OOM.

    // We are out of or nearly out of memory so future attempts to allocate may fail.  From this
    // point on, avoid performing any allocation.  Establish a "no allocation allowed" scope to
    // detect (assert) if we attempt to allocate.
    let _allocation_disabled = ScopedMemoryAllocationDisabled::new();

    kprintln!(
        "memory-pressure: pausing for {}ms after OOM mem signal",
        BootOptions::get().oom_timeout_ms
    );
    let status = HaltToken::get().wait_for_ack(&Deadline::after_mono(
        DurationMono::from_millis(BootOptions::get().oom_timeout_ms as i64),
        TimerSlack::none(),
    ));

    match status {
        Ok(()) => {
            kprintln!("memory-pressure: rebooting due to OOM. received user-mode acknowledgement.");
        }
        Err(Status::TIMED_OUT) => {
            // User mode code should have acked by now, since it hasn't, reboot the system.
            kprintln!(
                "memory-pressure: rebooting due to OOM. timed out after waiting {}ms for \
                 user-mode ack.",
                BootOptions::get().oom_timeout_ms
            );
        }
        Err(status) => {
            kprintln!(
                "memory-pressure: rebooting due to OOM. unexpected error while waiting for \
                 user-mode acknowledgement (status {}).",
                status.into_raw()
            );
        }
    }

    // Tell the oom_tests host test that we are about to generate an OOM
    // crashlog to keep it happy.  Without these messages present in a
    // specific order in the log, the test will fail.
    kprint!("memory-pressure: stowing crashlog\nZIRCON REBOOT REASON (OOM)\n");

    // The debuglog could contain diagnostic messages that would assist in debugging the cause of
    // the OOM.  Shutdown debuglog before rebooting in order to flush any queued messages.
    //
    // It is important that we don't hang during this process so set a deadline for the debuglog
    // to shutdown.
    //
    // How long should we wait?  Shutting down the debuglog includes flushing any buffered
    // messages to the serial port (if present).  Writing to a serial port can be slow.  Assuming
    // we have a full debuglog buffer of 128KB, at 115200 bps, with 8-N-1, it will take roughly
    // 11.4 seconds to drain the buffer.  The timeout should be long enough to allow a full DLOG
    // buffer to be drained.
    let deadline = current_mono_time() + DurationMono::from_seconds(20);
    let status = dlog_shutdown(deadline);
    if let Err(status) = status {
        // If `dlog_shutdown` failed, there's not much we can do besides print an error (which
        // probably won't make it out anyway since we've already called `dlog_shutdown`) and
        // continue on to `platform_halt`.
        kprintln!("ERROR: dlog_shutdown failed: {}", status.into_raw());
    }
    platform_halt(PlatformHaltAction::Reboot, ZirconCrashReason::Oom);
}

/// The callback provided to the `eviction_trigger` timer.
///
/// # Safety
///
/// `arg` must be a valid pointer to an initialized `MemoryWatchdogState`.
unsafe extern "C" fn eviction_trigger_callback(
    _timer: *mut kernel::timer::Timer,
    _now: i64,
    arg: *mut c_void,
) {
    // SAFETY: `arg` points to a live `MemoryWatchdogState` registered when setting the timer.
    let watchdog = unsafe { &*arg.cast::<MemoryWatchdogState>() };
    watchdog.eviction_trigger();
}

zr::static_assert_size_and_align!(
    MemoryWatchdogState,
    object_constants::kMemoryWatchdogStateSize,
    object_constants::kMemoryWatchdogStateAlign,
);

/// Monitors system-wide memory pressure levels, signals userspace events, and triggers eviction
/// or out-of-memory actions.
///
/// Object is thread safe.
#[pin_data]
#[repr(C)]
pub struct MemoryWatchdogState {
    /// Kernel-owned events used to signal userspace at different levels of memory pressure.
    mem_pressure_events: UnsafeCell<[Option<fbl::RefPtr<EventDispatcher>>; NUM_LEVELS]>,

    /// Event used for communicating memory related state changes between other parts of the system
    /// and the `worker_thread`.
    #[pin]
    mem_state_signal: AutounsignalEvent,

    /// Relaxed atomic so that debug methods can safely read it.
    mem_event_idx: RelaxedAtomicPressureLevel,
    prev_mem_event_idx: UnsafeCell<PressureLevel>,

    /// Watermark information is not modified after `init` and so is safe for multiple threads to
    /// access.
    mem_watermarks: UnsafeCell<[u64; NUM_WATERMARKS]>,
    watermark_debounce: UnsafeCell<u64>,

    /// Used to delay signaling memory level transitions in the case of rapid changes.
    hysteresis_seconds: UnsafeCell<DurationMono>,

    /// Used to delay eviction when going from a pressure level that does not require eviction to
    /// one that does.
    eviction_delay_ms: UnsafeCell<DurationMono>,

    /// Tracks last time the memory state was evaluated (and signaled if required).
    prev_mem_state_eval_time: UnsafeCell<InstantMono>,

    /// The highest pressure level we trigger eviction at, OOM being the lowest pressure level (0).
    max_eviction_level: UnsafeCell<PressureLevel>,

    /// The free memory target to aim for when we trigger eviction.
    free_mem_target: UnsafeCell<u64>,

    /// Current minimum amount of memory we want the triggered eviction to reclaim.
    min_free_target: UnsafeCell<u64>,

    /// A timer is used to trigger eviction so that user space is given a chance to act upon a
    /// memory pressure signal first.
    #[pin]
    eviction_trigger: UnsafeCell<Timer>,

    /// Tracks whether or not continuous eviction is presently happening or not. This is an atomic
    /// so that it can be set by the `eviction_trigger` after the timeout occurs, and this
    /// happens in a different thread. This value is only allowed to be set to true by the
    /// `eviction_trigger` callback, and is only allowed to be set to false by the main
    /// `worker_thread`, preventing any races in transitioning its state. Races in reading the
    /// state are fine, and the correctness of these is justified at each usage site.
    continuous_eviction_active: AtomicBool,

    /// OneShot eviction strategy only triggers eviction events at memory pressure state
    /// transitions. Continuous eviction strategy enables continuous background eviction as long
    /// the system remains under memory pressure (i.e. at a memory pressure level that is
    /// eligible for eviction).
    eviction_strategy: UnsafeCell<EvictionStrategy>,

    /// Record of the thread running `worker_thread` created during `init`.
    worker_thread: UnsafeCell<Option<kernel::thread::ThreadPtr>>,

    executor: UnsafeCell<Option<NonNull<Executor>>>,
}

/// Type alias for [`MemoryWatchdogState`].
pub type MemoryWatchdog = MemoryWatchdogState;

unsafe extern "C" {
    #[allow(improper_ctypes)]
    fn cpp_get_memory_watchdog() -> *const MemoryWatchdogState;
}

/// Returns a reference to the global [`MemoryWatchdog`].
#[inline]
pub fn get_memory_watchdog() -> &'static MemoryWatchdog {
    // SAFETY: `cpp_get_memory_watchdog` returns a pointer to the static `gExecutor`'s
    // `MemoryWatchdog`, which is layout-compatible with `MemoryWatchdogState` (verified by
    // `static_assert` in `memory_watchdog.cc`) and valid for `'static`.
    unsafe { &*cpp_get_memory_watchdog() }
}

// SAFETY: `MemoryWatchdogState` fields are either initialized once in `init` before any concurrent
// access, accessed exclusively by the worker thread, or synchronized via atomics (`mem_event_idx`,
// `continuous_eviction_active`), `AutounsignalEvent`, and `Timer::cancel` ordering before target
// updates.
unsafe impl Sync for MemoryWatchdogState {}
// SAFETY: `MemoryWatchdogState` can be safely transferred across threads under the same invariants
// as `Sync`.
unsafe impl Send for MemoryWatchdogState {}

impl MemoryWatchdogState {
    /// Creates an in-place initializer for a new `MemoryWatchdogState`.
    pub fn new() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            mem_pressure_events: UnsafeCell::new([None, None, None, None, None]),
            mem_state_signal <- AutounsignalEvent::init(false),
            mem_event_idx: RelaxedAtomicPressureLevel::new(PressureLevel::Normal),
            prev_mem_event_idx: UnsafeCell::new(PressureLevel::Normal),
            mem_watermarks: UnsafeCell::new([0; NUM_WATERMARKS]),
            watermark_debounce: UnsafeCell::new(0),
            hysteresis_seconds: UnsafeCell::new(DurationMono::from_seconds(10)),
            eviction_delay_ms: UnsafeCell::new(DurationMono::from_millis(5000)),
            prev_mem_state_eval_time: UnsafeCell::new(InstantMono::INFINITE_PAST),
            max_eviction_level: UnsafeCell::new(PressureLevel::Critical),
            free_mem_target: UnsafeCell::new(0),
            min_free_target: UnsafeCell::new(0),
            eviction_trigger <- UnsafeCell::pin_init(Timer::init(kernel::timer::ZX_CLOCK_MONOTONIC)),
            continuous_eviction_active: AtomicBool::new(false),
            eviction_strategy: UnsafeCell::new(EvictionStrategy::OneShot),
            worker_thread: UnsafeCell::new(None),
            executor: UnsafeCell::new(None),
        })
    }

    fn is_imminent_oom_enabled(&self) -> bool {
        // SAFETY: `mem_watermarks` is not modified after `init`.
        unsafe {
            (*self.mem_watermarks.get())[PressureLevel::ImminentOutOfMemory as usize]
                != (*self.mem_watermarks.get())[PressureLevel::OutOfMemory as usize]
        }
    }

    fn free_mem_bounds_for_level(&self, level: PressureLevel) -> (u64, u64) {
        // If ImminentOOM is disabled, we will never enter that level, and so we should never be
        // asked to compute its bounds.
        debug_assert!(
            self.is_imminent_oom_enabled() || level != PressureLevel::ImminentOutOfMemory
        );
        // Calculate the range, including debounce, for the current memory level.
        let mut lower = 0;
        let mut upper = u64::MAX;
        // SAFETY: `mem_watermarks` and `watermark_debounce` are not modified after `init`.
        unsafe {
            if (level as usize) > (PressureLevel::OutOfMemory as usize) {
                lower = (*self.mem_watermarks.get())[level as usize - 1]
                    .saturating_sub(*self.watermark_debounce.get());
            }
            if (level as usize) < NUM_WATERMARKS {
                upper = (*self.mem_watermarks.get())[level as usize]
                    .saturating_add(*self.watermark_debounce.get());
            }
        }
        (lower, upper)
    }

    fn calculate_pressure_level(&self) -> PressureLevel {
        // Get the bounds for the current level, this is inclusive of the debounce.
        let current = self.mem_event_idx.load();
        let (lower, upper) = self.free_mem_bounds_for_level(current);

        // Retrieve current free memory.
        // SAFETY: Queries PMM free page count and reads `mem_watermarks` (not modified after
        // `init`).
        let (free_mem, watermarks) =
            unsafe { (pmm::node().count_free_pages() * PAGE_SIZE, &*self.mem_watermarks.get()) };

        // Check if still inside the bounds.
        if free_mem >= lower && free_mem <= upper {
            // No change.
            return current;
        }

        // Determine the new level using a simple O(N).
        let mut new_level = PressureLevel::OutOfMemory as usize;
        while new_level < NUM_WATERMARKS && free_mem > watermarks[new_level] {
            new_level += 1;
        }
        let new_level = PressureLevel::try_from(new_level as u8).unwrap();
        debug_assert_ne!(new_level, current);
        // If ImminentOOM is disabled, we should never compute it as the current level.
        debug_assert!(
            self.is_imminent_oom_enabled() || new_level != PressureLevel::ImminentOutOfMemory
        );
        new_level
    }

    // Called by the `worker_thread` to determine if a kernel event needs to be signaled
    // corresponding to pressure change to level `idx`.
    #[inline]
    fn is_signal_due(&self, idx: PressureLevel, time_now: InstantMono) -> bool {
        // We signal a memory state change immediately if any of these conditions are met:
        // 1) The current index is lower than the previous one signaled (i.e. available memory is
        //    lower now), so that clients can act on the signal quickly.
        // 2) `hysteresis_seconds` have elapsed since the last time we examined the state.
        // SAFETY: Called exclusively on `worker_thread`; `hysteresis_seconds` is constant after
        // `init`.
        unsafe {
            idx < *self.prev_mem_event_idx.get()
                || time_now - *self.prev_mem_state_eval_time.get() >= *self.hysteresis_seconds.get()
        }
    }

    // Called by the `worker_thread` to determine if kernel eviction (asynchronous) needs to be
    // triggered in response to pressure change to level `idx`.
    #[inline]
    fn is_eviction_required(&self, idx: PressureLevel) -> bool {
        // Trigger asynchronous eviction if:
        // 1) the memory availability state is more critical than the previous one
        // AND
        // 2) we're configured to evict at that level.
        //
        // Do not trigger asynchronous eviction at the OOM level, as we have already performed
        // synchronous eviction to attempt a quick recovery before reaching here. At this
        // point we are about to signal filesystems to shut down on OOM, after which
        // eviction will be a no-op anyway, since there will no longer be any pager-backed
        // memory to evict.
        //
        //SAFETY: Called exclusively on `worker_thread`;
        // `max_eviction_level` is constant after `init`.
        unsafe {
            idx < *self.prev_mem_event_idx.get()
                && idx <= *self.max_eviction_level.get()
                && idx != PressureLevel::OutOfMemory
        }
    }

    fn eviction_trigger(&self) {
        // SAFETY: `eviction_strategy` and `free_mem_target` are constant after `init`.
        // `min_free_target` is only modified by `worker_thread` after canceling `eviction_trigger`.
        unsafe {
            // This runs from a timer interrupt context, as such we do not want to be performing
            // synchronous eviction and blocking some random thread. Therefore we use
            // the asynchronous eviction trigger that will cause the eviction thread to
            // perform the actual eviction work.
            if *self.eviction_strategy.get() == EvictionStrategy::Continuous {
                // Under a continuous eviction strategy we must set `continuous_eviction_active` to
                // true so that the `worker_thread` knows it should bump the evictor
                // when memory states change.
                self.continuous_eviction_active.store(true, Ordering::SeqCst);
            }
            EVICTION_TRIGGERED.add(1);
            pmm::node().evictor().evict_asynchronous(
                *self.min_free_target.get(),
                *self.free_mem_target.get(),
                evictor::EvictionLevel::OnlyOldest,
                evictor::Output::Print,
            );
        }
    }

    // Helper called by the memory pressure thread when OOM state is entered.
    fn on_oom(&self) {
        match BootOptions::get().oom_behavior {
            boot_options::OomBehavior::JobKill => {
                // SAFETY: `executor` is initialized in `init` before the worker thread starts.
                let executor = unsafe { (*self.executor.get()).unwrap().as_ref() };
                if !executor.get_root_job_dispatcher().kill_job_with_kill_on_oom() {
                    kprintln!("memory-pressure: no alive job has a kill bit");
                }

                // Since killing is asynchronous, sleep for a short period for the system to
                // quiesce. This prevents us from rapidly killing more jobs than
                // necessary. And if we don't find a killable job, don't just spin
                // since the next iteration probably won't find a one either.
                let _ = kernel::thread::sleep_relative(DurationMono::from_millis(500));
            }
            boot_options::OomBehavior::Reboot => {
                handle_on_oom_reboot();
            }
        }
    }

    fn wait_for_mem_change(&self, deadline: &Deadline) {
        let prev = self.mem_event_idx.load();
        // Coming into this method we must not be in the `OutOfMemory` state, as if this were
        // possible allocations would be stalled and we would be waiting for a memory state
        // change *before* triggering the evictor, which would cause a deadlock.
        debug_assert_ne!(prev, PressureLevel::OutOfMemory);
        let mut status = Ok(());
        // Count how many times in a row the setting of the free memory signal fails. This should
        // only fail in the case of an unlikely race, and failing repeatedly could indicate
        // a bug and also means the free_pages_evt_ in the pmm is not getting signaled.
        let mut set_free_memory_failed_iterations: u32 = 0;
        // This loop can iterate many times as it is woken up by both pmm state changes and, if
        // continuous eviction is enabled, page queues state changes.
        // SAFETY: `mem_watermarks`, `watermark_debounce`, and `free_mem_target` are not modified
        // after `init`, and `mem_state_signal` is valid for the lifetime of `self`.
        unsafe {
            loop {
                let cur_level = self.mem_event_idx.load();
                // We cannot enter this method in the `OutOfMemory` state and since we would exit
                // this loop if the state were to change we can never be configuring
                // the free memory signal in this state.
                debug_assert_ne!(cur_level, PressureLevel::OutOfMemory);
                let (lower, upper) = self.free_mem_bounds_for_level(cur_level);
                let delay_alloc_level = ((*self.mem_watermarks.get())
                    [PressureLevel::OutOfMemory as usize]
                    .saturating_sub(*self.watermark_debounce.get()))
                    / PAGE_SIZE;
                if pmm::node().set_free_memory_signal(
                    lower / PAGE_SIZE,
                    upper / PAGE_SIZE,
                    delay_alloc_level,
                    self.mem_state_signal.as_raw().cast(),
                ) {
                    // After having successfully set the event check again for any allocation
                    // failures. This is to ensure that if an allocation failure
                    // happened while we did not have an event set that it is
                    // not missed.
                    set_free_memory_failed_iterations = 0;
                    if BootOptions::get().oom_trigger_on_alloc_failure
                        && pmm::node().has_alloc_failed_no_mem()
                    {
                        return;
                    }
                    status = self.mem_state_signal.wait(deadline);
                } else {
                    set_free_memory_failed_iterations += 1;
                    // Setting failed, must've raced. Fall through and compute the new pressure
                    // level.
                    if set_free_memory_failed_iterations > 5
                        && set_free_memory_failed_iterations.is_power_of_two()
                    {
                        kprintln!(
                            "memory-pressure: WARNING pmm_set_free_memory_signal has failed \
                             {set_free_memory_failed_iterations} times in a row"
                        );
                    }
                }
                self.mem_event_idx.store(self.calculate_pressure_level());
                // If continuous eviction is currently active then let the evictor know that it may
                // have some work to do. This is done by requesting an asynchronous
                // eviction to the free memory target. As we are running in the
                // context of the `worker_thread`, there is no race where active could
                // transition from true->false, so we will not trigger eviction unnecessarily.
                // Should there be a false->true race (due to the `eviction_trigger`
                // callback running) then this is fine, since that callback will set
                // the eviction target. See the documentation on
                // EvictOneShotAsynchronous for how eviction requests combine, and why
                // we can repeatedly perform this request correctly.
                if self.continuous_eviction_active.load(Ordering::SeqCst) {
                    pmm::node().evictor().evict_asynchronous(
                        0,
                        *self.free_mem_target.get(),
                        evictor::EvictionLevel::OnlyOldest,
                        evictor::Output::NoPrint,
                    );
                }
                // In the case where we raced with additional pmm actions keep looping unless the
                // deadline was reached.
                if self.mem_event_idx.load() != prev || status.is_err() {
                    break;
                }
            }
        }
    }

    fn worker_thread(&self) -> ! {
        // Tracks whether we have logged entering the OOM state. Used to rate limit logs when
        // bouncing in and out of OOM rapidly. Hysteresis delays confirming the transition
        // to a lower pressure state, so we can re-enter OOM multiple times before that
        // happens.
        let mut oom_entry_logged = false;
        // Accumulates the total number of pages evicted and time spent evicting during a
        // contiguous OOM period (which may involve multiple bounces in and out of OOM level).
        let mut total_oom_evicted_pages: u64 = 0;
        let mut total_oom_eviction_duration = DurationMono::ZERO;

        // SAFETY: `worker_thread` is the single thread that mutates `prev_mem_event_idx`,
        // `prev_mem_state_eval_time`, `min_free_target`, and `eviction_trigger` (after `init`),
        // and reads configuration fields that are immutable after `init`.
        unsafe {
            loop {
                // If we've hit OOM level perform some immediate synchronous eviction to attempt to
                // avoid OOM.
                if self.mem_event_idx.load() == PressureLevel::OutOfMemory {
                    oom_ktrace_duration!(1, "MemoryWatchdog::OutOfMemory");

                    // Log only the first time we enter OOM in this period to avoid spam.
                    if !oom_entry_logged {
                        kprintln!(
                            "memory-pressure: beginning reclamation to avoid OOM. Allocations are \
                             now disabled"
                        );
                        oom_entry_logged = true;
                    }

                    count_pressure_event(self.mem_event_idx.load());
                    let eviction_start = current_mono_time();
                    // Keep trying to perform eviction for as long as we are evicting non-zero pages
                    // and we remain in the out of memory state.
                    while self.mem_event_idx.load() == PressureLevel::OutOfMemory {
                        let evicted_pages = pmm::node()
                            .evictor()
                            .evict_synchronous(
                                MB * BootOptions::get().oom_eviction_delta_at_oom_mb,
                                0,
                                evictor::EvictionLevel::IncludeNewest,
                                evictor::Output::NoPrint,
                                evictor::TriggerReason::OOM,
                            )
                            .counts
                            .non_loaned_total();
                        if evicted_pages == 0 {
                            kprintln!("memory-pressure: found no pages to evict");
                            break;
                        }
                        // Accumulate total pages evicted across all bounces in this OOM period.
                        total_oom_evicted_pages =
                            total_oom_evicted_pages.saturating_add(evicted_pages);
                        self.mem_event_idx.store(self.calculate_pressure_level());
                    }
                    total_oom_eviction_duration =
                        total_oom_eviction_duration + (current_mono_time() - eviction_start);
                }

                // Check to see if the PMM has failed any allocations.  If the PMM has ever failed
                // to allocate because it was out of memory, then escalate the
                // pressure level to trigger an OOM response immediately.  The idea
                // here is that usermode processes may not be able to handle allocation
                // failure and therefore could have become wedged in some way.
                if BootOptions::get().oom_trigger_on_alloc_failure
                    && pmm::node().has_alloc_failed_no_mem()
                {
                    let first_failure = pmm::node().get_first_alloc_failure();
                    // This log message is load-bearing server-side as it's used to identify the
                    // culprit of the OOM.
                    // Please notify //src/developer/forensics/OWNERS upon changing.
                    kprintln!(
                        "memory-pressure: failed one or more allocations (first reported type: \
                         {:s}, size: {}, free memory: {}MB), escalating to oom...",
                        first_failure.type_str(),
                        first_failure.size,
                        (first_failure.free_count * PAGE_SIZE) / MB
                    );
                    self.mem_event_idx.store(PressureLevel::OutOfMemory);
                }

                let time_now = current_mono_time();

                if self.is_signal_due(self.mem_event_idx.load(), time_now) {
                    count_pressure_event(self.mem_event_idx.load());

                    // Log the total pages reclaimed during this OOM period (if any) and reset the
                    // OOM logging flag.
                    if oom_entry_logged {
                        if total_oom_evicted_pages > 0 {
                            let mut buf = [0u8; pretty::MAX_FORMAT_SIZE_LEN];
                            kprintln!(
                                "memory-pressure: reclaimed {:s} in {}ms to avoid OOM",
                                pretty::format_size_rs(
                                    &mut buf,
                                    (total_oom_evicted_pages * PAGE_SIZE) as usize
                                ),
                                total_oom_eviction_duration.into_millis()
                            );
                            total_oom_evicted_pages = 0;
                        }
                        total_oom_eviction_duration = DurationMono::ZERO;
                        oom_entry_logged = false;
                    }

                    kprintln!(
                        "memory-pressure: memory availability state - {:s}",
                        pressure_level_to_string(self.mem_event_idx.load())
                    );

                    if self.is_eviction_required(self.mem_event_idx.load()) {
                        pmm::page_queues().dump();
                        // Clear any previous eviction trigger. Once Cancel completes we know that
                        // we will not race with the callback and are free
                        // to update the targets. Cancel will return true if the
                        // timer was canceled before it was scheduled on a cpu, i.e. an eviction was
                        // outstanding.
                        let eviction_was_outstanding =
                            Pin::new_unchecked(&mut *self.eviction_trigger.get()).cancel();

                        if BootOptions::get().oom_evict_with_min_target {
                            let free_mem = pmm::node().count_free_pages() * PAGE_SIZE;
                            // Set the minimum amount to free as half the amount required to reach
                            // our desired free memory level. This
                            // minimum ensures that even if the user reduces memory in reaction to
                            // this signal we will always attempt to free a bit.
                            *self.min_free_target.get() = if free_mem < *self.free_mem_target.get()
                            {
                                (*self.free_mem_target.get() - free_mem) / 2
                            } else {
                                0
                            };
                        } else {
                            *self.min_free_target.get() = 0;
                        }

                        // If eviction was outstanding when we canceled the eviction trigger,
                        // trigger eviction immediately without any delay.
                        // We are here because of a rapid allocation spike which
                        // caused the memory pressure to become more critical in a very short
                        // interval, so it might be better to evict pages as
                        // soon as possible to try and counter the allocation spike.
                        // Otherwise if eviction was not outstanding, trigger the eviction for
                        // slightly in the future. Half the hysteresis time
                        // here is a balance between giving user space time to
                        // release memory and the eviction running before the end of the hysteresis
                        // period.
                        if eviction_was_outstanding
                            || *self.eviction_delay_ms.get() == DurationMono::ZERO
                        {
                            self.eviction_trigger();
                        } else {
                            Pin::new_unchecked(&*self.eviction_trigger.get()).set_oneshot(
                                (time_now + *self.eviction_delay_ms.get()).into_nanos(),
                                eviction_trigger_callback,
                                self as *const Self as *mut c_void,
                            );
                        }
                    } else if *self.eviction_strategy.get() == EvictionStrategy::Continuous
                        && self.mem_event_idx.load() > *self.max_eviction_level.get()
                    {
                        // If we're out of the max configured eviction-eligible memory pressure
                        // level, disable continuous eviction.

                        // Cancel any outstanding eviction trigger, so that eviction is not
                        // accidentally enabled *after* we disable it here.
                        Pin::new_unchecked(&mut *self.eviction_trigger.get()).cancel();
                        self.continuous_eviction_active.store(false, Ordering::SeqCst);
                    }

                    // Unsignal the last event that was signaled.
                    if let Err(status) = (*self.mem_pressure_events.get())
                        [*self.prev_mem_event_idx.get() as usize]
                        .as_ref()
                        .unwrap()
                        .user_signal_self(zx_types::ZX_EVENT_SIGNALED, 0)
                    {
                        panic!(
                            "memory-pressure: unsignal memory event {} failed: {}\n",
                            pressure_level_to_string(*self.prev_mem_event_idx.get()),
                            status.into_raw()
                        );
                    }

                    // Signal event corresponding to the new memory state.
                    if let Err(status) = (*self.mem_pressure_events.get())
                        [self.mem_event_idx.load() as usize]
                        .as_ref()
                        .unwrap()
                        .user_signal_self(0, zx_types::ZX_EVENT_SIGNALED)
                    {
                        panic!(
                            "memory-pressure: signal memory event {} failed: {}\n",
                            pressure_level_to_string(self.mem_event_idx.load()),
                            status.into_raw()
                        );
                    }
                    *self.prev_mem_event_idx.get() = self.mem_event_idx.load();
                    *self.prev_mem_state_eval_time.get() = time_now;

                    // If we're below the out-of-memory watermark, trigger OOM behavior.
                    if self.mem_event_idx.load() == PressureLevel::OutOfMemory {
                        pmm::page_queues().dump();
                        self.on_oom();
                    }

                    // Wait for the memory state to change again.
                    self.wait_for_mem_change(&Deadline::infinite());
                } else {
                    *self.prev_mem_state_eval_time.get() = time_now;

                    // We are ignoring this memory state transition. Wait for only
                    // `hysteresis_seconds`, and then re-evaluate the memory
                    // state. Otherwise we could remain stuck at the lower memory state if
                    // `mem_state_signal` is not signaled.
                    self.wait_for_mem_change(&Deadline::no_slack(InstantUnknown::from(
                        time_now + *self.hysteresis_seconds.get(),
                    )));
                }
            }
        }
    }

    /// `init` must be called before any other methods.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `executor` is safe to dereference.
    pub unsafe fn init(&self, executor: NonNull<Executor>) {
        // SAFETY: `init` is called once during boot before any other methods or threads access
        // `self`.
        unsafe {
            debug_assert!((*self.executor.get()).is_none());

            *self.executor.get() = Some(executor);

            let events = &mut *self.mem_pressure_events.get();
            for (i, event) in events.iter_mut().enumerate().take(NUM_LEVELS) {
                let level = PressureLevel::try_from(i as u8).unwrap();
                let handle = match EventDispatcher::create() {
                    Ok((h, _rights)) => h,
                    Err(status) => {
                        panic!(
                            "memory-pressure: create memory event {} failed: {}\n",
                            pressure_level_to_string(level),
                            status.into_raw()
                        );
                    }
                };
                *event = Some(handle.release());
            }

            let boot_options = BootOptions::get();
            if boot_options.oom_enabled {
                {
                    let watermarks = &mut *self.mem_watermarks.get();
                    // TODO(rashaeqbal): The watermarks chosen below are arbitrary. Tune them based
                    // on memory usage patterns. Consider moving to percentages
                    // of total memory instead of absolute numbers - will
                    // be easier to maintain across platforms.
                    watermarks[PressureLevel::OutOfMemory as usize] =
                        boot_options.oom_out_of_memory_threshold_mb * MB;
                    watermarks[PressureLevel::ImminentOutOfMemory as usize] = watermarks
                        [PressureLevel::OutOfMemory as usize]
                        + boot_options.oom_imminent_oom_delta_mb * MB;
                    watermarks[PressureLevel::Critical as usize] =
                        boot_options.oom_critical_threshold_mb * MB;
                    watermarks[PressureLevel::Warning as usize] =
                        boot_options.oom_warning_threshold_mb * MB;
                }
                let watermarks = &*self.mem_watermarks.get();

                *self.watermark_debounce.get() = boot_options.oom_debounce_mb * MB;
                if boot_options.oom_evict_at_warning {
                    *self.max_eviction_level.get() = PressureLevel::Warning;
                }

                // Validate our watermarks and debounce settings makes sense.
                for j in 0..NUM_WATERMARKS {
                    let prev = if j == 0 { 0 } else { watermarks[j - 1] };
                    let next = if j == NUM_WATERMARKS - 1 { u64::MAX } else { watermarks[j + 1] };
                    let curr = watermarks[j];
                    // The watermarks should be in increasing order, with a minimum of
                    // `watermark_debounce` difference between consecutive
                    // levels. The only exception is if the ImminentOOM level is
                    // disabled (by setting oom_imminent_oom_delta_mb to 0), in which case the OOM
                    // and ImminentOOM watermarks will be the same, and the
                    // ImminentOOM level will never be entered; we will
                    // either be in OOM or Critical.
                    assert!(
                        curr > prev
                            || (j == PressureLevel::ImminentOutOfMemory as usize && curr == prev)
                    );
                    assert!(
                        curr < next || (j == PressureLevel::OutOfMemory as usize && curr == next)
                    );
                    assert!(
                        curr.saturating_sub(prev) > *self.watermark_debounce.get()
                            || (j == PressureLevel::ImminentOutOfMemory as usize && curr == prev)
                    );
                    assert!(
                        next.saturating_sub(curr) > *self.watermark_debounce.get()
                            || (j == PressureLevel::OutOfMemory as usize && curr == next)
                    );
                }

                // Set our eviction target to be such that we try to get completely out of the max
                // eviction level, taking into account the debounce.
                *self.free_mem_target.get() = watermarks[*self.max_eviction_level.get() as usize]
                    + *self.watermark_debounce.get();

                *self.hysteresis_seconds.get() =
                    DurationMono::from_seconds(boot_options.oom_hysteresis_seconds as i64);
                *self.eviction_delay_ms.get() =
                    DurationMono::from_millis(boot_options.oom_eviction_delay_ms as i64);

                kprintln!(
                    "memory-pressure: memory watermarks - OutOfMemory: {}MB, Critical: {}MB, \
                     Warning: {}MB, Debounce: {}MB",
                    watermarks[PressureLevel::OutOfMemory as usize] / MB,
                    watermarks[PressureLevel::Critical as usize] / MB,
                    watermarks[PressureLevel::Warning as usize] / MB,
                    *self.watermark_debounce.get() / MB
                );

                kprintln!(
                    "memory-pressure: hysteresis interval - {} seconds",
                    (*self.hysteresis_seconds.get()).into_seconds()
                );

                if boot_options.oom_evict_continuous {
                    *self.eviction_strategy.get() = EvictionStrategy::Continuous;
                    pmm::page_queues()
                        .set_aging_event(Some(NonNull::from(self.mem_state_signal.as_event())));
                } else {
                    *self.eviction_strategy.get() = EvictionStrategy::OneShot;
                }

                kprintln!(
                    "memory-pressure: eviction: level - {:s}, strategy - {:s}, delay - {} ms",
                    pressure_level_to_string(*self.max_eviction_level.get()),
                    if boot_options.oom_evict_continuous { "continuous" } else { "one-shot" },
                    (*self.eviction_delay_ms.get()).into_millis()
                );

                if self.is_imminent_oom_enabled() {
                    kprintln!(
                        "memory-pressure: ImminentOutOfMemory watermark - {}MB",
                        watermarks[PressureLevel::ImminentOutOfMemory as usize] / MB
                    );
                }

                extern "C" fn memory_worker_thread(arg: *mut c_void) -> i32 {
                    // SAFETY: `arg` is a valid pointer to `MemoryWatchdogState` passed to
                    // `create_with_priority`.
                    let watchdog = unsafe { &*arg.cast::<MemoryWatchdogState>() };
                    watchdog.worker_thread()
                }

                *self.worker_thread.get() = kernel::thread::create_with_priority(
                    c"memory-pressure-thread".as_ptr(),
                    memory_worker_thread,
                    self as *const Self as *mut c_void,
                    kernel::scheduler_state::HIGHEST_PRIORITY,
                )
                .ok();
                debug_assert!((*self.worker_thread.get()).is_some());
                (*self.worker_thread.get()).unwrap().resume();
            }
        }
    }

    /// Returns the memory pressure event dispatcher corresponding to `kind`.
    pub fn get_mem_pressure_event(&self, kind: u32) -> Option<fbl::RefPtr<EventDispatcher>> {
        // SAFETY: `mem_pressure_events` is initialized in `init` and not modified thereafter.
        let events = unsafe { &*self.mem_pressure_events.get() };
        match kind {
            zx_types::ZX_SYSTEM_EVENT_OUT_OF_MEMORY => {
                events[PressureLevel::OutOfMemory as usize].clone()
            }
            zx_types::ZX_SYSTEM_EVENT_IMMINENT_OUT_OF_MEMORY => {
                events[PressureLevel::ImminentOutOfMemory as usize].clone()
            }
            zx_types::ZX_SYSTEM_EVENT_MEMORY_PRESSURE_CRITICAL => {
                events[PressureLevel::Critical as usize].clone()
            }
            zx_types::ZX_SYSTEM_EVENT_MEMORY_PRESSURE_WARNING => {
                events[PressureLevel::Warning as usize].clone()
            }
            zx_types::ZX_SYSTEM_EVENT_MEMORY_PRESSURE_NORMAL => {
                events[PressureLevel::Normal as usize].clone()
            }
            _ => None,
        }
    }

    /// Returns the number of bytes of memory that must be allocated to reach `level`.
    pub fn debug_num_bytes_till_pressure_level(&self, level: PressureLevel) -> u64 {
        // SAFETY: `executor`, `mem_watermarks`, and `watermark_debounce` are not modified after
        // `init`.
        unsafe {
            // Check we have been initialized.
            debug_assert!((*self.executor.get()).is_some());
            if self.mem_event_idx.load() <= level {
                // Already in level, or in a state with less available memory than level
                return 0;
            }
            // We need to either get free_pages below mem_watermarks[level] or, if we are
            // in state (level + 1), we also need to clear the debounce amount. For simplicity we
            // just always allocate the debounce amount as well.
            let trigger = (*self.mem_watermarks.get())[level as usize]
                .saturating_sub(*self.watermark_debounce.get());
            let free_count = pmm::node().count_free_pages() * PAGE_SIZE;
            // Handle races in the current pressure level.
            if free_count < trigger {
                return 0;
            }
            free_count - trigger
        }
    }

    /// Dumps the current memory watchdog state to the kernel log.
    pub fn dump(&self) {
        // SAFETY: `mem_watermarks` and `watermark_debounce` are not modified after `init`.
        unsafe {
            let mut buf1 = [0u8; pretty::MAX_FORMAT_SIZE_LEN];
            let mut buf2 = [0u8; pretty::MAX_FORMAT_SIZE_LEN];
            kprint!("watermarks: [");
            for i in 0..NUM_WATERMARKS {
                let level = PressureLevel::try_from(i as u8).unwrap();
                kprint!(
                    "{:s}: {:s}{:s}",
                    pressure_level_to_string(level),
                    pretty::format_size_rs(&mut buf1, (*self.mem_watermarks.get())[i] as usize),
                    if i + 1 == NUM_WATERMARKS { "]\n" } else { ", " }
                );
            }
            let current = self.mem_event_idx.load();
            let (lower, upper) = self.free_mem_bounds_for_level(current);
            kprintln!(
                "debounce: {:s}",
                pretty::format_size_rs(&mut buf1, *self.watermark_debounce.get() as usize)
            );
            kprintln!("current state: {} [{:s}]", current as u8, pressure_level_to_string(current));
            kprintln!(
                "current bounds: [{:s}, {:s}]",
                pretty::format_size_rs(&mut buf1, lower as usize),
                pretty::format_size_rs(&mut buf2, upper as usize)
            );
            kprintln!(
                "free memory: {:s}",
                pretty::format_size_rs(
                    &mut buf1,
                    (pmm::node().count_free_pages() * PAGE_SIZE) as usize
                )
            );
            let stats = StallAggregator::get().read_stats();
            kprintln!(
                "memory stall time: some {}, full {}",
                stats.stalled_time_some.into_nanos(),
                stats.stalled_time_full.into_nanos()
            );
        }
    }

    /// Debug method to retrieve any current worker thread. Only to be used for testing / debugging
    /// purposes. It is up to the caller to know if this objects is alive or not.
    pub fn debug_get_worker_thread(&self) -> Option<kernel::thread::ThreadPtr> {
        // SAFETY: `worker_thread` is written once in `init` before any debug callers run.
        unsafe { *self.worker_thread.get() }
    }
}

/// Initializes a `MemoryWatchdogState` in-place.
///
/// # Safety
///
/// `storage` must point to valid, properly aligned, uninitialized memory for `MemoryWatchdogState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_memory_watchdog_construct(
    storage: *mut MaybeUninit<MemoryWatchdogState>,
) {
    // SAFETY: `storage` is aligned and sized for `MemoryWatchdogState`.
    let _ = unsafe { storage.as_mut_unchecked().write_pin_init(MemoryWatchdogState::new()) };
}

/// Drops a `MemoryWatchdogState` in-place.
///
/// # Safety
///
/// `storage` must point to a valid, initialized `MemoryWatchdogState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_memory_watchdog_destroy(storage: *mut MemoryWatchdogState) {
    // SAFETY: `storage` points to an initialized `MemoryWatchdogState`.
    unsafe {
        core::ptr::drop_in_place(storage);
    }
}
