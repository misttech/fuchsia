// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_int;
use debug::dprintf;
use zx_status::Status;
use zx_types::ZX_OK;

use super::system_x86_ffi::{
    cpp_x86_clear_limit_reason_log, cpp_x86_power_limit_suported, cpp_x86_print_limit_reasons,
    cpp_x86_print_limits, cpp_x86_status_callback_toggle,
};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};

/// Returns whether RAPL power limit controls are supported on the current x86
/// microarchitecture.
fn power_limit_suported() -> bool {
    // SAFETY: Queries x86 microarchitecture configuration initialized during early boot.
    unsafe { cpp_x86_power_limit_suported() }
}

/// Toggles the recurring RAPL energy status callback.
fn status_callback_toggle() {
    // SAFETY: Synchronizes recurring callback state internally with no caller preconditions.
    unsafe { cpp_x86_status_callback_toggle() };
}

/// Prints CPU and graphics performance limit reasons from architectural MSRs.
fn print_limit_reasons(use_log: bool) {
    // SAFETY: Checks microarchitecture support internally before reading MSRs; no caller
    // preconditions.
    unsafe { cpp_x86_print_limit_reasons(use_log) };
}

/// Clears the latched CPU and graphics performance limit reason log MSR bits.
fn clear_limit_reason_log() {
    // SAFETY: Checks microarchitecture support internally before writing MSRs; no caller
    // preconditions.
    unsafe { cpp_x86_clear_limit_reason_log() };
}

fn print_command_usage() {
    struct Subcommand {
        cmd_str: &'static str,
        help_str: &'static str,
    }
    const SUBCOMMANDS: [Subcommand; 4] = [
        Subcommand { cmd_str: "status", help_str: "toggle status display" },
        Subcommand { cmd_str: "limitreason clear", help_str: "clear the cpu limit reason log" },
        Subcommand {
            cmd_str: "limitreason log",
            help_str: "print all cpu limit reasons since last clear",
        },
        Subcommand { cmd_str: "limits", help_str: "print package power limits" },
    ];
    dprintf!(ALWAYS, "usage:\n");
    for subcommand in SUBCOMMANDS {
        dprintf!(ALWAYS, "\tpower {:<32}: {}\n", subcommand.cmd_str, subcommand.help_str);
    }
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_power(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    if argv.len() < 2 {
        print_command_usage();
        return Status::INVALID_ARGS.into_raw();
    }

    if !power_limit_suported() {
        dprintf!(ALWAYS, "Power limit controls not supported\n");
        return Status::NOT_SUPPORTED.into_raw();
    }

    if argv[1].as_str() == "status" {
        status_callback_toggle();
        return ZX_OK;
    } else if argv[1].as_str() == "limitreason" {
        if argv.len() < 3 {
            print_command_usage();
            return Status::INVALID_ARGS.into_raw();
        }
        if argv[2].as_str() == "log" {
            print_limit_reasons(true);
            return ZX_OK;
        } else if argv[2].as_str() == "clear" {
            clear_limit_reason_log();
            return ZX_OK;
        }
    } else if argv[1].as_str() == "limits" {
        // SAFETY: `power_limit_suported()` was verified above.
        unsafe { cpp_x86_print_limits() };
        return ZX_OK;
    }

    print_command_usage();
    Status::INVALID_ARGS.into_raw()
}

static_command!(
    CMD_POWER,
    c"power".as_ptr(),
    c"power limiting debug commands (for x86 only)".as_ptr(),
    cmd_power,
    CMD_AVAIL_NORMAL
);
