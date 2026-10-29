// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ffi::c_int;
use core::sync::atomic::{AtomicU64, Ordering};

use debug::dprintf;
use kprint::kprintln;
use zx_status::Status;
use zx_types::ZX_OK;

use crate::arch_rs::feature::{X86MicroarchList, x86_get_microarch_config};
use crate::arch_rs::platform_access::{MsrAccess, RealMsrAccess};
use crate::arch_rs::registers::{
    X86_MSR_GFX_PERF_LIMIT_REASONS, X86_MSR_PERF_LIMIT_REASONS, X86_MSR_PKG_ENERGY_STATUS,
    X86_MSR_PKG_POWER_LIMIT, X86_MSR_PP0_ENERGY_STATUS, X86_MSR_PP1_ENERGY_STATUS,
    X86_MSR_RAPL_POWER_UNIT,
};
use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
use crate::kernel::bits::bits_shift;
use crate::kernel::recurring_callback::StaticRecurringCallback;

/// Intel Volume 3 Section 14.9.3.
const POWER_LIMIT_PL1_ENABLE: u64 = 1u64 << 15;
const POWER_LIMIT_PL1_CLAMP: u64 = 1u64 << 16;
const POWER_LIMIT_PL2_ENABLE: u64 = 1u64 << 47;
const POWER_LIMIT_PL2_CLAMP: u64 = 1u64 << 48;

#[derive(Copy, Clone)]
struct LimitReason {
    bit: u64,
    str: &'static str,
}

/// Intel Volume 4 Table 2-39.
const LIMIT_REASONS: [LimitReason; 11] = [
    LimitReason { bit: 1 << 0, str: "PROCHOT" },
    LimitReason { bit: 1 << 1, str: "Thermal event" },
    LimitReason { bit: 1 << 4, str: "Residency state regulation limit" },
    LimitReason { bit: 1 << 5, str: "Running average thermal limit" },
    LimitReason { bit: 1 << 6, str: "Voltage regulator (VR) thermal alert" },
    LimitReason { bit: 1 << 7, str: "Voltage regulator (VR) thermal design current limit" },
    LimitReason { bit: 1 << 8, str: "Other" },
    LimitReason { bit: 1 << 10, str: "Package/platform-Level PL1" },
    LimitReason { bit: 1 << 11, str: "Package/platform-Level PL2" },
    LimitReason { bit: 1 << 12, str: "Max turbo limit" },
    LimitReason { bit: 1 << 13, str: "Turbo transition attenuation" },
];

/// Intel Volume 4 Table 2-39 "MSR_GRAPHICS_PERF_LIMIT_REASONS".
const LIMIT_REASONS_GFX: [LimitReason; 9] = [
    LimitReason { bit: 1 << 0, str: "PROCHOT" },
    LimitReason { bit: 1 << 1, str: "Thermal event" },
    LimitReason { bit: 1 << 5, str: "Running average thermal limit" },
    LimitReason { bit: 1 << 6, str: "Voltage regulator (VR) thermal alert" },
    LimitReason { bit: 1 << 7, str: "Voltage regulator (VR) thermal design current limit" },
    LimitReason { bit: 1 << 8, str: "Other" },
    LimitReason { bit: 1 << 10, str: "Package/platform-Level PL1" },
    LimitReason { bit: 1 << 11, str: "Package/platform-Level PL2" },
    LimitReason { bit: 1 << 12, str: "Inefficient operation" },
];

const LIMIT_REASONS_LOG_SHIFT: u64 = 16;

struct RaplUnits {
    power_mw: u32,
    time_us: u32,
    energy_uj: u32,
}

/// Returns whether the performance limit reason MSRs are supported on this microarchitecture.
fn limit_reason_supported() -> bool {
    // Limit reason MSR is supported on Intel Core generations 6 through 11, Intel Xeon generations
    // 1 through 3, Intel Core i3 8th generation, and Intel Xeon E processors. See Intel Volume 4
    // Table 2-39.
    let x86_microarch = x86_get_microarch_config().unwrap().x86_microarch;
    (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_SKYLAKE)
        || (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_CANNONLAKE)
        || (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_TIGERLAKE)
}

/// Returns whether the RAPL power limit MSRs are supported on this microarchitecture.
fn power_limit_supported() -> bool {
    let x86_microarch = x86_get_microarch_config().unwrap().x86_microarch;
    (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_SANDY_BRIDGE)
        || (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_SILVERMONT)
        || (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_BROADWELL)
        || (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_HASWELL)
        || (x86_microarch == X86MicroarchList::X86_MICROARCH_INTEL_SKYLAKE)
}

