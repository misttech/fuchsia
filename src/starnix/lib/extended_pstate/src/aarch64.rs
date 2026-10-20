// Copyright 2023 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use static_assertions::const_assert_eq;
use std::sync::LazyLock;
use zx::CpuFeatureFlags;

// The SVE vector length, in bytes, that this module supports.
//
// Zircon only ever enables SVE with a 128-bit vector length (`ZCR_EL1.LEN = 0`, see
// `zircon/kernel/arch/arm64/arch.cc`), and all SVE handling in Starnix relies on that invariant:
// - Z0-Z31 are exactly 128 bits wide, so Q0-Q31 hold their complete contents and no separate Z
//   state needs to be saved or restored.
// - P0-P15 and FFR are 16 bits (2 bytes) each, which fixes the layout of `State` and the
//   `[xN, #imm, mul vl]` addressing used by `save_extended_pstate` and `restore_extended_pstate`
//   in `aarch64_asm.S`.
// - The Linux ABI exposed by Starnix (`sve_context` in signal frames, `prctl(PR_SVE_GET_VL)` and
//   `prctl(PR_SVE_SET_VL)`) reports a fixed 128-bit vector length.
// `HAS_SVE` verifies the invariant in debug builds.
pub const SVE_VL_BYTES: u64 = 16;

/// Whether the CPU supports SVE (`ZX_ARM64_FEATURE_ISA_SVE`).
pub static HAS_SVE: LazyLock<bool> = LazyLock::new(|| {
    let has_sve = zx::system_get_feature_flags::<CpuFeatureFlags>()
        .map(|flags| flags.contains(CpuFeatureFlags::ARM64_FEATURE_ISA_SVE))
        .unwrap_or(false);
    if has_sve {
        debug_assert_eq!(
            read_sve_vl_bytes(),
            SVE_VL_BYTES,
            "Starnix SVE support requires a 128-bit vector length"
        );
    }
    has_sve
});

/// Returns the current SVE vector length in bytes.
///
/// Must only be called when SVE is supported, as `rdvl` is UNDEFINED otherwise.
fn read_sve_vl_bytes() -> u64 {
    let vl: u64;
    // SAFETY: `rdvl` only reads the current vector length into a general-purpose register and has
    // no other effects. The caller guarantees that SVE is supported, so it cannot trap.
    unsafe {
        core::arch::asm!(
            ".arch_extension sve",
            "rdvl {vl}, #1",
            ".arch_extension nosve",
            vl = out(reg) vl,
            options(nomem, nostack, preserves_flags),
        );
    }
    vl
}

#[derive(Clone, Copy)]
#[repr(C)]
pub struct State {
    // [arm/v8]: A1.3.1 Execution state
    // 32 registers, 128 bits each. At the 128-bit SVE vector length (see `SVE_VL_BYTES`), these
    // are also the complete contents of Z0-Z31.
    pub q: [u128; 32],
    // [arm/v8]: A1.5 Advanced SIMD and floating-point support
    pub fpcr: u32,
    pub fpsr: u32,
    // SVE predicate registers P0-P15 (16 bits each at 128-bit vector length).
    pub p: [u16; 16],
    // SVE First Fault Register (16 bits at 128-bit vector length).
    pub ffr: u16,
    // Whether SVE is supported, in which case `p` and `ffr` are saved and restored. Preserved by
    // `reset`.
    pub has_sve: u8,
    // Whether the SVE vector length is inherited across `execve` (`PR_SVE_VL_INHERIT`). Not used
    // by the assembly routines. Preserved by `reset`, like Linux's `TIF_SVE_VL_INHERIT`.
    pub sve_vl_inherit: u8,
    pub _pad: [u8; 4],
}

impl Default for State {
    fn default() -> Self {
        Self {
            q: [0; 32],
            fpcr: 0,
            fpsr: 0,
            p: [0; 16],
            ffr: 0,
            has_sve: u8::from(*HAS_SVE),
            sve_vl_inherit: 0,
            _pad: [0; 4],
        }
    }
}

const_assert_eq!(std::mem::size_of::<State>(), 512 + 48);

