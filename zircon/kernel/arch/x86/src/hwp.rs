// Copyright 2017 The Fuchsia Authors
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

pub(super) use boot_options::IntelHwpPolicy;

use super::cpuid::{CpuId, Features};
use super::platform_access::MsrAccess;
use super::registers::{
    X86_MSR_IA32_ENERGY_PERF_BIAS, X86_MSR_IA32_HWP_CAPABILITIES, X86_MSR_IA32_HWP_REQUEST,
    X86_MSR_IA32_PM_ENABLE,
};

zr::static_assert!(core::mem::size_of::<IntelHwpPolicy>() == 1);
zr::static_assert!(core::mem::align_of::<IntelHwpPolicy>() == 1);

/// An "energy performance preference" is a 8-bit value specifying a desired
/// tradeoff between running a CPU in a high performance mode (0) vs an
/// energy-efficient mode (255).
#[derive(Clone, Copy, PartialEq, Eq)]
struct EnergyPerformancePref(u8);

/// Various EnergyPerformancePref values.
const MAX_PERFORMANCE_EPP: EnergyPerformancePref = EnergyPerformancePref(0x00);
const BALANCED_EPP: EnergyPerformancePref = EnergyPerformancePref(0x80);
const POWER_SAVE_EPP: EnergyPerformancePref = EnergyPerformancePref(0xff);

/// An 8-bit "performance level", as used by the IA32_HWP_CAPABILITIES
/// MSR. Higher values indicate higher performance, at the cost of using
/// more power.
#[derive(Clone, Copy, PartialEq, Eq)]
struct PerformanceLevel(u8);

/// Convert the 4-bit IA32_ENERGY_PERF_BIAS value into an 8-bit
/// IA32_ENERGY_PERF_PREFERENCE value.
///
/// IA32_ENERGY_PERF_BIAS is a 4-bit value that may be set by firmware to
/// indicate a platform's desired tradeoff between performance and power
/// efficiency. It is only used when HWP is not active, so we convert it to HWP's
/// ENERGY_PERFORMANCE_PREFERENCE scale.
fn perf_bias_to_perf_pref(mut epb: u8) -> EnergyPerformancePref {
    const ENERGY_PERF_BIAS_TO_ENERGY_PERF_PREFERENCE: [u8; 16] = [
        /* 0x0 */ 0x20, // 'PERFORMANCE'
        /* 0x1 */ 0x20, /* 0x2 */ 0x20, /* 0x3 */ 0x20,
        /* 0x4 */ 0x40, // 'BALANCED PERFORMANCE'
        /* 0x5 */ 0x40, /* 0x6 */ 0x80, // 'NORMAL'
        /* 0x7 */ 0x80, /* 0x8 */ 0x80, // 'BALANCED POWERSAVE'
        /* 0x9 */ 0xFF, /* 0xA */ 0xFF, /* 0xB */ 0xFF, /* 0xC */ 0xFF,
        /* 0xD */ 0xFF, /* 0xE */ 0xFF, /* 0xF */ 0xFF, // 'POWERSAVE'
    ];

    epb &= 0xF; // Sanitize ENERGY_PERF_BIAS just in case.
    EnergyPerformancePref(ENERGY_PERF_BIAS_TO_ENERGY_PERF_PREFERENCE[epb as usize])
}

/// Hardware-enumerated PerformanceLevel capability values.
struct HwpCapabilities {
    #[expect(dead_code)]
    most_efficient_performance: PerformanceLevel,
    guaranteed_performance: PerformanceLevel,
    highest_performance: PerformanceLevel,
    lowest_performance: PerformanceLevel,
}

/// Parse the HWP capabilities of the CPU.
fn read_hwp_capabilities(msr: &impl MsrAccess) -> HwpCapabilities {
    let hwp_caps = msr.read_msr(X86_MSR_IA32_HWP_CAPABILITIES);

    HwpCapabilities {
        highest_performance: PerformanceLevel(hwp_caps as u8),
        guaranteed_performance: PerformanceLevel((hwp_caps >> 8) as u8),
        most_efficient_performance: PerformanceLevel((hwp_caps >> 16) as u8),
        lowest_performance: PerformanceLevel((hwp_caps >> 24) as u8),
    }
}