/// Reads `MSR_RAPL_POWER_UNIT` and computes the power, time, and energy unit multipliers.
fn get_units(msr: &impl MsrAccess) -> RaplUnits {
    // MSR_RAPL_POWER_UNIT provides the following information across all RAPL domains
    // Power Units[3:0]: power info (in watts) is based on the multiplier, 1/2^PU where PU is an
    // unsigned integer represented by bits [3:0].
    //
    // Time Units[19:16]: Time info (in seconds) is based on multiplier, 1/2^TU where TU is an
    // unsigned integer represented by bits[19:16]
    //
    // Energy Units[12:8]: Energy related information (in Joules) is based on the multiplier,
    // 1/2^ESU, where ESU is an unsigned integer represented by bits 12:8.
    //
    // Based on Intel Software Manual vol 3, chapter 14.9.
    //
    // To give better precision we specify power in milliwatts, time in microseconds, and energy in
    // microjoules.
    let rapl_unit = msr.read_msr(X86_MSR_RAPL_POWER_UNIT);
    RaplUnits {
        power_mw: 1000u32 / (1u32 << bits_shift(rapl_unit, 3, 0)),
        time_us: 1_000_000u32 / (1u32 << bits_shift(rapl_unit, 19, 16)),
        energy_uj: 1_000_000u32 / (1u32 << bits_shift(rapl_unit, 12, 8)),
    }
}

/// Prints the current PL1 and PL2 package power limits and time windows.
fn print_limits() {
    let msr = RealMsrAccess;
    let units = get_units(&msr);

    let rapl = msr.read_msr(X86_MSR_PKG_POWER_LIMIT);

    // Based on Intel Software Manual vol 3, chapter 14.9,
    // Time limit = 2^Y * (1.0 + Z/4.0) * Time_Unit
    let mut y = bits_shift(rapl, 21, 17) as u32;
    let mut z = bits_shift(rapl, 23, 22) as u32;
    let mut time_window = (1u32 << y).wrapping_mul(4 + z).wrapping_mul(units.time_us) / 4;
    let mut power_limit = bits_shift(rapl, 14, 0) as u32;

    kprintln!("PL1 limit: {}mW", power_limit.wrapping_mul(units.power_mw));
    kprintln!("PL1 window: {}us", time_window);
    kprintln!(
        "PL1 {:s}abled, clamping {:s}abled",
        if (rapl & POWER_LIMIT_PL1_ENABLE) != 0 { "en" } else { "dis" },
        if (rapl & POWER_LIMIT_PL1_CLAMP) != 0 { "en" } else { "dis" }
    );

    // Repeat for PL2
    y = bits_shift(rapl, 53, 49) as u32;
    z = bits_shift(rapl, 55, 54) as u32;
    time_window = (1u32 << y).wrapping_mul(4 + z).wrapping_mul(units.time_us) / 4;
    power_limit = bits_shift(rapl, 46, 32) as u32;

    kprintln!("PL2 limit: {}mW", power_limit.wrapping_mul(units.power_mw));
    kprintln!("PL2 window: {}us", time_window);
    kprintln!(
        "PL2 {:s}abled, clamping {:s}abled",
        if (rapl & POWER_LIMIT_PL2_ENABLE) != 0 { "en" } else { "dis" },
        if (rapl & POWER_LIMIT_PL2_CLAMP) != 0 { "en" } else { "dis" }
    );
}

/// Clears the latched CPU and graphics performance limit reason log bits.
fn clear_limit_reason_log() {
    if !limit_reason_supported() {
        kprintln!("Limit reasons msr not supported");
        return;
    }

    // The limit reason log is stored in bits 29:16 and can be cleared by writing zeros.
    let mut msr = RealMsrAccess;
    msr.write_msr(X86_MSR_PERF_LIMIT_REASONS, 0);
    msr.write_msr(X86_MSR_GFX_PERF_LIMIT_REASONS, 0);
}

/// Prints the active or logged CPU and graphics performance limit reasons.
fn print_limit_reasons(use_log: bool) {
    if !limit_reason_supported() {
        kprintln!("Limit reasons msr not supported");
        return;
    }

    let msr = RealMsrAccess;
    let mut limit_reasons = msr.read_msr(X86_MSR_PERF_LIMIT_REASONS);

    // The log bits (29:16) are latched versions of the status bits (13:0). If we're printing the
    // log shift the register value down.
    if use_log {
        limit_reasons >>= LIMIT_REASONS_LOG_SHIFT;
    }

    let mut is_limited = false;
    kprintln!("perf limit reasons:");
    for reason in &LIMIT_REASONS {
        if (limit_reasons & reason.bit) == 0 {
            continue;
        }
        kprintln!("\t{:s}", reason.str);
        is_limited = true;
    }
    if !is_limited {
        kprintln!("\tnone");
    }

    limit_reasons = msr.read_msr(X86_MSR_GFX_PERF_LIMIT_REASONS);
    if use_log {
        limit_reasons >>= LIMIT_REASONS_LOG_SHIFT;
    }

    kprintln!("gfx perf limit reasons:");
    is_limited = false;
    for reason in &LIMIT_REASONS_GFX {
        if (limit_reasons & reason.bit) == 0 {
            continue;
        }
        kprintln!("\t{:s}", reason.str);
        is_limited = true;
    }
    if !is_limited {
        kprintln!("\tnone");
    }
}

