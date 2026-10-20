// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::offset_of;
use core::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use percpu_bindings as bindings;

unsafe extern "C" {
    fn cpp_cpu_stats_inc_reschedules();
    fn cpp_cpu_stats_inc_context_switches();
    fn cpp_cpu_stats_inc_preempts();
    fn cpp_cpu_stats_inc_yields();
    fn cpp_cpu_stats_inc_interrupts();
    fn cpp_cpu_stats_inc_timer_ints();
    fn cpp_cpu_stats_inc_timers();
    fn cpp_cpu_stats_inc_perf_ints();
    fn cpp_cpu_stats_inc_syscalls();
    fn cpp_cpu_stats_inc_page_faults();
    fn cpp_cpu_stats_inc_reschedule_ipis();
    fn cpp_cpu_stats_inc_generic_ipis();
}

/// Per-CPU kernel level statistics (`struct cpu_stats`).
#[repr(C)]
#[derive(Debug, Default, Copy, Clone)]
pub struct CpuStats {
    pub last_updated_instant: i64,
    pub normalized_busy_time: i64,
    pub idle_time: i64,
    pub reschedules: u64,
    pub context_switches: u64,
    pub preempts: u64,
    pub yields: u64,
    pub interrupts: u64,
    pub timer_ints: u64,
    pub timers: u64,
    pub perf_ints: u64,
    pub syscalls: u64,
    pub page_faults: u64,
    pub reschedule_ipis: u64,
    pub generic_ipis: u64,
    pub active_energy_consumption_nj: u64,
    pub idle_energy_consumption_nj: u64,
}

impl CpuStats {
    /// Creates a zero-initialized [`CpuStats`].
    pub const fn new() -> Self {
        Self {
            last_updated_instant: 0,
            normalized_busy_time: 0,
            idle_time: 0,
            reschedules: 0,
            context_switches: 0,
            preempts: 0,
            yields: 0,
            interrupts: 0,
            timer_ints: 0,
            timers: 0,
            perf_ints: 0,
            syscalls: 0,
            page_faults: 0,
            reschedule_ipis: 0,
            generic_ipis: 0,
            active_energy_consumption_nj: 0,
            idle_energy_consumption_nj: 0,
        }
    }

    /// Reads the atomically-updated fields of `CpuStats` from `src` using `Ordering::Relaxed`
    /// atomic loads, leaving the queue-lock-synchronized energy fields zeroed.
    ///
    /// # Safety
    ///
    /// `src` must be a valid, properly aligned pointer to an initialized `CpuStats` whose atomic
    /// counter fields are only mutated via atomic operations.
    pub unsafe fn read_atomic_fields_relaxed(src: *mut Self) -> Self {
        // SAFETY: `src` is a valid, aligned `CpuStats` pointer, and all loaded fields are updated
        // exclusively via atomic operations.
        unsafe {
            Self {
                last_updated_instant: AtomicI64::from_ptr(&raw mut (*src).last_updated_instant)
                    .load(Ordering::Relaxed),
                normalized_busy_time: AtomicI64::from_ptr(&raw mut (*src).normalized_busy_time)
                    .load(Ordering::Relaxed),
                idle_time: AtomicI64::from_ptr(&raw mut (*src).idle_time).load(Ordering::Relaxed),
                reschedules: AtomicU64::from_ptr(&raw mut (*src).reschedules)
                    .load(Ordering::Relaxed),
                context_switches: AtomicU64::from_ptr(&raw mut (*src).context_switches)
                    .load(Ordering::Relaxed),
                preempts: AtomicU64::from_ptr(&raw mut (*src).preempts).load(Ordering::Relaxed),
                yields: AtomicU64::from_ptr(&raw mut (*src).yields).load(Ordering::Relaxed),
                interrupts: AtomicU64::from_ptr(&raw mut (*src).interrupts).load(Ordering::Relaxed),
                timer_ints: AtomicU64::from_ptr(&raw mut (*src).timer_ints).load(Ordering::Relaxed),
                timers: AtomicU64::from_ptr(&raw mut (*src).timers).load(Ordering::Relaxed),
                perf_ints: AtomicU64::from_ptr(&raw mut (*src).perf_ints).load(Ordering::Relaxed),
                syscalls: AtomicU64::from_ptr(&raw mut (*src).syscalls).load(Ordering::Relaxed),
                page_faults: AtomicU64::from_ptr(&raw mut (*src).page_faults)
                    .load(Ordering::Relaxed),
                reschedule_ipis: AtomicU64::from_ptr(&raw mut (*src).reschedule_ipis)
                    .load(Ordering::Relaxed),
                generic_ipis: AtomicU64::from_ptr(&raw mut (*src).generic_ipis)
                    .load(Ordering::Relaxed),
                active_energy_consumption_nj: 0,
                idle_energy_consumption_nj: 0,
            }
        }
    }

