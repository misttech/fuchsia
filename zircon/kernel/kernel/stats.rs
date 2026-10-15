// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::mem::offset_of;
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

#[repr(C)]
#[derive(Debug, Copy, Clone)]
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