static G_STATUS_CALLBACK: StaticRecurringCallback = StaticRecurringCallback::new(|| {
    let msr = RealMsrAccess;
    let units = get_units(&msr);

    // Sequential invocations of this callback are serialized because the underlying one-shot
    // `Timer` is only re-armed (`set_deadline`) after this callback returns. `Ordering::Relaxed` is
    // sufficient because each `LAST_*_ENERGY_STATUS` atomic is an independent scalar counter used
    // solely to avoid `static mut`.
    kprintln!("energy consumed:");
    {
        static LAST_ENERGY_STATUS: AtomicU64 = AtomicU64::new(0);

        let energy_status = msr.read_msr(X86_MSR_PKG_ENERGY_STATUS);
        let last_energy_status = LAST_ENERGY_STATUS.swap(energy_status, Ordering::Relaxed);
        let uj =
            energy_status.wrapping_sub(last_energy_status).wrapping_mul(u64::from(units.energy_uj));

        kprintln!(
            "\tpkg: {} uJ ({} J) (total: {} uJ)",
            uj,
            uj / 1_000_000,
            energy_status.wrapping_mul(u64::from(units.energy_uj))
        );
    }

    {
        // PP0 usually is the core
        static LAST_PP0_ENERGY_STATUS: AtomicU64 = AtomicU64::new(0);

        let pp0_energy_status = msr.read_msr(X86_MSR_PP0_ENERGY_STATUS);
        let last_pp0_energy_status =
            LAST_PP0_ENERGY_STATUS.swap(pp0_energy_status, Ordering::Relaxed);
        let uj = pp0_energy_status
            .wrapping_sub(last_pp0_energy_status)
            .wrapping_mul(u64::from(units.energy_uj));

        kprintln!(
            "\tpp0: {} uJ ({} J) (total: {} uJ)",
            uj,
            uj / 1_000_000,
            pp0_energy_status.wrapping_mul(u64::from(units.energy_uj))
        );
    }

    {
        // PP1 usually is graphics
        static LAST_PP1_ENERGY_STATUS: AtomicU64 = AtomicU64::new(0);

        let pp1_energy_status = msr.read_msr(X86_MSR_PP1_ENERGY_STATUS);
        let last_pp1_energy_status =
            LAST_PP1_ENERGY_STATUS.swap(pp1_energy_status, Ordering::Relaxed);
        let uj = pp1_energy_status
            .wrapping_sub(last_pp1_energy_status)
            .wrapping_mul(u64::from(units.energy_uj));

        kprintln!(
            "\tpp1: {} uJ ({} J) (total: {} uJ)",
            uj,
            uj / 1_000_000,
            pp1_energy_status.wrapping_mul(u64::from(units.energy_uj))
        );
    }

    print_limit_reasons(/*use_log=*/ false);
});

/// Prints usage information for the `power` console command.
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
    for subcommand in &SUBCOMMANDS {
        dprintf!(ALWAYS, "\tpower {:<32}: {}\n", subcommand.cmd_str, subcommand.help_str);
    }
}

/// # Safety
///
/// If `argc > 0` and `argv` is non-null, the caller must ensure that `argv` is properly aligned
/// and points to `argc` consecutive initialized [`CmdArgs`] elements valid for reads for the
/// duration of this call, and that each element's `arg_str` (if non-null) points to a valid
/// NUL-terminated C string for the duration of this call.
unsafe extern "C" fn cmd_power(argc: c_int, argv: *const CmdArgs, _flags: u32) -> c_int {
    if argc <= 0 || argv.is_null() {
        return Status::INVALID_ARGS.into_raw();
    }
    // SAFETY: We verified `argc > 0` and `!argv.is_null()` above, and by this function's
    // `# Safety` contract the caller guarantees `argv` is aligned and points to `argc`
    // initialized `CmdArgs` elements valid for reads for the duration of this call.
    let argv = unsafe { core::slice::from_raw_parts(argv, argc as usize) };
    if argv.len() < 2 {
        print_command_usage();
        return Status::INVALID_ARGS.into_raw();
    }

    if !power_limit_supported() {
        dprintf!(ALWAYS, "Power limit controls not supported\n");
        return Status::NOT_SUPPORTED.into_raw();
    }

    if argv[1].as_str() == "status" {
        G_STATUS_CALLBACK.toggle();
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
        print_limits();
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
