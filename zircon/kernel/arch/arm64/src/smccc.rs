// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM Secure Monitor Call Calling Convention (SMCCC)
//!
//! <http://infocenter.arm.com/help/index.jsp?topic=/com.arm.doc.den0028b/index.html>

/// Low-level result structure from ARM SMCCC calls matching `arm_smccc_result_t`.
#[repr(C)]
#[derive(Debug, Copy, Clone, Default, Eq, PartialEq)]
pub struct ArmSmcccResult {
    pub x0: u64,
    pub x1: u64,
    pub x2: u64,
    pub x3: u64,
    /// At least one implementation uses it as a way to return session_id.
    pub x6: u64,
}

zr::static_assert!(core::mem::size_of::<ArmSmcccResult>() == 40);
zr::static_assert!(core::mem::align_of::<ArmSmcccResult>() == 8);

/// SMCCC Function Identifiers: the values of `arch::ArmSmcccFunction` in
/// `<lib/arch/arm64/smccc.h>`.
#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)] // The names follow the C++ enumerators.
#[allow(dead_code)] // The whole C++ table; Rust passes only some of these.
pub(super) enum ArmSmcccFunction {
    // [arm/smccc] 7: Arm Architecture Calls
    SmcccVersion = 0x8000_0000,
    SmcccArchFeatures = 0x8000_0001,
    SmcccArchSocId = 0x8000_0002,
    SmcccArchWorkaround1 = 0x8000_8000,
    SmcccArchWorkaround2 = 0x8000_7fff,
    SmcccArchWorkaround3 = 0x8000_3fff,

    // [arm/psci] 5.1: Function prototypes
    // 0x8400_0000 - 0x8400_001f and their SMC64 counterparts at
    // 0xc400_0000 - 0xc400_001f are reserved by [arm/smccc] for PSCI.
    //
    // These are the SMC64 versions when both SMC32 and SMC64 versions are
    // defined.
    PsciPsciVersion = 0x8400_0000,
    PsciCpuSuspend = 0xc400_0001,
    PsciCpuOff = 0x8400_0002,
    PsciCpuOn = 0xc400_0003,
    PsciAffinityInfo = 0xc400_0004,
    PsciMigrate = 0xc400_0005,
    PsciMigrateInfoType = 0x8400_0006,
    PsciMigrateInfoUpCpu = 0xc400_0007,
    PsciSystemOff = 0x8400_0008,
    PsciSystemReset = 0x8400_0009,
    PsciSystemReset2 = 0xc400_0012,
    PsciMemProtect = 0x8400_0013,
    PsciMemProtectCheckRange = 0xc400_0014,
    PsciPsciFeatures = 0x8400_000a,
    PsciCpuFreeze = 0x8400_000b,
    PsciCpuDefaultSuspend = 0x8400_000c,
    PsciNodeHwState = 0x8400_000d,
    PsciSystemSuspend = 0xc400_000e,
    PsciPsciSetSuspendMode = 0x8400_000f,
    PsciPsciStatResidency = 0xc400_0010,
    PsciPsciStatCount = 0xc400_0011,
}

unsafe extern "C" {
    /// Calls the low-level SMC assembly routine (`arm_smccc_smc_internal`).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the function ID and argument registers conform to
    /// the ARM SMCCC specification and are safe to execute at the secure monitor level.
    pub fn arm_smccc_smc_internal(
        w0: u32,
        x1: u64,
        x2: u64,
        x3: u64,
        x4: u64,
        x5: u64,
        x6: u64,
        w7: u32,
    ) -> ArmSmcccResult;

    /// This is defined in assembly (`smccc.S`), through the code-patched conduit instruction.
    /// The first argument is the Function Identifier and the other arguments vary by function.
    ///
    /// # Safety
    ///
    /// The call and its arguments must be safe for the firmware to execute.
    pub(super) fn ArmSmcccCall(function: ArmSmcccFunction, arg1: u64, arg2: u64, arg3: u64) -> u64;
}

/// Calls the low-level SMC function.
#[inline]
#[allow(clippy::too_many_arguments)]
pub fn arm_smccc_smc(
    w0: u32, // Function Identifier
    x1: u64, // x1-x6: Parameters
    x2: u64,
    x3: u64,
    x4: u64,
    x5: u64,
    x6: u64,
    w7: u32, // Client ID[15:0], Secure OS ID[31:16]
) -> ArmSmcccResult {
    // SAFETY: Direct call to assembly SMC routine. The calling convention preserves
    // required registers and returns results in x0-x3, x6.
    unsafe { arm_smccc_smc_internal(w0, x1, x2, x3, x4, x5, x6, w7) }
}
