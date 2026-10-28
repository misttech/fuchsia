// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2014 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Debug console functions.

use core::ffi::{CStr, c_int, c_void};
use core::ptr;
use kprint::kprintln;
use zx_types::{ZX_TIME_INFINITE, zx_duration_mono_t, zx_koid_t};

use crate::arch_rs::InterruptDisableGuard;
use crate::arch_rs::vm::is_kernel_address;
use crate::console::{CMD_AVAIL_ALWAYS, CMD_AVAIL_NORMAL, CMD_FLAG_PANIC, CmdArgs, static_command};
use crate::kconcurrent::{
    ChainLockTransaction, ChainLocked, IRQ_SAVE_OPTION, clt_tag, single_chain_lock_guard_owned,
};
use crate::kernel::mp::{SMP_MAX_CPUS, cpu_num_to_mask};
use crate::kernel::percpu::{CpuStats, PerCpu};
use crate::kernel::recurring_callback::StaticRecurringCallback;
use crate::kernel::scheduler::{self, Scheduler};
use crate::kernel::thread::{
    Backtrace, DEFAULT_PRIORITY, Thread, ThreadPtr, create_with_priority, reschedule,
    thread_id_to_thread_slow,
};
use crate::kernel::types::cpu_num_t;
use crate::platform_rs::timer::{InstantMono, current_mono_time, zx_sec};

/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where `argc >= 1`
/// and each element's `arg_str` is a valid null-terminated C string. When `"bt"` or `"dump"` is
/// invoked with a kernel address in `argv[2].arg_uint`, that address must point to a valid, live
/// [`Thread`].
unsafe extern "C" fn cmd_thread(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
    // SAFETY: The caller guarantees `argc >= 1` and `argv` points to at least `argc` initialized
    // `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("{:s} bt <thread pointer or id>", argv[0].as_str());
        kprintln!("{:s} dump <thread pointer or id>", argv[0].as_str());
        kprintln!("{:s} list [owner name]", argv[0].as_str());
        kprintln!("{:s} list_full [owner name]", argv[0].as_str());
        -1
    };
    if argc < 2 {
        kprintln!("not enough arguments");
        return usage();
    }

    if argv[1].as_str() == "bt" {
        if argc < 3 {
            kprintln!("not enough arguments");
            return usage();
        }

        {
            // Hold the list lock so that thread objects cannot destruct while we dump
            // this info.
            //
            // Note that the practice of dumping info about a thread by direct kernel
            // address is inherently dangerous.  No validation of the pointer is
            // currently done.
            ksync::lock!(let _list_lock_guard = Thread::get_list_lock().lock());
            // SAFETY: `_list_lock_guard` holds `Thread::get_list_lock()`, and diagnostic command
            // inspects `t` while holding the list lock.
            unsafe {
                let t = if is_kernel_address(argv[2].arg_uint as usize) {
                    ThreadPtr::from_raw(ptr::with_exposed_provenance_mut::<Thread>(
                        argv[2].arg_uint as usize,
                    ))
                } else {
                    thread_id_to_thread_slow(argv[2].arg_uint as zx_koid_t)
                };
                if let Some(t) = t {
                    let mut bt = Backtrace::default();
                    t.get_backtrace(&mut bt);
                    bt.print();
                }
            }
        }
    } else if argv[1].as_str() == "dump" {
        if argc < 3 {
            kprintln!("not enough arguments");
            return usage();
        }

        // SAFETY: Diagnostic command dumps raw kernel thread address provided by operator, or
        // calls `dump_tid_during_panic` when `CMD_FLAG_PANIC` is set.
        unsafe {
            if is_kernel_address(argv[2].arg_uint as usize) {
                if let Some(t) = ThreadPtr::from_raw(ptr::with_exposed_provenance_mut::<Thread>(
                    argv[2].arg_uint as usize,
                )) {
                    t.dump(true);
                }
            } else if (flags & CMD_FLAG_PANIC) != 0 {
                Thread::dump_tid_during_panic(argv[2].arg_uint as zx_koid_t, true);
            } else {
                Thread::dump_tid(argv[2].arg_uint as zx_koid_t, true);
            }
        }
    } else if argv[1].as_str() == "list" {
        kprintln!("thread list:");
        // SAFETY: `argv[2].arg_str` is a valid C string when `argc >= 3`, and
        // `dump_all_during_panic` is only called when `CMD_FLAG_PANIC` is set.
        unsafe {
            let match_owner =
                (argc >= 3 && !argv[2].arg_str.is_null()).then(|| CStr::from_ptr(argv[2].arg_str));
            if (flags & CMD_FLAG_PANIC) != 0 {
                Thread::dump_all_during_panic(match_owner, false);
            } else {
                Thread::dump_all(match_owner, false);
            }
        }
    } else if argv[1].as_str() == "list_full" {
        kprintln!("thread list:");
        // SAFETY: `argv[2].arg_str` is a valid C string when `argc >= 3`, and
        // `dump_all_during_panic` is only called when `CMD_FLAG_PANIC` is set.
        unsafe {
            let match_owner =
                (argc >= 3 && !argv[2].arg_str.is_null()).then(|| CStr::from_ptr(argv[2].arg_str));
            if (flags & CMD_FLAG_PANIC) != 0 {
                Thread::dump_all_during_panic(match_owner, true);
            } else {
                Thread::dump_all(match_owner, true);
            }
        }
    } else {
        kprintln!("invalid args");
        return usage();
    }

    // reschedule to let debuglog potentially run
    if (flags & CMD_FLAG_PANIC) == 0 {
        reschedule();
    }

    0
}