// Ensure ABI compatibility with assembly routines in `aarch64_asm.S`.
static_assertions::assert_eq_align!(State, u128);
const_assert_eq!(std::mem::offset_of!(State, q), 0);
// LINT.IfChange(aarch64_state_offsets)
const_assert_eq!(std::mem::offset_of!(State, fpcr), 512);
const_assert_eq!(std::mem::offset_of!(State, fpsr), 516);
const_assert_eq!(std::mem::offset_of!(State, p), 520);
const_assert_eq!(std::mem::offset_of!(State, ffr), 552);
const_assert_eq!(std::mem::offset_of!(State, has_sve), 554);
// LINT.ThenChange(aarch64_asm.S:aarch64_state_offsets)
const_assert_eq!(std::mem::offset_of!(State, sve_vl_inherit), 555);

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct Aarch32State {
    // [arm/v8]: E1.3.1 The SIMD and floating-point register file
    // 16 registers, 128 bits each
    pub q: [u128; 16],

    // AArch32 technically has only 32 bits of user space accessible status/control space, see
    // [arm/v8]: G8.2.55 FPSCR, Floating-Point Status and Control Register.
    // The restricted mode implementation maps these to the fpcr/fpsr registers used by
    // AArch64, so we store those here instead of a single u32.
    pub fpcr: u32,
    pub fpsr: u32,
}

const_assert_eq!(std::mem::size_of::<Aarch32State>(), 256 + 16);

// Ensure ABI compatibility with assembly routines in `aarch64_asm.S`.
static_assertions::assert_eq_align!(Aarch32State, u128);
const_assert_eq!(std::mem::offset_of!(Aarch32State, q), 0);
// LINT.IfChange(aarch32_state_offsets)
const_assert_eq!(std::mem::offset_of!(Aarch32State, fpcr), 256);
const_assert_eq!(std::mem::offset_of!(Aarch32State, fpsr), 260);
// LINT.ThenChange(aarch64_asm.S:aarch32_state_offsets)

// Aarch64 supports aligned and unaligned stores to/from vector registers. Aligned accesses may be
// faster.
const_assert_eq!(std::mem::align_of::<u128>(), 16);

impl Aarch32State {
    pub fn reset(&mut self) {
        *self = Default::default();
    }
}

