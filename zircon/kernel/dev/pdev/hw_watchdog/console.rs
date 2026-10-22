// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{
    hw_watchdog_get_last_pet_time, hw_watchdog_get_timeout_nsec, hw_watchdog_is_enabled,
    hw_watchdog_is_petting_suppressed, hw_watchdog_pet, hw_watchdog_present,
    hw_watchdog_set_enabled, hw_watchdog_suppress_petting,
};
use crate::arch_rs;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::debuglog_rs::dlog_panic_start;
use crate::kernel::thread;
use crate::kernel::types::BOOT_CPU_ID;
use crate::platform_rs::halt_helper::platform_halt_secondary_cpus;
use crate::platform_rs::timer::{
    InstantMono, current_boot_time, current_mono_time, zx_msec, zx_sec,
};
use core::ffi::{CStr, c_int};
use kprint::kprint;
use zx_status::Status;

fn usage(cmd_name: &CStr) {
    kprint!("Usage:\n");
    kprint!(
        "{:s} status   : show the recent status of the hardware watchdog subsystem.\n",
        cmd_name
    );
    kprint!("{:s} pet      : force an immediate pet of the watchdog.\n", cmd_name);
    kprint!("{:s} enable   : attempt to enable the watchdog.\n", cmd_name);
    kprint!("{:s} disable  : attempt to disable the watchdog.\n", cmd_name);
    kprint!("{:s} force    : force the watchdog to fire by locking up all cores.\n", cmd_name);
    kprint!("{:s} suppress : Pet the WDT one last time, then suppress future pets.\n", cmd_name);
    kprint!("{:s} help     : show this message.\n", cmd_name);
}

enum Cmd {
    Status,
    Pet,
    Enable,
    Disable,
    Force,
    Suppress,
}

/// # Safety
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_watchdog(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console framework passes `argc` valid arguments in `argv`.
    let args = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    // SAFETY: The console framework passes null-terminated argument strings.
    let cmd_name = unsafe { CStr::from_ptr(args[0].arg_str) };

    if argc < 2 {
        kprint!("Not enough arguments.\n");
        usage(cmd_name);
        return -1;
    }

    // SAFETY: The console framework passes null-terminated argument strings.
    let arg1 = unsafe { CStr::from_ptr(args[1].arg_str) };
    let command = if arg1 == c"status" {
        Cmd::Status
    } else if arg1 == c"pet" {
        Cmd::Pet
    } else if arg1 == c"enable" {
        Cmd::Enable
    } else if arg1 == c"disable" {
        Cmd::Disable
    } else if arg1 == c"force" {
        Cmd::Force
    } else if arg1 == c"suppress" {
        Cmd::Suppress
    } else if arg1 == c"help" {
        usage(cmd_name);
        return 0;
    } else {
        kprint!("Unrecognized command.\n");
        usage(cmd_name);
        return -1;
    };

    // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
    if !unsafe { hw_watchdog_present() } {
        kprint!("There is no hardware watchdog present in this system.\n");
        return 0;
    }

    match command {
        Cmd::Status => {
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            let last_pet = unsafe { hw_watchdog_get_last_pet_time() };
            let now = current_boot_time().0;
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            kprint!(
                "Enabled  : {:s}\n",
                if unsafe { hw_watchdog_is_enabled() } { "yes" } else { "no" }
            );
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            kprint!("Timeout  : {} mSec\n", unsafe { hw_watchdog_get_timeout_nsec() } / zx_msec(1));
            kprint!(
                "Last Pet : {}.{:03} ({} mSec ago)\n",
                last_pet / zx_sec(1),
                (last_pet / zx_msec(1)) % 1000,
                (now - last_pet) / zx_msec(1)
            );
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            kprint!(
                "Petting  : {:s}\n",
                if unsafe { hw_watchdog_is_petting_suppressed() } {
                    "Suppressed"
                } else {
                    "Enabled"
                }
            );
        }

        Cmd::Pet => {
            kprint!("Watchdog has been pet.  They're a good dog! (yes they are!!)\n");
        }

        Cmd::Enable => {
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            let res = unsafe { hw_watchdog_set_enabled(true) };
            if res == Err(Status::NOT_SUPPORTED) {
                kprint!("Watchdog does not support enabling.\n");
                return Status::result_into_raw(res);
            } else if res != Ok(()) {
                kprint!("Error enabling watchdog ({})\n", Status::result_into_raw(res));
                return Status::result_into_raw(res);
            } else {
                kprint!("Watchdog enabled.\n");
            }
        }

        Cmd::Disable => {
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            let res = unsafe { hw_watchdog_set_enabled(false) };
            if res == Err(Status::NOT_SUPPORTED) {
                kprint!("Watchdog does not support disabling.\n");
                return Status::result_into_raw(res);
            } else if res != Ok(()) {
                kprint!("Error disabling watchdog ({})\n", Status::result_into_raw(res));
                return Status::result_into_raw(res);
            } else {
                kprint!("Watchdog disabled.\n");
            }
        }

        Cmd::Force => {
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            if !unsafe { hw_watchdog_is_enabled() } {
                kprint!("Watchdog is not enabled.  Enable the watchdog first.\n");
                return Status::BAD_STATE.into_raw();
            }

            // In order to _really_ wedge the system we...
            // 1) Migrate our thread to the boot core. (this pins the thread there too)
            // 2) Halt all of the secondary cores
            // 3) Disable interrupts.
            // 4) Spin forever.
            //
            // SAFETY: `BOOT_CPU_ID` is active, and console commands run in normal thread context
            // with no spinlocks or `ChainLockTransaction` held.
            unsafe { thread::migrate_to_cpu(BOOT_CPU_ID) };
            let status = platform_halt_secondary_cpus(InstantMono::INFINITE);
            debug_assert!(status == Ok(()));

            arch_rs::disable_ints();

            // Make sure that our kprint goes directly to the UART, bypassing any
            // buffering which is not going to get drained now that we have stopped
            // the system.
            dlog_panic_start();

            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            let deadline = unsafe { hw_watchdog_get_last_pet_time() }
                .saturating_add(unsafe { hw_watchdog_get_timeout_nsec() });
            kprint!(
                "System wedged!  Watchdog will fire in {} mSec\n",
                (deadline - current_mono_time().0) / zx_msec(1)
            );

            // Spin forever.  The watchdog should reboot us.
            loop {
                libarch::r#yield();
            }
        }

        Cmd::Suppress => {
            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            unsafe {
                hw_watchdog_pet();
                hw_watchdog_suppress_petting(true);
            }

            // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
            if !unsafe { hw_watchdog_is_enabled() } {
                kprint!("WDT petting is now suppressed, but the WDT is not currently enabled.\n");
            } else {
                // SAFETY: The layer always resolves a `'static` driver, set once in early boot.
                let deadline = unsafe { hw_watchdog_get_last_pet_time() }
                    .saturating_add(unsafe { hw_watchdog_get_timeout_nsec() });
                kprint!(
                    "WDT petting is now suppressed, WDT will fire in {} mSec\n",
                    (deadline - current_mono_time().0) / zx_msec(1)
                );
            }
        }
    }

    0
}

static_command!(
    CMD_WDT,
    c"wdt".as_ptr(),
    c"hardware watchdog commands".as_ptr(),
    cmd_watchdog,
    CMD_AVAIL_NORMAL
);