/// # Safety
///
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match the console
/// command signature.
unsafe extern "C" fn cmd_threadstats(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    for i in 0..(PerCpu::processor_count() as cpu_num_t) {
        if !scheduler::peek_is_active(i) {
            continue;
        }
        let percpu = PerCpu::get(i);
        // SAFETY: `percpu.stats.get()` points to a valid, aligned `CpuStats` instance whose
        // counter fields are updated via atomic operations.
        let stats = unsafe { CpuStats::read_atomic_fields_relaxed(percpu.stats.get()) };

        kprintln!("thread stats (cpu {}):", i);
        kprintln!("\ttotal idle time: {}", stats.idle_time);
        kprintln!("\ttotal busy time: {}", current_mono_time().0.saturating_sub(stats.idle_time));
        kprintln!("\treschedules: {}", stats.reschedules);
        kprintln!("\treschedule_ipis: {}", stats.reschedule_ipis);
        kprintln!("\tcontext_switches: {}", stats.context_switches);
        kprintln!("\tpreempts: {}", stats.preempts);
        kprintln!("\tyields: {}", stats.yields);
        kprintln!("\ttimer interrupts: {}", stats.timer_ints);
        kprintln!("\ttimers: {}", stats.timers);
    }

    0
}

ksync::declare_singleton_spinlock!(ThreadLoadStateLock);

struct ThreadLoadState {
    old_stats: [CpuStats; SMP_MAX_CPUS],
    last_idle_time: [zx_duration_mono_t; SMP_MAX_CPUS],
}

static THREADLOAD_STATE: ksync::KCell<ThreadLoadState, ThreadLoadStateLock> =
    ksync::KCell::new(ThreadLoadState {
        old_stats: [CpuStats::new(); SMP_MAX_CPUS],
        last_idle_time: [0; SMP_MAX_CPUS],
    });