    /// Copies all fields of `CpuStats` from `src` using `Ordering::Relaxed` atomic loads for the
    /// atomically-updated fields, equivalent to
    /// `concurrent::WellDefinedCopyFrom<concurrent::SyncOpt::None, alignof(cpu_stats)>`.
    ///
    /// # Safety
    ///
    /// `src` must be a valid, properly aligned pointer to an initialized `CpuStats`, and the
    /// caller must hold the CPU's scheduler queue lock so that `active_energy_consumption_nj` and
    /// `idle_energy_consumption_nj` are not concurrently modified.
    pub unsafe fn copy_from_relaxed(src: *mut Self) -> Self {
        // SAFETY: The caller guarantees `src` is valid and the scheduler queue lock is held.
        unsafe {
            Self {
                active_energy_consumption_nj: (*src).active_energy_consumption_nj,
                idle_energy_consumption_nj: (*src).idle_energy_consumption_nj,
                ..Self::read_atomic_fields_relaxed(src)
            }
        }
    }
}

zr::static_assert!(core::mem::size_of::<CpuStats>() == 136);
zr::static_assert!(core::mem::align_of::<CpuStats>() == 8);
zr::static_assert!(core::mem::size_of::<CpuStats>() == core::mem::size_of::<bindings::cpu_stats>());
zr::static_assert!(
    core::mem::align_of::<CpuStats>() == core::mem::align_of::<bindings::cpu_stats>()
);
zr::static_assert!(
    offset_of!(CpuStats, last_updated_instant)
        == offset_of!(bindings::cpu_stats, last_updated_instant)
);
zr::static_assert!(
    offset_of!(CpuStats, normalized_busy_time)
        == offset_of!(bindings::cpu_stats, normalized_busy_time)
);
zr::static_assert!(offset_of!(CpuStats, idle_time) == offset_of!(bindings::cpu_stats, idle_time));
zr::static_assert!(
    offset_of!(CpuStats, reschedules) == offset_of!(bindings::cpu_stats, reschedules)
);
zr::static_assert!(
    offset_of!(CpuStats, context_switches) == offset_of!(bindings::cpu_stats, context_switches)
);
zr::static_assert!(offset_of!(CpuStats, preempts) == offset_of!(bindings::cpu_stats, preempts));
zr::static_assert!(offset_of!(CpuStats, yields) == offset_of!(bindings::cpu_stats, yields));
zr::static_assert!(offset_of!(CpuStats, interrupts) == offset_of!(bindings::cpu_stats, interrupts));
zr::static_assert!(offset_of!(CpuStats, timer_ints) == offset_of!(bindings::cpu_stats, timer_ints));
zr::static_assert!(offset_of!(CpuStats, timers) == offset_of!(bindings::cpu_stats, timers));
zr::static_assert!(offset_of!(CpuStats, perf_ints) == offset_of!(bindings::cpu_stats, perf_ints));
zr::static_assert!(offset_of!(CpuStats, syscalls) == offset_of!(bindings::cpu_stats, syscalls));
zr::static_assert!(
    offset_of!(CpuStats, page_faults) == offset_of!(bindings::cpu_stats, page_faults)
);
zr::static_assert!(
    offset_of!(CpuStats, reschedule_ipis) == offset_of!(bindings::cpu_stats, reschedule_ipis)
);
zr::static_assert!(
    offset_of!(CpuStats, generic_ipis) == offset_of!(bindings::cpu_stats, generic_ipis)
);
zr::static_assert!(
    offset_of!(CpuStats, active_energy_consumption_nj)
        == offset_of!(bindings::cpu_stats, active_energy_consumption_nj)
);
zr::static_assert!(
    offset_of!(CpuStats, idle_energy_consumption_nj)
        == offset_of!(bindings::cpu_stats, idle_energy_consumption_nj)
);

