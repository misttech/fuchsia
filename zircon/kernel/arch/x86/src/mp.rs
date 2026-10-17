// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::{CStr, c_int};
use kprint::kprintln;
use zr::OpaqueBytes;
use zx_status::Status;
use zx_types::{ZX_OK, zx_duration_t};

use super::feature::X86_MAX_CSTATES;
use super::mp_ffi::{
    USE_MONITOR, X86_NUM_CPUS, cpp_x86_idle_state_cumulative_duration,
    cpp_x86_idle_state_mwait_hint, cpp_x86_idle_state_name, cpp_x86_idle_state_reset_counters,
    cpp_x86_idle_state_times_entered, cpp_x86_idle_states_num_states,
    cpp_x86_idle_states_set_state_mask, cpp_x86_idle_states_states, cpp_x86_mp_ap_idle_states,
    cpp_x86_mp_bp_idle_states,
};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::types::cpu_num_t;

/// Returns whether `MONITOR`/`MWAIT` idle states are enabled on the system.
#[inline]
fn mp_use_monitor() -> bool {
    // SAFETY: `use_monitor` is written once during single-threaded early SMP boot.
    unsafe { core::ptr::addr_of!(USE_MONITOR).read() }
}

/// Returns the number of x86 CPUs (`x86_num_cpus`).
#[inline]
fn mp_num_cpus() -> u8 {
    // SAFETY: `x86_num_cpus` is written once during single-threaded early SMP boot.
    unsafe { core::ptr::addr_of!(X86_NUM_CPUS).read() }
}

/// Returns a reference to the bootstrap processor's `X86IdleStates` table.
fn bp_idle_states() -> &'static X86IdleStates {
    // SAFETY: `bp_percpu.idle_states` is initialized during early boot and lives for `'static`.
    unsafe { &*cpp_x86_mp_bp_idle_states() }
}

/// Returns a reference to AP `idx`'s `X86IdleStates` table (`ap_percpus[idx].idle_states`).
fn ap_idle_states(idx: usize) -> &'static X86IdleStates {
    assert!(idx < (mp_num_cpus() as usize).saturating_sub(1));
    // SAFETY: `idx` is bounds-checked above and `ap_percpus[idx].idle_states` is initialized during
    // early SMP boot and lives for `'static`.
    unsafe { &*cpp_x86_mp_ap_idle_states(idx) }
}

#[repr(C, align(8))]
pub(super) struct X86IdleState(OpaqueBytes<24>);

impl X86IdleState {
    fn name(&self) -> &str {
        // SAFETY: `self` points to a valid `X86IdleState` whose `Name()` is a valid static C
        // string.
        unsafe { CStr::from_ptr(cpp_x86_idle_state_name(self)) }.to_str().unwrap_or("")
    }

    /// Returns the hint to provide to MWAIT to enter this state.
    fn mwait_hint(&self) -> u32 {
        // SAFETY: `self` points to a valid `X86IdleState`.
        unsafe { cpp_x86_idle_state_mwait_hint(self) }
    }

    /// Returns the number of times the system entered this state since the last
    /// call to ResetCounters().
    fn times_entered(&self) -> u64 {
        // SAFETY: `self` points to a valid `X86IdleState`.
        unsafe { cpp_x86_idle_state_times_entered(self) }
    }

    /// Returns total time the system has spent in this state since the last
    /// call to ResetCounters()
    fn cumulative_duration(&self) -> zx_duration_t {
        // SAFETY: `self` points to a valid `X86IdleState`.
        unsafe { cpp_x86_idle_state_cumulative_duration(self) }
    }

    /// Resets the entry and duration counters.
    fn reset_counters(&self) {
        // SAFETY: `self` wraps `OpaqueBytes<24>` (`UnsafeCell`) and points to a valid
        // `X86IdleState`.
        unsafe { cpp_x86_idle_state_reset_counters(core::ptr::from_ref(self).cast_mut()) };
    }
}

#[repr(C, align(8))]
pub(super) struct X86IdleStates(OpaqueBytes<312>);

impl X86IdleStates {
    /// Returns the list of states supported by the CPU, with the same
    /// ordering constraints as documented for x86_idle_states_t.
    fn states(&self) -> &[X86IdleState; X86_MAX_CSTATES] {
        // SAFETY: `cpp_x86_idle_states_states` returns a pointer to the `X86_MAX_CSTATES`-element
        // `states_` array inside `self`.
        unsafe { &*cpp_x86_idle_states_states(core::ptr::from_ref(self).cast_mut()).cast() }
    }

    fn num_states(&self) -> usize {
        // SAFETY: `self` points to a valid `X86IdleStates`.
        unsafe { cpp_x86_idle_states_num_states(self) }
    }

    /// Updates the mask of valid C-states.
    fn set_state_mask(&self, mask: u32) {
        // SAFETY: `self` wraps `OpaqueBytes<312>` (`UnsafeCell`) and `SetStateMask` writes to
        // `ktl::atomic<uint32_t> state_mask_`.
        unsafe { cpp_x86_idle_states_set_state_mask(core::ptr::from_ref(self).cast_mut(), mask) };
    }
}

zr::static_assert!(core::mem::size_of::<X86IdleState>() == 24);
zr::static_assert!(core::mem::align_of::<X86IdleState>() == 8);
zr::static_assert!(core::mem::size_of::<X86IdleStates>() == 312);
zr::static_assert!(core::mem::align_of::<X86IdleStates>() == 8);

fn reset_idle_counters(idle_states: &X86IdleStates) {
    for i in 0..idle_states.num_states() {
        idle_states.states()[i].reset_counters();
    }
}

#[allow(clippy::needless_range_loop, clippy::manual_checked_ops)]
fn report_idlestates(cpu_num: cpu_num_t, idle_states: &X86IdleStates) {
    kprintln!("CPU {cpu_num}:");
    let states = idle_states.states();
    for i in 0..idle_states.num_states() {
        let state = &states[i];
        kprintln!(
            "  {:>4s} (MWAIT {:02X}): {} entries, {} ns avg duration ({} ns total)",
            state.name(),
            state.mwait_hint(),
            state.times_entered(),
            if state.times_entered() > 0 {
                (state.cumulative_duration() as u64) / state.times_entered()
            } else {
                0
            },
            state.cumulative_duration()
        );
    }
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_idlestates(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements.
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

static_command!(
    CMD_IDLESTATES,
    c"idlestates".as_ptr(),
    c"control or report on CPU idle state selection".as_ptr(),
    cmd_idlestates,
    CMD_AVAIL_NORMAL
);