fn threadload_callback() {
    kprintln!(
        "cpu    load sched (cs ylds pmpts)   sysc ints (hw  tmr tmr_cb) ipi (rs  gen)     watts"
    );
    for i in 0..(PerCpu::processor_count() as cpu_num_t) {
        let idle_power_thread = PerCpu::get(i).idle_power_thread.thread();
        let mut stats = CpuStats::new();

        let maybe_idle_time: Option<zx_duration_mono_t> = {
            single_chain_lock_guard_owned!(
                thread_guard,
                idle_power_thread,
                IRQ_SAVE_OPTION,
                clt_tag!("g_threadload_callback")
            );

            Scheduler::run_in_locked_scheduler(i, |_scheduler| -> Option<zx_duration_mono_t> {
                // Don't display time for inactive cpus.
                if !scheduler::peek_is_active(i) {
                    return None;
                }

                // The scheduler queue lock synchronizes this copy.
                let percpu = PerCpu::get(i);
                // SAFETY: `percpu.stats.get()` points to a valid initialized `CpuStats`, and
                // `run_in_locked_scheduler` holds CPU `i`'s scheduler queue lock.
                stats = unsafe { CpuStats::copy_from_relaxed(percpu.stats.get()) };

                // If the cpu is currently idle, add the time since it went idle up until now to the
                // idle counter.
                if scheduler::peek_is_idle(i) {
                    // SAFETY: Called within `single_chain_lock_guard_owned!` with an active
                    // transaction, and `as_chain_lock()` is used only for a read-only
                    // `assert_held` check.
                    unsafe {
                        let token = ChainLockTransaction::assert_active();
                        idle_power_thread.lock().as_chain_lock().assert_held(&token);
                    }
                    let recent_idle_time =
                        current_mono_time().0.saturating_sub(stats.last_updated_instant);
                    return Some(stats.idle_time.saturating_add(recent_idle_time));
                }

                Some(stats.idle_time)
            })
        };

        let Some(idle_time) = maybe_idle_time else {
            continue;
        };

        let idx = i as usize;
        let (old_stats, last_idle_time) = {
            let lock = ThreadLoadStateLock::get();
            ksync::lock!(let mut state_guard = lock.lock());
            // SAFETY: `state_guard.token_mut()` belongs to the singleton `ThreadLoadStateLock`
            // that protects `THREADLOAD_STATE`.
            let state = unsafe { THREADLOAD_STATE.get_mut(state_guard.token_mut()) };
            (
                core::mem::replace(&mut state.old_stats[idx], stats),
                core::mem::replace(&mut state.last_idle_time[idx], idle_time),
            )
        };
        let delta_time = idle_time.saturating_sub(last_idle_time);
        let busy_time =
            if zx_sec(1) > delta_time { zx_sec(1).saturating_sub(delta_time) } else { 0 };
        let busypercent = busy_time.saturating_mul(10000) / zx_sec(1);

        let active_energy_delta_nj =
            stats.active_energy_consumption_nj.wrapping_sub(old_stats.active_energy_consumption_nj);
        let idle_energy_delta_nj =
            stats.idle_energy_consumption_nj.wrapping_sub(old_stats.idle_energy_consumption_nj);

        const NANOS_TO_MICROS: u64 = 1_000;
        const ROUND_TO_MICROS: u64 = 500;
        let total_power_uw =
            active_energy_delta_nj.wrapping_add(idle_energy_delta_nj).wrapping_add(ROUND_TO_MICROS)
                / NANOS_TO_MICROS;

        kprintln!(
            "{:3} {:3}.{:02}% {:9} {:4} {:5} {:7} {:8} {:4} {:6} {:8} {:4} {:3}.{:06}",
            i,
            (busypercent / 100) as u32,
            (busypercent % 100) as u32,
            stats.context_switches.wrapping_sub(old_stats.context_switches),
            stats.yields.wrapping_sub(old_stats.yields),
            stats.preempts.wrapping_sub(old_stats.preempts),
            stats.syscalls.wrapping_sub(old_stats.syscalls),
            stats.interrupts.wrapping_sub(old_stats.interrupts),
            stats.timer_ints.wrapping_sub(old_stats.timer_ints),
            stats.timers.wrapping_sub(old_stats.timers),
            stats.reschedule_ipis.wrapping_sub(old_stats.reschedule_ipis),
            stats.generic_ipis.wrapping_sub(old_stats.generic_ipis),
            (total_power_uw / 1_000_000) as u32,
            (total_power_uw % 1_000_000) as u32,
        );
    }
}

fn threadq_callback() {
    kprintln!("----------------------------------------------------");
    for i in 0..(PerCpu::processor_count() as cpu_num_t) {
        if scheduler::peek_is_active(i) {
            kprintln!("thread queue cpu {:2}:", i);
            PerCpu::get(i).scheduler.dump(false);
        }
    }
    kprintln!("");
}

fn threadq_stats_callback() {
    kprintln!("----------------------------------------------------");
    for i in 0..(PerCpu::processor_count() as cpu_num_t) {
        if scheduler::peek_is_active(i) {
            kprintln!("thread queue cpu {:2}:", i);
            PerCpu::get(i).scheduler.dump(true);
        }
    }
    kprintln!("");
}

static THREADLOAD_CALLBACK: StaticRecurringCallback =
    StaticRecurringCallback::new(threadload_callback);
static THREADQ_CALLBACK: StaticRecurringCallback = StaticRecurringCallback::new(threadq_callback);
static THREADQ_STATS_CALLBACK: StaticRecurringCallback =
    StaticRecurringCallback::new(threadq_stats_callback);

/// # Safety
///
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match the console
/// command signature.
unsafe extern "C" fn cmd_threadload(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    THREADLOAD_CALLBACK.toggle();
    0
}

/// # Safety
///
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match the console
/// command signature.
unsafe extern "C" fn cmd_threadq(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    THREADQ_CALLBACK.toggle();
    0
}

/// # Safety
///
/// Nothing: `argv` is never dereferenced. The function is `unsafe` only to match the console
/// command signature.
unsafe extern "C" fn cmd_threadqstats(_argc: c_int, _argv: *const CmdArgs, _flags: u32) -> c_int {
    THREADQ_STATS_CALLBACK.toggle();
    0
}

