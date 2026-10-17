// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_int;
use kprint::kprintln;
use zx_status::Status;
use zx_types::ZX_OK;

use super::mmu_mem_types_ffi::{G_MAX_PADDR_WIDTH, NUM_VARIABLE, SUPPORTS_FIXED_RANGE};
use super::registers::{
    X86_MSR_IA32_MTRR_DEF_TYPE, X86_MSR_IA32_MTRR_FIX4K_C0000, X86_MSR_IA32_MTRR_FIX16K_80000,
    X86_MSR_IA32_MTRR_FIX64K_00000, X86_MSR_IA32_MTRR_PHYSBASE0, X86_MSR_IA32_MTRR_PHYSMASK0,
    X86_MSR_IA32_PAT,
};
use super::x86::read_msr;
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::mp::{MpIpiTarget, sync_exec};

/// Returns whether fixed-range MTRRs are supported on the CPU.
#[inline]
fn mmu_supports_fixed_range() -> bool {
    // SAFETY: `supports_fixed_range` is written once during single-threaded early boot.
    unsafe { core::ptr::addr_of!(SUPPORTS_FIXED_RANGE).read() }
}

/// Returns the number of variable-range MTRRs supported on the CPU.
#[inline]
fn mmu_num_variable() -> u8 {
    // SAFETY: `num_variable` is written once during single-threaded early boot.
    unsafe { core::ptr::addr_of!(NUM_VARIABLE).read() }
}

/// Returns the maximum physical address width (`g_max_paddr_width`) on the CPU.
#[inline]
fn mmu_max_paddr_width() -> u8 {
    // SAFETY: `g_max_paddr_width` is written once during single-threaded early boot.
    unsafe { core::ptr::addr_of!(G_MAX_PADDR_WIDTH).read() }
}

const IA32_MTRR_NUM_FIX16K: u32 = 2;
const IA32_MTRR_NUM_FIX4K: u32 = 8;

macro_rules! ia32_mtrr_fix16k_80000 {
    ($x:expr) => {
        X86_MSR_IA32_MTRR_FIX16K_80000 + ($x)
    };
}

macro_rules! ia32_mtrr_fix4k_c0000 {
    ($x:expr) => {
        X86_MSR_IA32_MTRR_FIX4K_C0000 + ($x)
    };
}

macro_rules! ia32_mtrr_physbase {
    ($x:expr) => {
        X86_MSR_IA32_MTRR_PHYSBASE0 + 2 * ($x)
    };
}

macro_rules! ia32_mtrr_physmask {
    ($x:expr) => {
        X86_MSR_IA32_MTRR_PHYSMASK0 + 2 * ($x)
    };
}

macro_rules! mtrr_def_type_enable {
    ($x:expr) => {
        (($x) & (1 << 11)) != 0
    };
}

macro_rules! mtrr_def_type_fixed_enable {
    ($x:expr) => {
        (($x) & (1 << 10)) != 0
    };
}

macro_rules! mtrr_def_type_type {
    ($x:expr) => {
        ($x) as u8
    };
}

macro_rules! mtrr_physbase_base {
    ($x:expr) => {{
        let width = mmu_max_paddr_width();
        ($x) & !((1u64 << 12) - 1) & ((1u64 << width) - 1)
    }};
}

macro_rules! mtrr_physbase_type {
    ($x:expr) => {
        ($x) as u8
    };
}

macro_rules! mtrr_physmask_mask {
    ($x:expr) => {{
        let width = mmu_max_paddr_width();
        ($x) & !((1u64 << 12) - 1) & ((1u64 << width) - 1)
    }};
}

macro_rules! mtrr_physmask_valid {
    ($x:expr) => {
        (($x) & (1 << 11)) != 0
    };
}

/// Helper for decoding and printing MTRRs
///
/// # Safety
///
/// `msr` must be a valid fixed-range MTRR MSR supported by the current CPU.
unsafe fn print_fixed_range_mtrr(msr: u32, mut base: u32, record_size: u32) {
    // SAFETY: Caller guarantees `msr` is a valid fixed-range MTRR MSR.
    let mut val = unsafe { read_msr(msr) };
    for _i in 0..8 {
        kprintln!("  f {:#05x}-{:#05x}: {:#02x}", base, base + record_size - 1, val as u8);
        base += record_size;
        val >>= 8;
    }
}