/// Return the EnergyPerformancePref recommended by the BIOS/firmware.
fn get_bios_epp(features: &Features, msr: &impl MsrAccess) -> EnergyPerformancePref {
    if features.has_feature(Features::EPB) {
        return perf_bias_to_perf_pref((msr.read_msr(X86_MSR_IA32_ENERGY_PERF_BIAS) & 0xF) as u8);
    }
    BALANCED_EPP
}

/// Construct a 64-bit MSR_REQUEST MSR value.
fn make_hwp_request(
    min_perf: PerformanceLevel,
    max_perf: PerformanceLevel,
    desired_perf: PerformanceLevel,
    epp: EnergyPerformancePref,
) -> u64 {
    u64::from(min_perf.0)
        | (u64::from(max_perf.0) << 8)
        | (u64::from(desired_perf.0) << 16)
        | (u64::from(epp.0) << 24)
}

/// Parse a string as an HWP policy.
#[cfg(any(console_enabled, ktest))]
fn intel_hwp_parse_policy(s: &str) -> Option<IntelHwpPolicy> {
    match s {
        "bios-specified" => Some(IntelHwpPolicy::BiosSpecified),
        "performance" => Some(IntelHwpPolicy::Performance),
        "balanced" => Some(IntelHwpPolicy::Balanced),
        "power-save" => Some(IntelHwpPolicy::PowerSave),
        "stable-performance" => Some(IntelHwpPolicy::StablePerformance),
        _ => None,
    }
}

/// Determine if Intel HWP is supported from `features`.
fn intel_hwp_features_supported(features: &Features) -> bool {
    features.has_feature(Features::HWP) && features.has_feature(Features::HWP_PREF)
}

/// Initialize the Intel HWP on the current CPU.
///
/// If HWP is not supported on the current CPU, no action will be taken.
pub(super) fn intel_hwp_init(cpuid: &impl CpuId, msr: &mut impl MsrAccess, policy: IntelHwpPolicy) {
    // Ensure we have HWP on this CPU.
    let features = cpuid.read_features();
    if !intel_hwp_features_supported(&features) {
        return;
    }

    // Enable HWP.
    msr.write_msr(X86_MSR_IA32_PM_ENABLE, 1);

    // Get hardware capabilities.
    let caps = read_hwp_capabilities(msr);

    // Set up HWP preferences.
    //
    // In most cases, we set minimum/maximum to values from the corresponding
    // capabilities, set desired performance to 0 ("automatic"),  and set the
    // energy performance based on the policy.
    //
    // Reference: Intel SDM vol 3B section 14.4.7: Recommendations for OS use of
    // HWP controls
    let mut desired = PerformanceLevel(0); // auto
    let mut min = caps.lowest_performance;
    let mut max = caps.highest_performance;
    let pref;

    // Override defaults based on policy.
    match policy {
        IntelHwpPolicy::BiosSpecified => {
            pref = get_bios_epp(&features, msr);
        }
        IntelHwpPolicy::Performance => {
            pref = MAX_PERFORMANCE_EPP;
        }
        IntelHwpPolicy::Balanced => {
            pref = BALANCED_EPP;
        }
        IntelHwpPolicy::PowerSave => {
            pref = POWER_SAVE_EPP;
        }
        IntelHwpPolicy::StablePerformance => {
            // Set min/max/desired to "guaranteed_performance" to try and keep CPU at
            // a stable performance level.
            min = caps.guaranteed_performance;
            max = caps.guaranteed_performance;
            desired = caps.guaranteed_performance;
            pref = MAX_PERFORMANCE_EPP;
        }
    }

    // Program the HWP request register.
    msr.write_msr(
        X86_MSR_IA32_HWP_REQUEST,
        make_hwp_request(
            /*min_perf=*/ min, /*max_perf=*/ max, /*desired_perf=*/ desired,
            /*epp=*/ pref,
        ),
    );
}

