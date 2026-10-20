// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Idle power thread facade and console commands.

use percpu_bindings as bindings;
use zx_types::zx_instant_boot_t;

use crate::kernel::thread::Thread;
use crate::wake_vector::WakeResult;

unsafe extern "C" {
    fn cpp_idle_power_thread_trigger_system_wake_event() -> WakeResult;
    fn cpp_idle_power_thread_acknowledge_system_wake_event();
    fn cpp_idle_power_thread_transition_all_active_to_suspend(
        resume_deadline: zx_instant_boot_t,
    ) -> zx_instant_boot_t;
    fn cpp_idle_power_thread_suspend_system(
        resume_deadline: zx_instant_boot_t,
    ) -> zx_instant_boot_t;
    fn cpp_idle_power_thread_get_thread(ipt: *const IdlePowerThread) -> *const Thread;
}

/// An opaque type representing the C++ `IdlePowerThread` class.
#[repr(transparent)]
pub struct IdlePowerThread(pub bindings::IdlePowerThread);

impl IdlePowerThread {
    /// Triggers a system wake event, waking the boot CPU if the system is suspended and this is
    /// the first pending wake event.
    #[inline]
    pub(crate) fn trigger_system_wake_event() -> WakeResult {
        // SAFETY: Foreign function call into `IdlePowerThread::TriggerSystemWakeEvent`.
        unsafe { cpp_idle_power_thread_trigger_system_wake_event() }
    }

    /// Acknowledges a pending system wake event.
    #[inline]
    pub(crate) fn acknowledge_system_wake_event() {
        // SAFETY: Foreign function call into `IdlePowerThread::AcknowledgeSystemWakeEvent`.
        unsafe { cpp_idle_power_thread_acknowledge_system_wake_event() }
    }

    /// Transitions all active CPUs into the suspend state until `resume_deadline` or a wake event
    /// occurs, returning the boot timestamp when the suspend transition started.
    #[inline]
    pub(crate) fn transition_all_active_to_suspend(
        resume_deadline: zx_instant_boot_t,
    ) -> zx_instant_boot_t {
        // SAFETY: Foreign function call into `IdlePowerThread::TransitionAllActiveToSuspend`.
        unsafe { cpp_idle_power_thread_transition_all_active_to_suspend(resume_deadline) }
    }

    /// Suspends the entire system until `resume_deadline` or a wake event occurs, returning the
    /// boot timestamp when the suspend transition started.
    #[inline]
    pub(crate) fn suspend_system(resume_deadline: zx_instant_boot_t) -> zx_instant_boot_t {
        // SAFETY: Foreign function call into `IdlePowerThread::SuspendSystem`.
        unsafe { cpp_idle_power_thread_suspend_system(resume_deadline) }
    }

    /// Accessor to the underlying [`Thread`] instance.
    #[inline]
    pub fn thread(&self) -> &Thread {
        // SAFETY: `self` points to a valid `IdlePowerThread` whose embedded `Thread` outlives
        // `&self`.
        unsafe { &*cpp_idle_power_thread_get_thread(self) }
    }
}

/// # Safety
///
/// `argv` must point to a valid array of at least `argc` [`crate::console::CmdArgs`] elements,
/// where `argc >= 1` and each element's `arg_str` is a valid null-terminated C string.
#[cfg(console_enabled)]
unsafe extern "C" fn cmd_suspend(
    argc: core::ffi::c_int,
    argv: *const crate::console::CmdArgs,
    _flags: u32,
) -> core::ffi::c_int {
    use crate::platform_rs::timer::{current_boot_time, zx_min, zx_msec, zx_nsec, zx_sec, zx_usec};
    use crate::wake_vector::WakeEvent;
    use core::ffi::c_int;
    use kprint::kprintln;
    use zx_status::Status;
    use zx_types::{ZX_OK, zx_duration_boot_t};

    // SAFETY: The caller guarantees `argc >= 1` and `argv` points to at least `argc` initialized
    // `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!("{:s} enter <resume delay> <m|s|ms|us|ns> [cpu|system]", argv[0].as_str());
        kprintln!("{:s} wake-events", argv[0].as_str());
        Status::INTERNAL.into_raw()
    };

    if argc < 2 {
        return usage();
    }

    if argv[1].as_str() == "enter" {
        if !(4..=5).contains(&argc) {
            return usage();
        }

        let mut suspend_system = false;
        if argc == 5 {
            if argv[4].as_str() == "system" {
                suspend_system = true;
            } else if argv[4].as_str() != "cpu" {
                kprintln!("Invalid mode: {:s}", argv[4].as_str());
                return usage();
            }
        }

        let mut delay: zx_duration_boot_t = argv[2].arg_int;
        let units = argv[3].as_str();
        if units == "m" {
            delay = zx_min(delay);
        } else if units == "s" {
            delay = zx_sec(delay);
        } else if units == "ms" {
            delay = zx_msec(delay);
        } else if units == "us" {
            delay = zx_usec(delay);
        } else if units == "ns" {
            delay = zx_nsec(delay);
        } else {
            kprintln!("Invalid units: {:s}", units);
            return usage();
        }

        let resume_at = current_boot_time().0.saturating_add(delay);
        if suspend_system {
            IdlePowerThread::suspend_system(resume_at);
        } else {
            IdlePowerThread::transition_all_active_to_suspend(resume_at);
        }
        return ZX_OK;
    }

    if argv[1].as_str() == "wake-events" {
        if argc != 2 {
            return usage();
        }

        kprintln!("Wake events:");
        struct Stdout;
        impl core::fmt::Write for Stdout {
            fn write_str(&mut self, s: &str) -> core::fmt::Result {
                kprint::kprint!("{:s}", s);
                Ok(())
            }
        }
        WakeEvent::dump(&mut Stdout, 0);
        return 0;
    }

    kprintln!("unknown command");
    usage()
}

#[cfg(console_enabled)]
crate::console::static_command!(
    CMD_SUSPEND,
    c"suspend".as_ptr(),
    c"processor suspend commands".as_ptr(),
    cmd_suspend,
    crate::console::CMD_AVAIL_ALWAYS
);
