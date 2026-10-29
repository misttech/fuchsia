// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2015 Intel Corporation
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_int;
use kprint::kprintln;
use zr::OpaqueBytes;
use zx_status::Status;
use zx_types::ZX_OK;

use super::Iframe;
use super::arch_ffi::{
    cpp_x86_backtrace_print, cpp_x86_feature_debug, cpp_x86_print_frame, cpp_x86_request_context,
};
use super::msr::{read_msr_safe_on_cpu, write_msr_safe_on_cpu};
use crate::arch_rs::InterruptDisableGuard;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::mp::is_cpu_online;
use crate::kernel::percpu::PerCpu;
use crate::kernel::types::cpu_num_t;
use crate::platform_rs::timer::DurationMono;

/// Prints an x86 interrupt frame to the kernel console.
fn print_frame(frame: &Iframe) {
    // SAFETY: `frame` is a valid `Iframe` reference.
    unsafe { cpp_x86_print_frame(frame) };
}

/// Prints x86 CPU feature and vulnerability information to the kernel console.
fn feature_debug() {
    // SAFETY: Reads CPUID/feature state initialized during early boot with no caller preconditions.
    unsafe { cpp_x86_feature_debug() };
}

#[repr(C, align(8))]
#[derive(Default)]
pub(super) struct Backtrace(OpaqueBytes<272>);

impl Backtrace {
    fn print(&self) {
        // SAFETY: `self` is a valid initialized `Backtrace`.
        unsafe { cpp_x86_backtrace_print(self) };
    }
}

#[repr(C)]
#[derive(Default)]
pub(super) struct CpuContext {
    frame: Iframe,
    backtrace: Backtrace,
}

zr::static_assert!(core::mem::size_of::<Backtrace>() == 272);
zr::static_assert!(core::mem::align_of::<Backtrace>() == 8);
zr::static_assert!(core::mem::size_of::<CpuContext>() == 448);
zr::static_assert!(core::mem::align_of::<CpuContext>() == 8);

fn get_context(argv: &[CmdArgs], _flags: u32) -> c_int {
    let print_usage = || {
        kprintln!("usage:");
        kprintln!("{:s} context <cpu_id> <timeout_ms>", argv[0].as_str());
    };

    if argv.len() < 4 {
        kprintln!("not enough arguments");
        print_usage();
        return 1;
    }

    let target = argv[2].arg_uint as cpu_num_t;
    if (target as usize) >= PerCpu::processor_count() {
        kprintln!("invalid cpu_id: {target}");
        return 1;
    }

    let timeout_ms = argv[3].arg_uint;
    kprintln!("requesting context of CPU-{target} with timeout {timeout_ms}ms");

    let mut context = CpuContext::default();
    let status = {
        let _irqd = InterruptDisableGuard::new();
        // SAFETY: Interrupts are disabled by `_irqd`, `target < PerCpu::processor_count()`, and
        // `&mut context` is a valid mutable `CpuContext` reference.
        unsafe {
            cpp_x86_request_context(
                target,
                DurationMono::from_millis(timeout_ms as i64).0,
                &mut context,
            )
        }
    };
    if status != ZX_OK {
        kprintln!("error: {status}");
        return 1;
    }
    context.backtrace.print();
    print_frame(&context.frame);

    0
}

/// Kernel console command handler for `cpu` subcommands.
///
/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_cpu(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
    if argc <= 0 || argv.is_null() {
        core::hint::cold_path();
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements,
    // and `argc > 0` and `!argv.is_null()` are verified above.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        core::hint::cold_path();
        kprintln!("usage:");
        kprintln!("{:s} features", argv[0].as_str());
        kprintln!("{:s} rdmsr <cpu_id> <msr_id>", argv[0].as_str());
        kprintln!("{:s} wrmsr <cpu_id> <msr_id> <value>", argv[0].as_str());
        kprintln!("{:s} lbr <subcommand>", argv[0].as_str());
        kprintln!("{:s} context <cpu_id> <timeout_ms>", argv[0].as_str());
        Status::INVALID_ARGS.into_raw()
    };
    'usage: {
        if argv.len() < 2 {
            kprintln!("not enough arguments");
            break 'usage;
        }

        if argv[1].as_str() == "features" {
            feature_debug();
        } else if argv[1].as_str() == "rdmsr" {
            if argv.len() != 4 {
                break 'usage;
            }
            let Ok(cpu) = cpu_num_t::try_from(argv[2].arg_uint) else {
                kprintln!("invalid cpu_id: {}", argv[2].arg_uint);
                return Status::INVALID_ARGS.into_raw();
            };
            if !is_cpu_online(cpu) {
                kprintln!("CPU {cpu} is not online");
                return Status::BAD_STATE.into_raw();
            }
            let Ok(msr_id) = u32::try_from(argv[3].arg_uint) else {
                kprintln!("invalid msr_id: {:x}h", argv[3].arg_uint);
                return Status::INVALID_ARGS.into_raw();
            };

            // SAFETY: Operator-invoked kernel debug console command; `read_msr_safe_on_cpu`
            // catches `#GP(0)` faults on unimplemented or inaccessible MSRs.
            match unsafe { read_msr_safe_on_cpu(cpu, msr_id) } {
                Ok(val) => {
                    kprintln!("CPU {cpu} RDMSR {msr_id:x}h val {val:x}h");
                }
                Err(err) => {
                    kprintln!("CPU {cpu} RDMSR {msr_id:x}h failed: {}", err.into_raw());
                    return err.into_raw();
                }
            }
        } else if argv[1].as_str() == "wrmsr" {
            if argv.len() != 5 {
                break 'usage;
            }
            let Ok(cpu) = cpu_num_t::try_from(argv[2].arg_uint) else {
                kprintln!("invalid cpu_id: {}", argv[2].arg_uint);
                return Status::INVALID_ARGS.into_raw();
            };
            if !is_cpu_online(cpu) {
                kprintln!("CPU {cpu} is not online");
                return Status::BAD_STATE.into_raw();
            }
            let Ok(msr_id) = u32::try_from(argv[3].arg_uint) else {
                kprintln!("invalid msr_id: {:x}h", argv[3].arg_uint);
                return Status::INVALID_ARGS.into_raw();
            };
            let val = argv[4].arg_uint;

            kprintln!("CPU {cpu} WRMSR {msr_id:x}h val {val:x}h");
            // SAFETY: Operator-invoked kernel debug console command; `write_msr_safe_on_cpu`
            // catches `#GP(0)` faults on unimplemented MSRs or invalid `val` bits.
            if let Err(err) = unsafe { write_msr_safe_on_cpu(cpu, msr_id, val) } {
                kprintln!("CPU {cpu} WRMSR {msr_id:x}h failed: {}", err.into_raw());
                return err.into_raw();
            }
        } else if argv[1].as_str() == "context" {
            return get_context(argv, flags);
        } else {
            kprintln!("unknown command");
            break 'usage;
        }

        return ZX_OK;
    }

    usage()
}

static_command!(CMD_CPU, c"cpu".as_ptr(), c"cpu test commands".as_ptr(), cmd_cpu, CMD_AVAIL_NORMAL);