/// Determine if Intel HWP is supported on given CPU.
pub(super) fn intel_hwp_supported(cpuid: &impl CpuId) -> bool {
    intel_hwp_features_supported(&cpuid.read_features())
}

#[cfg(console_enabled)]
mod console {
    use core::ffi::c_int;
    use kprint::{kprint, kprintln};
    use zx_status::Status;
    use zx_types::{ZX_OK, zx_status_t};

    use super::super::cpuid::RealCpuId;
    use super::super::platform_access::{MsrAccess, RealMsrAccess};
    use super::super::registers::{X86_MSR_IA32_HWP_REQUEST, X86_MSR_IA32_PM_ENABLE};
    use super::{
        MAX_PERFORMANCE_EPP, PerformanceLevel, intel_hwp_init, intel_hwp_parse_policy,
        intel_hwp_supported, make_hwp_request,
    };
    use crate::console::{CMD_AVAIL_NORMAL, CmdArgs, static_command};
    use crate::kernel::mp::{MpIpiTarget, sync_exec};

    fn cmd_print_usage() {
        kprint!(concat!(
            "usage:\n",
            "  hwp set-policy <policy-name>  - set performance policy\n",
            "                                  valid policies include bios-specified, ",
            "performance,\n",
            "                                  balanced, power-save, stable-performance.\n",
            "  hwp set-freq <int>            - set processor frequency to given value.\n",
            "                                  values map directly onto frequency targets, ",
            "but the\n",
            "                                  exact meaning is processor-dependent.\n",
        ));
    }