/// Increment the reschedule counter for the current CPU.
#[inline(always)]
pub fn inc_reschedules() {
    // SAFETY: cpp_cpu_stats_inc_reschedules increments the per-CPU reschedule counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_reschedules() }
}

/// Increment the context switch counter for the current CPU.
#[inline(always)]
pub fn inc_context_switches() {
    // SAFETY: cpp_cpu_stats_inc_context_switches increments the per-CPU context switch counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_context_switches() }
}

/// Increment the preempt counter for the current CPU.
#[inline(always)]
pub fn inc_preempts() {
    // SAFETY: cpp_cpu_stats_inc_preempts increments the per-CPU preempt counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_preempts() }
}

/// Increment the yield counter for the current CPU.
#[inline(always)]
pub fn inc_yields() {
    // SAFETY: cpp_cpu_stats_inc_yields increments the per-CPU yield counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_yields() }
}

/// Increment the interrupt counter for the current CPU.
#[inline(always)]
pub fn inc_interrupts() {
    // SAFETY: cpp_cpu_stats_inc_interrupts increments the per-CPU interrupt counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_interrupts() }
}

/// Increment the timer interrupt counter for the current CPU.
#[inline(always)]
pub fn inc_timer_ints() {
    // SAFETY: cpp_cpu_stats_inc_timer_ints increments the per-CPU timer interrupt counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_timer_ints() }
}

/// Increment the timer callback counter for the current CPU.
#[inline(always)]
pub fn inc_timers() {
    // SAFETY: cpp_cpu_stats_inc_timers increments the per-CPU timer callback counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_timers() }
}

/// Increment the performance monitor interrupt counter for the current CPU.
#[inline(always)]
pub fn inc_perf_ints() {
    // SAFETY: cpp_cpu_stats_inc_perf_ints increments the per-CPU performance interrupt counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_perf_ints() }
}

/// Increment the syscall counter for the current CPU.
#[inline(always)]
pub fn inc_syscalls() {
    // SAFETY: cpp_cpu_stats_inc_syscalls increments the per-CPU syscall counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_syscalls() }
}

/// Increment the page fault counter for the current CPU.
#[inline(always)]
pub fn inc_page_faults() {
    // SAFETY: cpp_cpu_stats_inc_page_faults increments the per-CPU page fault counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_page_faults() }
}

/// Increment the reschedule IPI counter for the current CPU.
#[inline(always)]
pub fn inc_reschedule_ipis() {
    // SAFETY: cpp_cpu_stats_inc_reschedule_ipis increments the per-CPU reschedule IPI counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_reschedule_ipis() }
}

/// Increment the generic IPI counter for the current CPU.
#[inline(always)]
pub fn inc_generic_ipis() {
    // SAFETY: cpp_cpu_stats_inc_generic_ipis increments the per-CPU generic IPI counter
    // for the currently executing CPU and is safe to call from any context.
    unsafe { cpp_cpu_stats_inc_generic_ipis() }
}
