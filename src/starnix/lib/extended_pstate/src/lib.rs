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
pub struct ExtendedAarch32PstateState {
    state: aarch64::Aarch32State,
}

#[cfg(target_arch = "aarch64")]
impl ExtendedAarch32PstateState {
    #[inline(always)]
    pub fn save(&mut self) {
        self.state.save()
    }

    /// This restores the extended processor state saved in this object into the processor's state
    /// registers.
    ///
    /// # Safety
    ///
    /// This clobbers the current vector register, floating point register, and floating
    /// point status and control register state including callee-saved registers. This should be
    /// used in conjunction with save() to switch to an alternate extended processor state.
    #[inline(always)]
    pub unsafe fn restore(&self) {
        #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
        unsafe {
            self.state.restore()
        }
    }

    pub fn reset(&mut self) {
        self.state.reset()
    }
}

impl ExtendedPstateState {
    #[cfg(target_arch = "x86_64")]
    pub fn with_strategy(strategy: x86_64::Strategy) -> Self {
        Self { state: x86_64::State::with_strategy(strategy) }
    }

    /// This saves the current extended processor state to this state object.
    #[inline(always)]
    fn save(&mut self) {
        self.state.save()
    }

    #[inline(always)]
    /// This restores the extended processor state saved in this object into the processor's state
    /// registers.
    ///
    /// Safety: This clobbers the current vector register, floating point register, and floating
    /// point status and control register state including callee-saved registers. This should be
    /// used in conjunction with save() to switch to an alternate extended processor state.
    unsafe fn restore(&self) {
        #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
        unsafe {
            self.state.restore()
        }
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

#[unsafe(no_mangle)]
/// Restores the current extended architectural process state.
///
/// # Safety
///    - state_addr must point to an instance of ExtendedPstateState.
pub unsafe extern "C" fn restore_extended_pstate(state_addr: usize) {
    let state = state_addr as *const ExtendedPstateState;
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        (&*state).restore()
    }
}

#[unsafe(no_mangle)]
/// Save the current extended architectural process state.
///
/// # Safety
///    - state_addr must point to an exclusively owned instance of ExtendedPstateState.
pub unsafe extern "C" fn save_extended_pstate(state_addr: usize) {
    let state = state_addr as *mut ExtendedPstateState;
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        (&mut *state).save()
    }
}

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
/// Restores the current extended AArch32-visible architectural process state.
///
/// # Safety
///    - state_addr must point to an instance of ExtendedAarch32PstateState.
pub unsafe extern "C" fn restore_extended_aarch32_pstate(state_addr: usize) {
    let state = state_addr as *const ExtendedAarch32PstateState;
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        (&*state).restore()
    }
}

#[cfg(target_arch = "aarch64")]
#[unsafe(no_mangle)]
/// Saves the current extended AArch32-visible architectural process state.
///
/// # Safety
///    - state_addr must point to an exclusively owned instance of ExtendedAarch32PstateState.
pub unsafe extern "C" fn save_extended_aarch32_pstate(state_addr: usize) {
    let state = state_addr as *mut ExtendedAarch32PstateState;
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        (&mut *state).save()
    }
}

#[cfg(test)]
mod test {
    use super::*;

    #[::fuchsia::test]
    fn extended_pstate_state_lifecycle() {
        let mut state = ExtendedPstateState::default();
        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        unsafe {
            state.save();
            state.restore();
        }
    }
}