impl State {
    /// Zeroes all register state (Q0-Q31, FPCR, FPSR, P0-P15, and FFR) while preserving the
    /// `has_sve` capability flag and the `sve_vl_inherit` setting.
    ///
    /// This is used on `execve`, and matches Linux's `fpsimd_flush_thread`, which discards the
    /// FPSIMD and SVE register state but leaves `TIF_SVE_VL_INHERIT` untouched.
    pub fn reset(&mut self) {
        let has_sve = self.has_sve;
        let sve_vl_inherit = self.sve_vl_inherit;
        *self = Default::default();
        self.has_sve = has_sve;
        self.sve_vl_inherit = sve_vl_inherit;
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::*;
    use core::arch::asm;

    #[fuchsia::test]
    fn test_save_restore_64() {
        let mut state = crate::ExtendedPstateState::default();
        let mut pstate_ptr_struct = crate::ExtendedPstatePointer { extended_pstate: &mut state };
        let pstate_ptr = &mut pstate_ptr_struct as *mut crate::ExtendedPstatePointer;

        let mut restored_state = State::default();
        let buffer_ptr = &mut restored_state as *mut State as *mut u8;

        let sentinel_q: u128 = 0xDEADBEEF_DEADBEEF_DEADBEEF_DEADBEEF_u128;
        let sentinel_fpcr: u64 = 0x01000000; // FZ bit
        let sentinel_fpsr: u64 = 0;

        let sentinels_q = [sentinel_q; 32];

        // SAFETY: all memory accesses are to mutable variables on the stack and all clobbers are
        // specified.
        unsafe {
            asm!(
                // 1. Load sentinels into registers
                "ldp q0, q1, [{sentinels_q}]",
                "ldp q2, q3, [{sentinels_q}, #32]",
                "ldp q4, q5, [{sentinels_q}, #64]",
                "ldp q6, q7, [{sentinels_q}, #96]",
                "ldp q8, q9, [{sentinels_q}, #128]",
                "ldp q10, q11, [{sentinels_q}, #160]",
                "ldp q12, q13, [{sentinels_q}, #192]",
                "ldp q14, q15, [{sentinels_q}, #224]",
                "ldp q16, q17, [{sentinels_q}, #256]",
                "ldp q18, q19, [{sentinels_q}, #288]",
                "ldp q20, q21, [{sentinels_q}, #320]",
                "ldp q22, q23, [{sentinels_q}, #352]",
                "ldp q24, q25, [{sentinels_q}, #384]",
                "ldp q26, q27, [{sentinels_q}, #416]",
                "ldp q28, q29, [{sentinels_q}, #448]",
                "ldp q30, q31, [{sentinels_q}, #480]",
                "msr fpcr, {sentinel_fpcr_reg}",
                "msr fpsr, {sentinel_fpsr_reg}",

                // 2. Call save routine
                "mov x0, {pstate_ptr}",
                "bl {save_fn}",

                // 3. Zero registers
                "eor v0.16b, v0.16b, v0.16b",
                "eor v1.16b, v1.16b, v1.16b",
                "eor v2.16b, v2.16b, v2.16b",
                "eor v3.16b, v3.16b, v3.16b",
                "eor v4.16b, v4.16b, v4.16b",
                "eor v5.16b, v5.16b, v5.16b",
                "eor v6.16b, v6.16b, v6.16b",
                "eor v7.16b, v7.16b, v7.16b",
                "eor v8.16b, v8.16b, v8.16b",
                "eor v9.16b, v9.16b, v9.16b",
                "eor v10.16b, v10.16b, v10.16b",
                "eor v11.16b, v11.16b, v11.16b",
                "eor v12.16b, v12.16b, v12.16b",
                "eor v13.16b, v13.16b, v13.16b",
                "eor v14.16b, v14.16b, v14.16b",
                "eor v15.16b, v15.16b, v15.16b",
                "eor v16.16b, v16.16b, v16.16b",
                "eor v17.16b, v17.16b, v17.16b",
                "eor v18.16b, v18.16b, v18.16b",
                "eor v19.16b, v19.16b, v19.16b",
                "eor v20.16b, v20.16b, v20.16b",
                "eor v21.16b, v21.16b, v21.16b",
                "eor v22.16b, v22.16b, v22.16b",
                "eor v23.16b, v23.16b, v23.16b",
                "eor v24.16b, v24.16b, v24.16b",
                "eor v25.16b, v25.16b, v25.16b",
                "eor v26.16b, v26.16b, v26.16b",
                "eor v27.16b, v27.16b, v27.16b",
                "eor v28.16b, v28.16b, v28.16b",
                "eor v29.16b, v29.16b, v29.16b",
                "eor v30.16b, v30.16b, v30.16b",
                "eor v31.16b, v31.16b, v31.16b",
                "msr fpcr, xzr",
                "msr fpsr, xzr",

                // 4. Call restore routine
                "mov x0, {pstate_ptr}",
                "bl {restore_fn}",

                // 5. Save registers to buffer
                "stp q0, q1, [{buffer_ptr_out}]",
                "stp q2, q3, [{buffer_ptr_out}, #32]",
                "stp q4, q5, [{buffer_ptr_out}, #64]",
                "stp q6, q7, [{buffer_ptr_out}, #96]",
                "stp q8, q9, [{buffer_ptr_out}, #128]",
                "stp q10, q11, [{buffer_ptr_out}, #160]",
                "stp q12, q13, [{buffer_ptr_out}, #192]",
                "stp q14, q15, [{buffer_ptr_out}, #224]",
                "stp q16, q17, [{buffer_ptr_out}, #256]",
                "stp q18, q19, [{buffer_ptr_out}, #288]",
                "stp q20, q21, [{buffer_ptr_out}, #320]",
                "stp q22, q23, [{buffer_ptr_out}, #352]",
                "stp q24, q25, [{buffer_ptr_out}, #384]",
                "stp q26, q27, [{buffer_ptr_out}, #416]",
                "stp q28, q29, [{buffer_ptr_out}, #448]",
                "stp q30, q31, [{buffer_ptr_out}, #480]",
                "mrs x2, fpcr",
                "str w2, [{buffer_ptr_out}, #512]",
                "mrs x3, fpsr",
                "str w3, [{buffer_ptr_out}, #516]",

                sentinels_q = in(reg) &sentinels_q,
                sentinel_fpcr_reg = in(reg) sentinel_fpcr,
                sentinel_fpsr_reg = in(reg) sentinel_fpsr,
                pstate_ptr = in(reg) pstate_ptr,
                buffer_ptr_out = in(reg) buffer_ptr,
                save_fn = sym save_extended_pstate,
                restore_fn = sym restore_extended_pstate,
                clobber_abi("C"),
                out("x0") _,
                out("x1") _,
                out("x2") _,
                out("x3") _,
                out("x4") _,
                out("v0") _, out("v1") _, out("v2") _, out("v3") _,
                out("v4") _, out("v5") _, out("v6") _, out("v7") _,
                out("v8") _, out("v9") _, out("v10") _, out("v11") _,
                out("v12") _, out("v13") _, out("v14") _, out("v15") _,
                out("v16") _, out("v17") _, out("v18") _, out("v19") _,
                out("v20") _, out("v21") _, out("v22") _, out("v23") _,
                out("v24") _, out("v25") _, out("v26") _, out("v27") _,
                out("v28") _, out("v29") _, out("v30") _, out("v31") _,
                out("x30") _,
            );
        }

        // Assertions
        let state_val = state.get_arm64_qregs();
        for i in 0..32 {
            assert_eq!(state_val[i], sentinel_q, "state.q[{}] mismatch", i);
        }
        assert_eq!(state.get_arm64_fpcr(), sentinel_fpcr as u32, "state.fpcr mismatch");
        assert_eq!(state.get_arm64_fpsr(), sentinel_fpsr as u32, "state.fpsr mismatch");

        for i in 0..32 {
            assert_eq!(restored_state.q[i], sentinel_q, "restored_state.q[{}] mismatch", i);
        }
        assert_eq!(restored_state.fpcr, sentinel_fpcr as u32, "restored_state.fpcr mismatch");
        assert_eq!(restored_state.fpsr, sentinel_fpsr as u32, "restored_state.fpsr mismatch");
    }

    #[fuchsia::test]
    fn test_sve_state_accessors_and_reset() {
        let mut state = crate::ExtendedPstateState::with_sve(true);
        assert!(state.has_arm64_sve());

        let qregs = [0x0102030405060708090a0b0c0d0e0f10u128; 32];
        let pregs = [0xa55au16; 16];
        state.set_arm64_state(&qregs, 0x11, 0x22);
        // Non-contiguous FFR bits above the first 0 bit must be masked off to a contiguous prefix
        // of 1s starting from bit 0 (`0b1010_0111` -> `0b0000_0111`).
        state.set_arm64_sve_state(&pregs, 0b1010_0111);
        assert_eq!(state.get_arm64_p_regs(), &pregs);
        assert_eq!(state.get_arm64_ffr(), 0b0000_0111);

        state.set_arm64_sve_state(&pregs, 0xffff);
        assert_eq!(state.get_arm64_ffr(), 0xffff);

        state.reset_arm64_sve_state();
        assert_eq!(state.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(state.get_arm64_ffr(), 0);
        assert_eq!(state.get_arm64_qregs(), &qregs);

        // The `PR_SVE_VL_INHERIT` setting defaults to off and is independent of the register state.
        assert!(!state.get_arm64_sve_vl_inherit());
        state.set_arm64_sve_vl_inherit(true);
        assert!(state.get_arm64_sve_vl_inherit());
        assert_eq!(state.get_arm64_qregs(), &qregs);

        // `reset` clears Q0-Q31, P0-P15, FFR, FPCR, and FPSR while preserving `has_sve` and
        // `sve_vl_inherit`, regardless of whether the host actually supports SVE.
        state.set_arm64_sve_state(&pregs, 0x00ff);
        state.reset();
        assert!(state.has_arm64_sve());
        assert!(state.get_arm64_sve_vl_inherit());
        assert_eq!(state.get_arm64_qregs(), &[0u128; 32]);
        assert_eq!(state.get_arm64_p_regs(), &[0u16; 16]);
        assert_eq!(state.get_arm64_ffr(), 0);
        assert_eq!(state.get_arm64_fpcr(), 0);
        assert_eq!(state.get_arm64_fpsr(), 0);

        state.set_arm64_sve_vl_inherit(false);
        state.reset();
        assert!(state.has_arm64_sve());
        assert!(!state.get_arm64_sve_vl_inherit());

        // A state without SVE support keeps reporting `has_sve == false` across `reset`.
        let mut non_sve_state = crate::ExtendedPstateState::with_sve(false);
        assert!(!non_sve_state.has_arm64_sve());
        non_sve_state.reset();
        assert!(!non_sve_state.has_arm64_sve());
    }

    #[fuchsia::test]
    fn test_sve_flags_preserved_by_reset_and_copy() {
        // `HAS_SVE` (the host CPU's capability) only determines the `Default` value of `has_sve`.
        assert_eq!(crate::ExtendedPstateState::default().has_arm64_sve(), *HAS_SVE);
        assert!(!crate::ExtendedPstateState::default().get_arm64_sve_vl_inherit());

        // Regardless of `HAS_SVE`, `reset` (execve) and copying the state (fork/clone) must
        // preserve `has_sve` and `sve_vl_inherit` exactly as they were set, for every combination
        // of the two flags.
        let qregs = [0x0f0e0d0c0b0a09080706050403020100u128; 32];
        let pregs = [0x1234u16; 16];
        for has_sve in [false, true] {
            for inherit in [false, true] {
                let context = format!("has_sve={has_sve} inherit={inherit}");
                let mut state = crate::ExtendedPstateState::with_sve(has_sve);
                assert_eq!(state.has_arm64_sve(), has_sve, "{context}");
                assert!(!state.get_arm64_sve_vl_inherit(), "{context}");
                state.set_arm64_state(&qregs, 0x11, 0x22);
                state.set_arm64_sve_state(&pregs, 0x000f);
                state.set_arm64_sve_vl_inherit(inherit);
                assert_eq!(state.has_arm64_sve(), has_sve, "{context}");
                assert_eq!(state.get_arm64_sve_vl_inherit(), inherit, "{context}");

                // A copy (`ExtendedPstateState` is `Copy`, which is what the `Clone` of the
                // boxed storage in Starnix relies on) carries over both flags and all registers.
                let copy = state;
                assert_eq!(copy.has_arm64_sve(), has_sve, "{context}");
                assert_eq!(copy.get_arm64_sve_vl_inherit(), inherit, "{context}");
                assert_eq!(copy.get_arm64_qregs(), &qregs, "{context}");
                assert_eq!(copy.get_arm64_fpsr(), 0x11, "{context}");
                assert_eq!(copy.get_arm64_fpcr(), 0x22, "{context}");
                assert_eq!(copy.get_arm64_p_regs(), &pregs, "{context}");
                assert_eq!(copy.get_arm64_ffr(), 0x000f, "{context}");

                // `reset` zeroes every register but keeps both flags, and leaves the copy alone.
                state.reset();
                assert_eq!(state.has_arm64_sve(), has_sve, "{context}");
                assert_eq!(state.get_arm64_sve_vl_inherit(), inherit, "{context}");
                assert_eq!(state.get_arm64_qregs(), &[0u128; 32], "{context}");
                assert_eq!(state.get_arm64_fpsr(), 0, "{context}");
                assert_eq!(state.get_arm64_fpcr(), 0, "{context}");
                assert_eq!(state.get_arm64_p_regs(), &[0u16; 16], "{context}");
                assert_eq!(state.get_arm64_ffr(), 0, "{context}");
                assert_eq!(copy.has_arm64_sve(), has_sve, "{context}");
                assert_eq!(copy.get_arm64_sve_vl_inherit(), inherit, "{context}");
                assert_eq!(copy.get_arm64_qregs(), &qregs, "{context}");
                assert_eq!(copy.get_arm64_p_regs(), &pregs, "{context}");
                assert_eq!(copy.get_arm64_ffr(), 0x000f, "{context}");

                // Toggling `sve_vl_inherit` and resetting again never changes `has_sve`, and
                // resetting only the SVE registers never changes either flag.
                state.set_arm64_sve_vl_inherit(!inherit);
                state.reset();
                assert_eq!(state.has_arm64_sve(), has_sve, "{context}");
                assert_eq!(state.get_arm64_sve_vl_inherit(), !inherit, "{context}");
                state.set_arm64_sve_state(&pregs, 0xffff);
                state.reset_arm64_sve_state();
                assert_eq!(state.has_arm64_sve(), has_sve, "{context}");
                assert_eq!(state.get_arm64_sve_vl_inherit(), !inherit, "{context}");
                assert_eq!(state.get_arm64_p_regs(), &[0u16; 16], "{context}");
                assert_eq!(state.get_arm64_ffr(), 0, "{context}");
            }
        }
    }

    #[fuchsia::test]
    fn test_sve_ffr_canonicalization() {
        // `set_arm64_sve_state` stores architecturally valid FFR values (a contiguous run of 1s
        // starting at bit 0) unchanged and masks any other value down to its valid prefix, so that
        // `wrffr` in `restore_extended_pstate` always has well-defined behavior. P0-P15 are stored
        // verbatim, since any bit pattern is a valid predicate.
        let cases: [(u16, u16); 11] = [
            (0x0000, 0x0000),
            (0x0001, 0x0001),
            (0x0003, 0x0003),
            (0b1000_0000, 0),
            (0b1010_0111, 0b0000_0111),
            (0x00ff, 0x00ff),
            (0x7fff, 0x7fff),
            (0x8001, 0x0001),
            (0xfffe, 0),
            (0xbfff, 0x3fff),
            (0xffff, 0xffff),
        ];
        let mut state = crate::ExtendedPstateState::with_sve(true);
        for (raw, canonical) in cases {
            let pregs = [raw; 16];
            state.set_arm64_sve_state(&pregs, raw);
            assert_eq!(state.get_arm64_ffr(), canonical, "ffr={raw:#06x}");
            assert_eq!(state.get_arm64_p_regs(), &pregs, "ffr={raw:#06x}");
            assert!(state.has_arm64_sve());
        }
    }

    #[fuchsia::test]
    fn test_sve_vector_length() {
        if !*HAS_SVE {
            return;
        }
        // Starnix relies on Zircon configuring a 128-bit SVE vector length.
        assert_eq!(read_sve_vl_bytes(), SVE_VL_BYTES);
    }

    #[fuchsia::test]
    fn test_save_restore_64_sve() {
        if !*HAS_SVE {
            return;
        }

        let mut state = crate::ExtendedPstateState::default();
        let mut pstate_ptr_struct = crate::ExtendedPstatePointer { extended_pstate: &mut state };
        let pstate_ptr = &mut pstate_ptr_struct as *mut crate::ExtendedPstatePointer;

        let mut sentinels_p = [0u16; 16];
        for i in 0..16 {
            sentinels_p[i] = 0x5a00 | (i as u16 + 1);
        }
        let sentinel_ffr: u16 = 0x00ff;
        let mut restored_p = [0u16; 16];
        let mut restored_ffr: u16 = 0;

        // SAFETY: all memory accesses are to mutable variables on the stack and SVE is supported.
        unsafe {
            asm!(
                ".arch_extension sve",
                // 1. Load sentinels into P0-P15 and FFR
                "ldr  p0, [{sentinels_p}, #0, mul vl]",
                "ldr  p1, [{sentinels_p}, #1, mul vl]",
                "ldr  p2, [{sentinels_p}, #2, mul vl]",
                "ldr  p3, [{sentinels_p}, #3, mul vl]",
                "ldr  p4, [{sentinels_p}, #4, mul vl]",
                "ldr  p5, [{sentinels_p}, #5, mul vl]",
                "ldr  p6, [{sentinels_p}, #6, mul vl]",
                "ldr  p7, [{sentinels_p}, #7, mul vl]",
                "ldr  p8, [{sentinels_p}, #8, mul vl]",
                "ldr  p9, [{sentinels_p}, #9, mul vl]",
                "ldr p10, [{sentinels_p}, #10, mul vl]",
                "ldr p11, [{sentinels_p}, #11, mul vl]",
                "ldr p12, [{sentinels_p}, #12, mul vl]",
                "ldr p13, [{sentinels_p}, #13, mul vl]",
                "ldr p14, [{sentinels_p}, #14, mul vl]",
                "ldr p15, [{sentinels_p}, #15, mul vl]",
                "ldr  p0, [{sentinel_ffr}]",
                "wrffr p0.b",
                "ldr  p0, [{sentinels_p}, #0, mul vl]",

                // 2. Call save routine
                "mov x0, {pstate_ptr}",
                "bl {save_fn}",

                // 3. Zero P0-P15 and FFR
                "pfalse p0.b",
                "pfalse p1.b",
                "pfalse p2.b",
                "pfalse p3.b",
                "pfalse p4.b",
                "pfalse p5.b",
                "pfalse p6.b",
                "pfalse p7.b",
                "pfalse p8.b",
                "pfalse p9.b",
                "pfalse p10.b",
                "pfalse p11.b",
                "pfalse p12.b",
                "pfalse p13.b",
                "pfalse p14.b",
                "pfalse p15.b",
                "wrffr p0.b",

                // 4. Call restore routine
                "mov x0, {pstate_ptr}",
                "bl {restore_fn}",

                // 5. Save restored P0-P15 and FFR
                "str  p0, [{restored_p}, #0, mul vl]",
                "str  p1, [{restored_p}, #1, mul vl]",
                "str  p2, [{restored_p}, #2, mul vl]",
                "str  p3, [{restored_p}, #3, mul vl]",
                "str  p4, [{restored_p}, #4, mul vl]",
                "str  p5, [{restored_p}, #5, mul vl]",
                "str  p6, [{restored_p}, #6, mul vl]",
                "str  p7, [{restored_p}, #7, mul vl]",
                "str  p8, [{restored_p}, #8, mul vl]",
                "str  p9, [{restored_p}, #9, mul vl]",
                "str p10, [{restored_p}, #10, mul vl]",
                "str p11, [{restored_p}, #11, mul vl]",
                "str p12, [{restored_p}, #12, mul vl]",
                "str p13, [{restored_p}, #13, mul vl]",
                "str p14, [{restored_p}, #14, mul vl]",
                "str p15, [{restored_p}, #15, mul vl]",
                "rdffr p0.b",
                "str  p0, [{restored_ffr}]",
                ".arch_extension nosve",

                sentinels_p = in(reg) &sentinels_p,
                sentinel_ffr = in(reg) &sentinel_ffr,
                pstate_ptr = in(reg) pstate_ptr,
                restored_p = in(reg) &mut restored_p,
                restored_ffr = in(reg) &mut restored_ffr,
                save_fn = sym save_extended_pstate,
                restore_fn = sym restore_extended_pstate,
                clobber_abi("C"),
                out("x0") _,
                out("x1") _,
                out("x2") _,
                out("x3") _,
                out("x4") _,
                out("v0") _, out("v1") _, out("v2") _, out("v3") _,
                out("v4") _, out("v5") _, out("v6") _, out("v7") _,
                out("v8") _, out("v9") _, out("v10") _, out("v11") _,
                out("v12") _, out("v13") _, out("v14") _, out("v15") _,
                out("v16") _, out("v17") _, out("v18") _, out("v19") _,
                out("v20") _, out("v21") _, out("v22") _, out("v23") _,
                out("v24") _, out("v25") _, out("v26") _, out("v27") _,
                out("v28") _, out("v29") _, out("v30") _, out("v31") _,
                out("p0") _, out("p1") _, out("p2") _, out("p3") _,
                out("p4") _, out("p5") _, out("p6") _, out("p7") _,
                out("p8") _, out("p9") _, out("p10") _, out("p11") _,
                out("p12") _, out("p13") _, out("p14") _, out("p15") _,
                out("x30") _,
            );
        }

        assert_eq!(state.get_arm64_p_regs(), &sentinels_p);
        assert_eq!(state.get_arm64_ffr(), sentinel_ffr);
        assert_eq!(restored_p, sentinels_p);
        assert_eq!(restored_ffr, sentinel_ffr);
    }

    #[fuchsia::test]
    fn test_save_restore_32() {
        let mut state = crate::ExtendedAarch32PstateState::default();
        let mut pstate_ptr_struct =
            crate::ExtendedPstatePointer { extended_aarch32_pstate: &mut state };
        let pstate_ptr = &mut pstate_ptr_struct as *mut crate::ExtendedPstatePointer;

        let mut restored_state = Aarch32State::default();
        let buffer_ptr = &mut restored_state as *mut Aarch32State as *mut u8;

        let sentinel_q: u128 = 0xDEADBEEF_DEADBEEF_DEADBEEF_DEADBEEF_u128;
        let sentinel_fpcr: u64 = 0x01000000;
        let sentinel_fpsr: u64 = 0;

        let sentinels_q = [sentinel_q; 16];

        // SAFETY: all memory accesses are to mutable variables on the stack and all clobbers are
        // specified.
        unsafe {
            asm!(
                // 1. Load sentinels into registers
                "ldp q0, q1, [{sentinels_q}]",
                "ldp q2, q3, [{sentinels_q}, #32]",
                "ldp q4, q5, [{sentinels_q}, #64]",
                "ldp q6, q7, [{sentinels_q}, #96]",
                "ldp q8, q9, [{sentinels_q}, #128]",
                "ldp q10, q11, [{sentinels_q}, #160]",
                "ldp q12, q13, [{sentinels_q}, #192]",
                "ldp q14, q15, [{sentinels_q}, #224]",
                "msr fpcr, {sentinel_fpcr_reg}",
                "msr fpsr, {sentinel_fpsr_reg}",

                // 2. Call save routine
                "mov x0, {pstate_ptr}",
                "bl {save_fn}",

                // 3. Zero registers
                "eor v0.16b, v0.16b, v0.16b",
                "eor v1.16b, v1.16b, v1.16b",
                "eor v2.16b, v2.16b, v2.16b",
                "eor v3.16b, v3.16b, v3.16b",
                "eor v4.16b, v4.16b, v4.16b",
                "eor v5.16b, v5.16b, v5.16b",
                "eor v6.16b, v6.16b, v6.16b",
                "eor v7.16b, v7.16b, v7.16b",
                "eor v8.16b, v8.16b, v8.16b",
                "eor v9.16b, v9.16b, v9.16b",
                "eor v10.16b, v10.16b, v10.16b",
                "eor v11.16b, v11.16b, v11.16b",
                "eor v12.16b, v12.16b, v12.16b",
                "eor v13.16b, v13.16b, v13.16b",
                "eor v14.16b, v14.16b, v14.16b",
                "eor v15.16b, v15.16b, v15.16b",
                "msr fpcr, xzr",
                "msr fpsr, xzr",

                // 4. Call restore routine
                "mov x0, {pstate_ptr}",
                "bl {restore_fn}",

                // 5. Save registers to buffer
                "stp q0, q1, [{buffer_ptr_out}]",
                "stp q2, q3, [{buffer_ptr_out}, #32]",
                "stp q4, q5, [{buffer_ptr_out}, #64]",
                "stp q6, q7, [{buffer_ptr_out}, #96]",
                "stp q8, q9, [{buffer_ptr_out}, #128]",
                "stp q10, q11, [{buffer_ptr_out}, #160]",
                "stp q12, q13, [{buffer_ptr_out}, #192]",
                "stp q14, q15, [{buffer_ptr_out}, #224]",
                "mrs x2, fpcr",
                "str w2, [{buffer_ptr_out}, #256]",
                "mrs x3, fpsr",
                "str w3, [{buffer_ptr_out}, #260]",

                sentinels_q = in(reg) &sentinels_q,
                sentinel_fpcr_reg = in(reg) sentinel_fpcr,
                sentinel_fpsr_reg = in(reg) sentinel_fpsr,
                pstate_ptr = in(reg) pstate_ptr,
                buffer_ptr_out = in(reg) buffer_ptr,
                save_fn = sym save_extended_aarch32_pstate,
                restore_fn = sym restore_extended_aarch32_pstate,
                clobber_abi("C"),
                out("x0") _,
                out("x1") _,
                out("x2") _,
                out("x3") _,
                out("x4") _,
                out("v0") _, out("v1") _, out("v2") _, out("v3") _,
                out("v4") _, out("v5") _, out("v6") _, out("v7") _,
                out("v8") _, out("v9") _, out("v10") _, out("v11") _,
                out("v12") _, out("v13") _, out("v14") _, out("v15") _,
                out("x30") _,
            );
        }

        // Assertions
        let state_val = state.get_arm32_qregs();
        for i in 0..16 {
            assert_eq!(state_val[i], sentinel_q, "state.q[{}] mismatch", i);
        }
        assert_eq!(state.get_arm32_fpcr(), sentinel_fpcr as u32, "state.fpcr mismatch");
        assert_eq!(state.get_arm32_fpsr(), sentinel_fpsr as u32, "state.fpsr mismatch");

        for i in 0..16 {
            assert_eq!(restored_state.q[i], sentinel_q, "restored_state.q[{}] mismatch", i);
        }
        assert_eq!(restored_state.fpcr, sentinel_fpcr as u32, "restored_state.fpcr mismatch");
        assert_eq!(restored_state.fpsr, sentinel_fpsr as u32, "restored_state.fpsr mismatch");
    }
}
