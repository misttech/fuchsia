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
    cpp_x86_backtrace_print, cpp_x86_feature_debug, cpp_x86_print_frame, cpp_x86_read_msr_on_cpu,
    cpp_x86_request_context, cpp_x86_write_msr_on_cpu,
};
use crate::arch_rs::InterruptDisableGuard;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
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

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_cpu(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!("{:s} features", argv[0].as_str());
        kprintln!("{:s} rdmsr <cpu_id> <msr_id>", argv[0].as_str());
        kprintln!("{:s} wrmsr <cpu_id> <msr_id> <value>", argv[0].as_str());
        kprintln!("{:s} lbr <subcommand>", argv[0].as_str());
        kprintln!("{:s} context <cpu_id> <timeout_ms>", argv[0].as_str());
        Status::INTERNAL.into_raw()
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

            // SAFETY: Console diagnostic command requested reading MSR `argv[3]` on CPU `argv[2]`.
            let val = unsafe {
                cpp_x86_read_msr_on_cpu(argv[2].arg_uint as u32, argv[3].arg_uint as u32)
            };
            kprintln!("CPU {} RDMSR {:x}h val {:x}h", argv[2].arg_uint, argv[3].arg_uint, val);
        } else if argv[1].as_str() == "wrmsr" {
            if argv.len() != 5 {
                break 'usage;
            }

            kprintln!(
                "CPU {} WRMSR {:x}h val {:x}h",
                argv[2].arg_uint,
                argv[3].arg_uint,
                argv[4].arg_uint
            );
            // SAFETY: Console diagnostic command requested writing MSR `argv[3]` on CPU `argv[2]`.
            unsafe {
                cpp_x86_write_msr_on_cpu(
                    argv[2].arg_uint as u32,
                    argv[3].arg_uint as u32,
                    argv[4].arg_uint,
                );
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