    fn cmd_set_policy(argv: &[CmdArgs], _flags: u32) -> zx_status_t {
        if argv.len() != 1 {
            cmd_print_usage();
            return Status::INVALID_ARGS.into_raw();
        }

        let Some(policy) = intel_hwp_parse_policy(argv[0].as_str()) else {
            kprintln!("Unknown policy '{:s}'.", argv[0].as_str());
            return Status::INVALID_ARGS.into_raw();
        };

        sync_exec(MpIpiTarget::All, 0, || {
            let cpuid = RealCpuId;
            let mut msr = RealMsrAccess;
            intel_hwp_init(&cpuid, &mut msr, policy);
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

        let desired_perf = PerformanceLevel(desired_freq as u8);
        let request =
            make_hwp_request(desired_perf, desired_perf, desired_perf, MAX_PERFORMANCE_EPP);
        sync_exec(MpIpiTarget::All, 0, || {
            let mut msr = RealMsrAccess;
            msr.write_msr(X86_MSR_IA32_PM_ENABLE, 1);
            msr.write_msr(X86_MSR_IA32_HWP_REQUEST, request);
        });

        kprintln!("Frequency set to target {desired_freq}.");
        ZX_OK
    }

    /// # Safety
    ///
    /// If `argc > 0`, `argv` must be non-null, properly aligned for [`CmdArgs`], point to `argc`
    /// consecutive initialized [`CmdArgs`] values (each with a valid NUL-terminated `str` pointer)
    /// within a single live allocation of at most `isize::MAX` bytes, and remain valid and
    /// unmutated for the duration of this call.
    unsafe extern "C" fn cmd_hwp(argc: c_int, argv: *const CmdArgs, flags: u32) -> c_int {
        if argc <= 0 || argv.is_null() {
            return Status::INVALID_ARGS.into_raw();
        }
        let argv =
            // SAFETY: `core::slice::from_raw_parts` requires a non-null, properly aligned pointer
            // to `len` consecutive initialized elements within a single live allocation of at most
            // `isize::MAX` bytes that is not mutated for the returned slice's lifetime. Here
            // `argc > 0` and `!argv.is_null()`, and the caller's `# Safety` contract guarantees
            // that `argv` is properly aligned and points to `argc` consecutive initialized
            // `CmdArgs` elements within a single live allocation of at most `isize::MAX` bytes
            // that is not mutated during this call.
            unsafe { core::slice::from_raw_parts(argv, argc as usize) };

        // Ensure we have the hardware.
        let cpuid = RealCpuId;
        if !intel_hwp_supported(&cpuid) {
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
}

/// x86 Intel HWP tests
#[cfg(ktest)]
#[unittest::suite(name = "x86_hwp")]
mod tests {
    use super::super::cpuid::Registers;
    use super::super::cpuid::tests::{
        CPU_ID_AMD_A4_9120C, CPU_ID_CELERON_J3455, CPU_ID_CORE_I5_6260U, CPU_ID_EPYC_7B13,
        CPU_ID_THREAD_RIPPER_2970WX, CPU_ID_XEON_2690V4, FakeCpuId, TestDataSet,
    };
    use super::super::fake_msr_access::{FakeMsr, FakeMsrAccess};
    use super::{
        IntelHwpPolicy, MsrAccess, X86_MSR_IA32_ENERGY_PERF_BIAS, X86_MSR_IA32_HWP_CAPABILITIES,
        X86_MSR_IA32_HWP_REQUEST, X86_MSR_IA32_PM_ENABLE, intel_hwp_init, intel_hwp_parse_policy,
        intel_hwp_supported,
    };
    use unittest::{assert_false, assert_true, expect_eq, expect_true, unwrap_some};

    fn make_raw_hwp_request(min_perf: u8, max_perf: u8, desired_perf: u8, epp: u8) -> u64 {
        u64::from(min_perf)
            | (u64::from(max_perf) << 8)
            | (u64::from(desired_perf) << 16)
            | (u64::from(epp) << 24)
    }

    /// Parse valid and invalid HWP policy names.
    #[test]
    fn test_parse_policy() {
        // Valid parses.
        for (name, expected) in [
            ("bios-specified", IntelHwpPolicy::BiosSpecified),
            ("performance", IntelHwpPolicy::Performance),
            ("balanced", IntelHwpPolicy::Balanced),
            ("power-save", IntelHwpPolicy::PowerSave),
            ("stable-performance", IntelHwpPolicy::StablePerformance),
        ] {
            expect_eq!(unwrap_some!(intel_hwp_parse_policy(name)), expected);
        }

        // Invalid parses.
        expect_true!(intel_hwp_parse_policy("").is_none());
        expect_true!(intel_hwp_parse_policy("invalid").is_none());
        expect_true!(intel_hwp_parse_policy("\n").is_none());
    }

    /// Detect HWP CPUID feature support and leave MSRs untouched when HWP is unsupported.
    #[test]
    fn test_cpu_support_and_noop_init() {
        // AMD processors that don't support Intel HWP.
        assert_false!(intel_hwp_supported(&CPU_ID_AMD_A4_9120C));
        assert_false!(intel_hwp_supported(&CPU_ID_THREAD_RIPPER_2970WX));
        assert_false!(intel_hwp_supported(&CPU_ID_EPYC_7B13));

        // Intel processors supporting HWP.
        assert_true!(intel_hwp_supported(&CPU_ID_CORE_I5_6260U));

        // Older Intel processors not supporting HWP.
        assert_false!(intel_hwp_supported(&CPU_ID_XEON_2690V4));
        assert_false!(intel_hwp_supported(&CPU_ID_CELERON_J3455));

        // HWP_PREF not supported, expect no MSR writes.
        let mut fake_msrs = FakeMsrAccess::default();
        intel_hwp_init(&CPU_ID_XEON_2690V4, &mut fake_msrs, IntelHwpPolicy::Balanced);
        // An empty FakeMsrAccess will panic if you attempt to write to any uninitialized MSRs.
    }

    /// Map firmware IA32_ENERGY_PERF_BIAS into HWP request EPP under BiosSpecified policy.
    #[test]
    fn test_use_bios_values() {
        // Skylake-U has HWP_PREF and EPB
        let mut fake_msrs = FakeMsrAccess::default();
        fake_msrs.msrs[0] = FakeMsr { index: X86_MSR_IA32_ENERGY_PERF_BIAS, value: 0x5 };
        fake_msrs.msrs[1] = FakeMsr { index: X86_MSR_IA32_PM_ENABLE, value: 0x0 };
        // min = 0x11, max=0xfe, efficient=0x22, guaranteed=0x33.
        fake_msrs.msrs[2] = FakeMsr { index: X86_MSR_IA32_HWP_CAPABILITIES, value: 0x11_22_33_FE };
        fake_msrs.msrs[3] = FakeMsr { index: X86_MSR_IA32_HWP_REQUEST, value: 0x0 };

        intel_hwp_init(&CPU_ID_CORE_I5_6260U, &mut fake_msrs, IntelHwpPolicy::BiosSpecified);

        // Ensure HWP was enabled.
        expect_eq!(fake_msrs.read_msr(X86_MSR_IA32_PM_ENABLE), 1); // HWP enabled.

        // Check the generated HWP request is correct.
        //
        // We expect IA32_ENERGY_PERF_BIAS = 0x5 mapped to 0x40 EPP, min/max copied
        // from HWP capabilities, and desired set to 0 (auto).
        expect_eq!(
            fake_msrs.read_msr(X86_MSR_IA32_HWP_REQUEST),
            make_raw_hwp_request(
                /*min_perf=*/ 0x11, /*max_perf=*/ 0xfe, /*desired_perf=*/ 0x0,
                /*epp=*/ 0x40,
            )
        );
    }

    /// Fall back to balanced EPP under BiosSpecified policy when EPB is unsupported.
    #[test]
    fn test_use_bios_values_without_epb() {
        static TEST_DATA_HWP_WITHOUT_EPB: TestDataSet = TestDataSet {
            features: &[],
            missing_features: &[],
            leaf0: Registers { reg: [0, 0, 0, 0] },
            leaf1: Registers { reg: [0, 0, 0, 0] },
            leaf6: Registers { reg: [(1 << 7) | (1 << 10), 0, 0, 0] },
            leaf7: Registers { reg: [0, 0, 0, 0] },
            leaf8_0: Registers { reg: [0, 0, 0, 0] },
            leaf8_1: Registers { reg: [0, 0, 0, 0] },
            leaf8_7: Registers { reg: [0, 0, 0, 0] },
            leaf8_21: Registers { reg: [0, 0, 0, 0] },
        };
        static CPU_ID_HWP_WITHOUT_EPB: FakeCpuId = FakeCpuId::new(&TEST_DATA_HWP_WITHOUT_EPB);

        // Leave slot 0 (`X86_MSR_IA32_ENERGY_PERF_BIAS`) uninitialized so any read panics.
        let mut fake_msrs = FakeMsrAccess::default();
        fake_msrs.msrs[1] = FakeMsr { index: X86_MSR_IA32_PM_ENABLE, value: 0x0 };
        fake_msrs.msrs[2] = FakeMsr { index: X86_MSR_IA32_HWP_CAPABILITIES, value: 0x11_22_33_FE };
        fake_msrs.msrs[3] = FakeMsr { index: X86_MSR_IA32_HWP_REQUEST, value: 0x0 };

        intel_hwp_init(&CPU_ID_HWP_WITHOUT_EPB, &mut fake_msrs, IntelHwpPolicy::BiosSpecified);

        expect_eq!(fake_msrs.read_msr(X86_MSR_IA32_PM_ENABLE), 1);
        expect_eq!(
            fake_msrs.read_msr(X86_MSR_IA32_HWP_REQUEST),
            make_raw_hwp_request(
                /*min_perf=*/ 0x11, /*max_perf=*/ 0xfe, /*desired_perf=*/ 0x0,
                /*epp=*/ 0x80,
            )
        );
    }

    /// Configure maximum performance EPP and auto desired level under Performance policy.
    #[test]
    fn test_use_performance_policy() {
        let mut fake_msrs = FakeMsrAccess::default();
        fake_msrs.msrs[1] = FakeMsr { index: X86_MSR_IA32_PM_ENABLE, value: 0x0 };
        // min = 0x11, max=0xfe, guaranteed=0x33, efficient=0x22
        fake_msrs.msrs[2] = FakeMsr { index: X86_MSR_IA32_HWP_CAPABILITIES, value: 0x11_22_33_FE };
        fake_msrs.msrs[3] = FakeMsr { index: X86_MSR_IA32_HWP_REQUEST, value: 0x0 };

        // Skylake-U has HWP_PREF.
        intel_hwp_init(&CPU_ID_CORE_I5_6260U, &mut fake_msrs, IntelHwpPolicy::Performance);

        // Ensure HWP was enabled.
        expect_eq!(fake_msrs.read_msr(X86_MSR_IA32_PM_ENABLE), 1); // HWP enabled.

        // Check the generated HWP request is correct.
        //
        // We expect perf preference set to max performance (0x0), min/max copied
        // from the HWP capability, and desired set to 0 (auto).
        expect_eq!(
            fake_msrs.read_msr(X86_MSR_IA32_HWP_REQUEST),
            make_raw_hwp_request(
                /*min_perf=*/ 0x11, /*max_perf=*/ 0xfe, /*desired_perf=*/ 0x0,
                /*epp=*/ 0x0,
            )
        );
    }

    /// Configure balanced (0x80) and power-save (0xff) EPP under Balanced and PowerSave policies.
    #[test]
    fn test_use_balanced_and_power_save_policies() {
        let mut fake_msrs = FakeMsrAccess::default();
        fake_msrs.msrs[1] = FakeMsr { index: X86_MSR_IA32_PM_ENABLE, value: 0x0 };
        fake_msrs.msrs[2] = FakeMsr { index: X86_MSR_IA32_HWP_CAPABILITIES, value: 0x11_22_33_FE };
        fake_msrs.msrs[3] = FakeMsr { index: X86_MSR_IA32_HWP_REQUEST, value: 0x0 };

        intel_hwp_init(&CPU_ID_CORE_I5_6260U, &mut fake_msrs, IntelHwpPolicy::Balanced);
        expect_eq!(fake_msrs.read_msr(X86_MSR_IA32_PM_ENABLE), 1);
        expect_eq!(
            fake_msrs.read_msr(X86_MSR_IA32_HWP_REQUEST),
            make_raw_hwp_request(
                /*min_perf=*/ 0x11, /*max_perf=*/ 0xfe, /*desired_perf=*/ 0x0,
                /*epp=*/ 0x80,
            )
        );

        intel_hwp_init(&CPU_ID_CORE_I5_6260U, &mut fake_msrs, IntelHwpPolicy::PowerSave);
        expect_eq!(
            fake_msrs.read_msr(X86_MSR_IA32_HWP_REQUEST),
            make_raw_hwp_request(
                /*min_perf=*/ 0x11, /*max_perf=*/ 0xfe, /*desired_perf=*/ 0x0,
                /*epp=*/ 0xff,
            )
        );
    }

    /// Pin min, max, and desired levels to guaranteed performance under StablePerformance policy.
    #[test]
    fn test_use_stable_performance_policy() {
        let mut fake_msrs = FakeMsrAccess::default();
        fake_msrs.msrs[1] = FakeMsr { index: X86_MSR_IA32_PM_ENABLE, value: 0x0 };
        // min = 0x11, max=0xfe, guaranteed=0x33, efficient=0x22
        fake_msrs.msrs[2] = FakeMsr { index: X86_MSR_IA32_HWP_CAPABILITIES, value: 0x11_22_33_FE };
        fake_msrs.msrs[3] = FakeMsr { index: X86_MSR_IA32_HWP_REQUEST, value: 0x0 };

        // Skylake-U has HWP_PREF.
        intel_hwp_init(&CPU_ID_CORE_I5_6260U, &mut fake_msrs, IntelHwpPolicy::StablePerformance);

        // Ensure HWP was enabled.
        expect_eq!(fake_msrs.read_msr(X86_MSR_IA32_PM_ENABLE), 1); // HWP enabled.

        // Check the generated HWP request is correct.
        //
        // We expect perf preference set to max performance (0x0), min/max/desired
        // all set to the guaranteed performance value.
        expect_eq!(
            fake_msrs.read_msr(X86_MSR_IA32_HWP_REQUEST),
            make_raw_hwp_request(
                /*min_perf=*/ 0x33, /*max_perf=*/ 0x33, /*desired_perf=*/ 0x33,
                /*epp=*/ 0x0,
            )
        );
    }
}
