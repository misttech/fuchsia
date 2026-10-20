// Copyright 2023 The Fuchsia Authors
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "x86_64")]
pub use x86_64::XSAVE_AREA_SIZE as X64_XSAVE_AREA_SIZE;

#[cfg(target_arch = "x86_64")]
pub use x86_64::SUPPORTED_XSAVE_FEATURES as X64_SUPPORTED_XSAVE_FEATURES;

#[cfg(target_arch = "aarch64")]
mod aarch64;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;

#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct ExtendedPstateState {
    #[cfg(target_arch = "x86_64")]
    state: x86_64::State,

    #[cfg(target_arch = "aarch64")]
    state: aarch64::State,

    #[cfg(target_arch = "riscv64")]
    state: riscv64::State,
}

#[cfg(target_arch = "aarch64")]
/// A version of [`ExtendedPstateState`] that only stores the processor state
/// accessible from AArch32 (e.g., registers Q0-Q15).
#[derive(Clone, Copy, Default)]
#[repr(C)]
pub struct ExtendedAarch32PstateState {
    state: aarch64::Aarch32State,
}

#[cfg(target_arch = "aarch64")]
impl ExtendedAarch32PstateState {
    pub fn reset(&mut self) {
        self.state.reset()
    }

    pub fn get_arm32_qregs(&self) -> &[u128; 16] {
        &self.state.q
    }

    pub fn get_arm32_fpsr(&self) -> u32 {
        self.state.fpsr
    }

    pub fn get_arm32_fpcr(&self) -> u32 {
        self.state.fpcr
    }

    pub fn set_arm32_state(&mut self, qregs: &[u128; 16], fpsr: u32, fpcr: u32) {
        self.state.q = *qregs;
        self.state.fpsr = fpsr;
        self.state.fpcr = fpcr;
    }
}

impl ExtendedPstateState {
    #[cfg(target_arch = "x86_64")]
    pub fn with_strategy(strategy: x86_64::Strategy) -> Self {
        Self { state: x86_64::State::with_strategy(strategy) }
    }

    pub fn reset(&mut self) {
        self.state.reset()
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_arm64_qregs(&self) -> &[u128; 32] {
        &self.state.q
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_arm64_fpsr(&self) -> u32 {
        self.state.fpsr
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_arm64_fpcr(&self) -> u32 {
        self.state.fpcr
    }

    #[cfg(target_arch = "aarch64")]
    pub fn set_arm64_state(&mut self, qregs: &[u128; 32], fpsr: u32, fpcr: u32) {
        self.state.q = *qregs;
        self.state.fpsr = fpsr;
        self.state.fpcr = fpcr;
    }

    #[cfg(target_arch = "aarch64")]
    pub fn with_sve(has_sve: bool) -> Self {
        Self { state: aarch64::State { has_sve: u8::from(has_sve), ..Default::default() } }
    }

    #[cfg(target_arch = "aarch64")]
    pub fn has_arm64_sve(&self) -> bool {
        self.state.has_sve != 0
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_arm64_p_regs(&self) -> &[u16; 16] {
        &self.state.p
    }

    #[cfg(target_arch = "aarch64")]
    pub fn get_arm64_ffr(&self) -> u16 {
        self.state.ffr
    }

    #[cfg(target_arch = "aarch64")]
    pub fn set_arm64_sve_state(&mut self, p_regs: &[u16; 16], ffr: u16) {
        self.state.p = *p_regs;
        // Per the Arm ARM for WRFFR, FFR must be a monotonically decreasing sequence of 1s
        // starting from bit 0 (2^k - 1 for 0 <= k <= 16), and the result of writing any other
        // value is not defined by the architecture. Linux copies a user-supplied FFR (e.g. from
        // a signal frame) verbatim into the task's SVE state and leaves it to the hardware; Starnix
        // instead canonicalizes the value by masking it to the contiguous prefix of 1s starting
        // from bit 0, so that `wrffr` in `restore_extended_pstate` always has well-defined
        // behavior. Architecturally valid values are stored unchanged.
        self.state.ffr = ((1u32 << ffr.trailing_ones()) - 1) as u16;
    }

    #[cfg(target_arch = "aarch64")]
    pub fn reset_arm64_sve_state(&mut self) {
        self.state.p = [0; 16];
        self.state.ffr = 0;
    }

    /// Whether the SVE vector length is inherited across `execve` (`PR_SVE_VL_INHERIT`).
    #[cfg(target_arch = "aarch64")]
    pub fn get_arm64_sve_vl_inherit(&self) -> bool {
        self.state.sve_vl_inherit != 0
    }

    #[cfg(target_arch = "aarch64")]
    pub fn set_arm64_sve_vl_inherit(&mut self, inherit: bool) {
        self.state.sve_vl_inherit = u8::from(inherit);
    }

    #[cfg(target_arch = "riscv64")]
    pub fn get_riscv64_state(&self) -> &riscv64::State {
        &self.state
    }

    #[cfg(target_arch = "riscv64")]
    pub fn get_riscv64_state_mut(&mut self) -> &mut riscv64::State {
        &mut self.state
    }

    #[cfg(target_arch = "x86_64")]
    pub fn get_x64_xsave_area(&self) -> [u8; X64_XSAVE_AREA_SIZE] {
        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        unsafe {
            std::mem::transmute(self.state.buffer)
        }
    }

    #[cfg(target_arch = "x86_64")]
    pub fn set_x64_xsave_area(&mut self, xsave_area: [u8; X64_XSAVE_AREA_SIZE]) {
        self.state.set_xsave_area(xsave_area);
    }
}

/// Stores a pointer to the currently active extended pstate storage.
/// The caller to the C entry points is responsible for ensuring that the active union
/// member corresponds to the entry points being called.
#[repr(C)]
pub union ExtendedPstatePointer {
    pub extended_pstate: *mut ExtendedPstateState,
    #[cfg(target_arch = "aarch64")]
    pub extended_aarch32_pstate: *mut ExtendedAarch32PstateState,
}

// Provided by assembly targets.
unsafe extern "C" {
    pub fn save_extended_pstate(state_addr: usize);
    pub fn restore_extended_pstate(state_addr: usize);

    #[cfg(target_arch = "aarch64")]
    pub fn save_extended_aarch32_pstate(state_addr: usize);
    #[cfg(target_arch = "aarch64")]
    pub fn restore_extended_aarch32_pstate(state_addr: usize);
}