/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`CmdArgs`] elements, where `argc >= 1`
/// and each element's `arg_str` is a valid null-terminated C string.
unsafe extern "C" fn cmd_zmips(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The caller guarantees `argc >= 1` and `argv` points to at least `argc` initialized
    // `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("{:s} <cpu number>", argv[0].as_str());
        -1
    };
    if argc < 2 {
        kprintln!("Not enough arguments.");
        return usage();
    }

    let mut cpu_num = argv[1].arg_uint as cpu_num_t;
    if (cpu_num as usize) >= PerCpu::processor_count() {
        kprintln!(
            "CPU number must be in the range [{}, {}].",
            0usize,
            PerCpu::processor_count() - 1
        );
        return usage();
    }

    extern "C" fn calibrate(arg: *mut c_void) -> c_int {
        // SAFETY: `arg` points to `cpu_num` on `cmd_zmips`'s stack, which remains valid until
        // `thread.join` returns.
        let cpu_num = unsafe { *(arg as *const cpu_num_t) };

        // Busy loop for the given number of iterations.
        let delay = |mut loops: u64| {
            while loops != 0 {
                // SAFETY: Empty inline assembly with an inout register has no memory or state
                // side effects.
                unsafe { core::arch::asm!("/* {0} */", inout(reg) loops) };
                loops -= 1;
            }
        };

        type U30 = ffl::FastFixed<u64, 30>;
        let mut zmips_min = U30::max();
        let mut zmips_max = U30::min();

        let max_samples: c_int = 10;
        let max_loops: u64 = 1u64 << 48;
        let target_duration_ns: zx_duration_mono_t = zx_sec(1) / 20;

        for _i in 0..max_samples {
            // Quickly find the number of loops it takes for the delay loop to run for at least
            // the target duration by stepping in power of two increments, avoiding excessively
            // large values.
            let mut loops: u64 = 1;
            while loops < max_loops {
                // Disable interrupts to limit the noise of the measurement. The target
                // duration is selected to provide suitable precision without disabling
                // interrupts for too long to risk tripping software/hardware watchdogs.
                let interrupt_disable = InterruptDisableGuard::new();
                let start_ns = current_mono_time().0;
                delay(loops);
                let stop_ns = current_mono_time().0;
                drop(interrupt_disable);

                let duration_ns = stop_ns.saturating_sub(start_ns);
                if duration_ns >= target_duration_ns {
                    kprintln!(
                        "Calibrating CPU {}: {} loops per {} ns",
                        cpu_num,
                        loops,
                        duration_ns
                    );

                    // On simpler, in-order architectures the number of loops corresponds to
                    // half of the clock rate. Scale this value by two for minor convenience.
                    let zmips = U30::from(ffl::from_ratio(2 * loops, duration_ns as u64)) * 1000u64;
                    zmips_min = zmips_min.min(zmips);
                    zmips_max = zmips_max.max(zmips);
                    break;
                }
                loops *= 2;
            }
        }

        let zmips_min_str = ffl::String::new(zmips_min, ffl::Mode::Dec, Some(2));
        let zmips_max_str = ffl::String::new(zmips_max, ffl::Mode::Dec, Some(2));
        kprintln!(
            "Calibrated CPU {}: {:s}-{:s} ZMIPS",
            cpu_num,
            zmips_min_str.as_str(),
            zmips_max_str.as_str(),
        );

        0
    }

    // SAFETY: `calibrate` and `&raw mut cpu_num` are valid for the lifetime of the thread,
    // which is configured, resumed, and joined synchronously below before `cpu_num` goes out
    // of scope.
    unsafe {
        let Ok(thread) = create_with_priority(
            c"calibrate_zmips".as_ptr(),
            calibrate,
            (&raw mut cpu_num).cast::<c_void>(),
            DEFAULT_PRIORITY,
        ) else {
            kprintln!("Failed to create calibration thread!");
            return -1;
        };

        thread.set_cpu_affinity(cpu_num_to_mask(cpu_num));
        thread.resume();

        thread.join(InstantMono(ZX_TIME_INFINITE)).unwrap_or(-1)
    }
}

static_command!(
    CMD_THREAD,
    c"thread".as_ptr(),
    c"manipulate kernel threads".as_ptr(),
    cmd_thread,
    CMD_AVAIL_ALWAYS
);
static_command!(
    CMD_THREADSTATS,
    c"threadstats".as_ptr(),
    c"thread level statistics".as_ptr(),
    cmd_threadstats,
    CMD_AVAIL_NORMAL
);
static_command!(
    CMD_THREADLOAD,
    c"threadload".as_ptr(),
    c"toggle thread load display".as_ptr(),
    cmd_threadload,
    CMD_AVAIL_NORMAL
);
static_command!(
    CMD_THREADQ,
    c"threadq".as_ptr(),
    c"toggle thread queue display".as_ptr(),
    cmd_threadq,
    CMD_AVAIL_NORMAL
);
static_command!(
    CMD_THREADQSTATS,
    c"threadqstats".as_ptr(),
    c"toggle thread queue stats display".as_ptr(),
    cmd_threadqstats,
    CMD_AVAIL_NORMAL
);
static_command!(
    CMD_ZMIPS,
    c"zmips".as_ptr(),
    c"compute zmips of a cpu".as_ptr(),
    cmd_zmips,
    CMD_AVAIL_NORMAL
);
