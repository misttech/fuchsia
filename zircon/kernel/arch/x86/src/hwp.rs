// Copyright 2017 The Fuchsia Authors
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_int;
use kprint::{kprint, kprintln};
use zx_status::Status;
use zx_types::{ZX_OK, zx_status_t};

use super::hwp_ffi::{
    cpp_x86_intel_hwp_init, cpp_x86_intel_hwp_parse_policy, cpp_x86_intel_hwp_supported,
    cpp_x86_make_hwp_request,
};
use super::platform_access::{MsrAccess, RealMsrAccess};
use super::registers::X86_MSR_IA32_HWP_REQUEST;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::mp::{MpIpiTarget, sync_exec};

const MAX_PERFORMANCE_EPP: u8 = 0x00;

/// Constructs a 64-bit `IA32_HWP_REQUEST` MSR value.
fn make_hwp_request(min_perf: u8, max_perf: u8, desired_perf: u8, epp: u8) -> u64 {
    // SAFETY: Pure bitfield packing function with no memory safety preconditions.
    unsafe { cpp_x86_make_hwp_request(min_perf, max_perf, desired_perf, epp) }
}

/// Returns whether Intel HWP is supported on the current CPU.
fn intel_hwp_supported() -> bool {
    // SAFETY: Reads CPUID feature leaves on the current CPU with no caller preconditions.
    unsafe { cpp_x86_intel_hwp_supported() }
}

fn cmd_print_usage() {
    kprint!(concat!(
        "usage:\n",
        "  hwp set-policy <policy-name>  - set performance policy\n",
        "                                  valid policies include bios-specified, performance,\n",
        "                                  balanced, power-save, stable-performance.\n",
        "  hwp set-freq <int>            - set processor frequency to given value.\n",
        "                                  values map directly onto frequency targets, but the \n",
        "                                  exact meaning is processor-dependant.\n",
    ));
}

fn cmd_set_policy(argv: &[CmdArgs], _flags: u32) -> zx_status_t {
    if argv.len() != 1 {
        cmd_print_usage();
        return Status::INVALID_ARGS.into_raw();
    }

    let mut policy: u8 = 0;
    // SAFETY: `argv[0].arg_str` is a valid C string from the console parser and `&mut policy` is a
    // valid stack reference.
    if !unsafe { cpp_x86_intel_hwp_parse_policy(argv[0].arg_str, &mut policy) } {
        kprintln!("Unknown policy '{:s}'.", argv[0].as_str());
        return Status::INVALID_ARGS.into_raw();
    }

    sync_exec(MpIpiTarget::All, 0, || {
        // SAFETY: `policy` was validated by `cpp_x86_intel_hwp_parse_policy` above.
        unsafe { cpp_x86_intel_hwp_init(policy) };
    });

    kprintln!("Policy updated to '{:s}'.", argv[0].as_str());
    ZX_OK
}

fn cmd_set_freq(argv: &[CmdArgs], _flags: u32) -> zx_status_t {
    if argv.len() != 1 {
        cmd_print_usage();
        return Status::INVALID_ARGS.into_raw();
    }

    let desired_freq = argv[0].arg_uint;
    if desired_freq == 0 || desired_freq >= 256 {
        kprintln!("Invalid frequency target.");
        return Status::INVALID_ARGS.into_raw();
    }

    sync_exec(MpIpiTarget::All, 0, || {
        let desired_freq = desired_freq as u8;
        let mut msr = RealMsrAccess;
        msr.write_msr(
            X86_MSR_IA32_HWP_REQUEST,
            make_hwp_request(desired_freq, desired_freq, desired_freq, MAX_PERFORMANCE_EPP),
        );
    });

    kprintln!("Frequency set to target {desired_freq}.");
    ZX_OK
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_hwp(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };

    // Ensure we have the hardware.
    if !intel_hwp_supported() {
        kprintln!("HWP not supported on system.");
        return Status::NOT_SUPPORTED.into_raw();
    }

    // Each command needs at least two tokens: "hwp <subcommand>".
    if argv.len() < 2 {
        cmd_print_usage();
        return Status::INVALID_ARGS.into_raw();
    }

    let subcommand = argv[1].as_str();
    if subcommand == "set-policy" {
        return cmd_set_policy(&argv[2..], flags);
    }

    if subcommand == "set-freq" {
        return cmd_set_freq(&argv[2..], flags);
    }

    cmd_print_usage();
    Status::INVALID_ARGS.into_raw()
}

static_command!(
    CMD_HWP,
    c"hwp".as_ptr(),
    c"hardware controlled performance states\n".as_ptr(),
    cmd_hwp,
    CMD_AVAIL_NORMAL
);
