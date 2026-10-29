// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#[cfg(console_enabled)]
use core::ffi::c_int;
#[cfg(console_enabled)]
use kprint::kprintln;
#[cfg(console_enabled)]
use zx_status::Status;
#[cfg(console_enabled)]
use zx_types::ZX_OK;

#[cfg(console_enabled)]
use super::idle_states::X86IdleStates;
#[cfg(console_enabled)]
use super::mp_ffi::{
    USE_MONITOR, X86_NUM_CPUS, cpp_x86_mp_ap_idle_states, cpp_x86_mp_bp_idle_states,
};
#[cfg(console_enabled)]
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
#[cfg(console_enabled)]
use crate::kernel::types::cpu_num_t;

/// Forcibly stops all other CPUs except the current one and the BSP (which is cpu 0).
///
/// # Safety
///
/// Caller must ensure that:
/// 1. The system is on a terminal halt or panic path where forcibly resetting other APs will not
///    violate kernel invariants required by the remaining execution on this CPU.
/// 2. Interrupts are disabled on the calling CPU so that it cannot migrate to another CPU while
///    iterating over APs.
#[inline]
pub unsafe fn x86_force_halt_all_but_local_and_bsp() {
    // SAFETY: Caller guarantees we are on a terminal halt/panic path with interrupts disabled.
    unsafe { super::mp_ffi::x86_force_halt_all_but_local_and_bsp() };
}

/// Returns whether `MONITOR`/`MWAIT` idle states are enabled on the system.
#[cfg(console_enabled)]
#[inline]
fn mp_use_monitor() -> bool {
    // SAFETY: `use_monitor` is written once during single-threaded early SMP boot.
    unsafe { core::ptr::addr_of!(USE_MONITOR).read() }
}

/// Returns the number of x86 CPUs (`x86_num_cpus`).
#[cfg(console_enabled)]
#[inline]
fn mp_num_cpus() -> u8 {
    // SAFETY: `x86_num_cpus` is written once during single-threaded early SMP boot.
    unsafe { core::ptr::addr_of!(X86_NUM_CPUS).read() }
}

/// Returns a reference to the bootstrap processor's `X86IdleStates` table.
#[cfg(console_enabled)]
fn bp_idle_states() -> &'static X86IdleStates {
    // SAFETY: `bp_percpu.idle_states` is initialized to `&fake_idle_states` statically and
    // optionally replaced during early SMP boot with a permanent allocation that lives for
    // `'static`.
    unsafe { cpp_x86_mp_bp_idle_states().as_ref().unwrap() }
}

/// Returns a reference to AP `idx`'s `X86IdleStates` table (`ap_percpus[idx].idle_states`).
#[cfg(console_enabled)]
fn ap_idle_states(idx: usize) -> &'static X86IdleStates {
    assert!(idx < (mp_num_cpus() as usize).saturating_sub(1));
    // SAFETY: `idx` is bounds-checked above. When non-null (i.e. when `use_monitor` is true),
    // `ap_percpus[idx].idle_states` was initialized during early SMP boot and lives for `'static`.
    unsafe { cpp_x86_mp_ap_idle_states(idx).as_ref().unwrap() }
}

#[cfg(console_enabled)]
fn reset_idle_counters(idle_states: &X86IdleStates) {
    for state in idle_states.states() {
        state.reset_counters();
    }
}

#[cfg(console_enabled)]
fn report_idlestates(cpu_num: cpu_num_t, idle_states: &X86IdleStates) {
    kprintln!("CPU {cpu_num}:");
    for state in idle_states.states() {
        let entered = state.times_entered();
        let total = state.cumulative_duration();
        let avg_ns = u64::try_from(total).unwrap_or(0).checked_div(entered).unwrap_or(0);
        kprintln!(
            "  {:>4s} (MWAIT {:02X}): {} entries, {} ns avg duration ({} ns total)",
            state.name(),
            state.mwait_hint(),
            entered,
            avg_ns,
            total
        );
    }
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_idlestates(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc <= 0 || argv.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements,
    // and `argc > 0` and `!argv.is_null()` are verified above.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("Usage: {:s} (printstats | resetstats | setmask)", argv[0].as_str());
        Status::INVALID_ARGS.into_raw()
    };
    'usage: {
        if argv.len() < 2 {
            break 'usage;
        }
        if !mp_use_monitor() {
            kprintln!("{:s} is only supported on systems with X86_FEATURE_MON", argv[0].as_str());
            return Status::NOT_SUPPORTED.into_raw();
        }
        if argv[1].as_str() == "resetstats" {
            reset_idle_counters(bp_idle_states());
            for i in 1..(mp_num_cpus() as cpu_num_t) {
                reset_idle_counters(ap_idle_states((i - 1) as usize));
            }
        } else if argv[1].as_str() == "printstats" {
            report_idlestates(0, bp_idle_states());
            for i in 1..(mp_num_cpus() as cpu_num_t) {
                report_idlestates(i, ap_idle_states((i - 1) as usize));
            }
        } else if argv[1].as_str() == "setmask" {
            if argv.len() < 3 {
                kprintln!("Usage: {:s} setmask $mask", argv[0].as_str());
                return Status::INVALID_ARGS.into_raw();
            }
            bp_idle_states().set_state_mask(argv[2].arg_uint as u32);
            for i in 1..(mp_num_cpus() as cpu_num_t) {
                ap_idle_states((i - 1) as usize).set_state_mask(argv[2].arg_uint as u32);
            }
        } else {
            break 'usage;
        }
        return ZX_OK;
    }

    usage()
}

#[cfg(console_enabled)]
static_command!(
    CMD_IDLESTATES,
    c"idlestates".as_ptr(),
    c"control or report on CPU idle state selection".as_ptr(),
    cmd_idlestates,
    CMD_AVAIL_NORMAL
);
