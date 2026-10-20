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