fn print_pat_entries() {
    // SAFETY: `X86_MSR_IA32_PAT` is an architectural MSR on x86_64.
    let mut pat = unsafe { read_msr(X86_MSR_IA32_PAT) };
    for i in 0..8 {
        kprintln!("  Index {}: {:#02x}", i, pat as u8);
        pat >>= 8;
    }
}

/// # Safety
///
/// `argv` must point to a valid array of `CmdArgs` of length `argc`, with `argc >= 1`.
unsafe extern "C" fn cmd_memtype(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    // SAFETY: The console framework guarantees `argv` points to `argc` valid `CmdArgs` elements.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    let usage = || -> c_int {
        kprintln!("usage:");
        kprintln!("{:s} mtrr", argv[0].as_str());
        kprintln!("{:s} pat", argv[0].as_str());
        Status::INTERNAL.into_raw()
    };
    'usage: {
        if argv.len() < 2 {
            kprintln!("not enough arguments");
            break 'usage;
        }

        if argv[1].as_str() == "mtrr" {
            let mut print_fixed = false;
            if argv.len() > 2 {
                if argv[2].as_str() == "-f" {
                    print_fixed = true;
                } else {
                    kprintln!("usage: {:s} mtrr [-f]", argv[0].as_str());
                    kprintln!("  -f    Display fixed registers");
                    return Status::INTERNAL.into_raw();
                }
            }
            // SAFETY: `X86_MSR_IA32_MTRR_DEF_TYPE` is a valid architectural MTRR MSR on x86_64.
            let default_type = unsafe { read_msr(X86_MSR_IA32_MTRR_DEF_TYPE) };
            kprintln!(
                "MTRR state: master {:s}, fixed {:s}",
                if mtrr_def_type_enable!(default_type) { "enable" } else { "disable" },
                if mtrr_def_type_fixed_enable!(default_type) { "enable" } else { "disable" }
            );
            kprintln!("  default: {:#02x}", mtrr_def_type_type!(default_type));
            if mmu_supports_fixed_range() && print_fixed {
                // SAFETY: `supports_fixed_range` is true, so all architectural fixed-range MTRR
                // MSRs are valid.
                unsafe {
                    print_fixed_range_mtrr(X86_MSR_IA32_MTRR_FIX64K_00000, 0x00000, 1 << 16);
                    for i in 0..IA32_MTRR_NUM_FIX16K {
                        print_fixed_range_mtrr(
                            ia32_mtrr_fix16k_80000!(i),
                            0x80000 + i * (1 << 17),
                            1 << 14,
                        );
                    }
                    for i in 0..IA32_MTRR_NUM_FIX4K {
                        print_fixed_range_mtrr(
                            ia32_mtrr_fix4k_c0000!(i),
                            0xC0000 + i * (1 << 15),
                            1 << 12,
                        );
                    }
                }
            }

            for i in 0..(mmu_num_variable() as u32) {
                // SAFETY: `i < num_variable` guarantees `PHYSBASE(i)` and `PHYSMASK(i)` are valid.
                let base = unsafe { read_msr(ia32_mtrr_physbase!(i)) };
                // SAFETY: `i < num_variable` guarantees `PHYSBASE(i)` and `PHYSMASK(i)` are valid.
                let mask = unsafe { read_msr(ia32_mtrr_physmask!(i)) };
                kprintln!(
                    "  v ({:s}) base {:#016x}, mask {:#016x}: {:#02x}",
                    if mtrr_physmask_valid!(mask) { "valid" } else { "invalid" },
                    mtrr_physbase_base!(base),
                    mtrr_physmask_mask!(mask),
                    mtrr_physbase_type!(base)
                );
            }
        } else if argv[1].as_str() == "pat" {
            let num_cpus = crate::arch_rs::max_num_cpus();
            for i in 0..num_cpus {
                kprintln!("CPU {i} Page Attribute Table types:");
                sync_exec(MpIpiTarget::Mask, 1u32 << i, print_pat_entries);
            }
        } else {
            kprintln!("unknown command");
            break 'usage;
        }

        return ZX_OK;
    }

    usage()
}

static_command!(
    CMD_MEMTYPE,
    c"memtype".as_ptr(),
    c"memory type commands".as_ptr(),
    cmd_memtype,
    CMD_AVAIL_NORMAL
);
