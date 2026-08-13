// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Architecture-specific saved normal mode state for riscv64.
///
/// Currently riscv64 does not need to save any normal mode state across restricted entry.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchSavedNormalState {
    _dummy: u8,
}

const _: () = {
    assert!(core::mem::size_of::<ArchSavedNormalState>() == 1);
    assert!(core::mem::align_of::<ArchSavedNormalState>() == 1);
};

#[allow(unused_imports)]
pub use arch_types_bindings::{GeneralRegsSource, UserEntryState};

use debug::ltracef;
use zx_status::Status;
use zx_types::{zx_restricted_state_t, zx_status_t, zx_thread_state_general_regs_t};

const LOCAL_TRACE: u32 = 0;

/// Canonical address mask for RISC-V 64 Sv39 virtual addresses.
///
/// [riscv/priv/v1.12]: Section 4.4.1 (Sv39: Page-Based 39-bit Virtual-Memory System)
const RISCV64_CANONICAL_ADDRESS_MASK: usize = !((1usize << 38) - 1);

/// Supervisor Previous Interrupt Enable bit in `sstatus` CSR.
///
/// [riscv/priv/v1.12]: Section 3.1.6.1 (Supervisor Status Register sstatus)
const RISCV64_CSR_SSTATUS_SPIE: u64 = 1u64 << 5;

/// Supervisor User Extended Length (UXL) set to 64-bit in `sstatus` CSR.
///
/// [riscv/priv/v1.12]: Section 3.1.6.3 (Base ISA Control in sstatus Register)
const RISCV64_CSR_SSTATUS_UXL_64BIT: u64 = 2u64 << 32;

unsafe extern "C" {
    fn cpp_riscv64_curr_hart_id() -> u32;
    fn cpp_riscv64_boot_hart_id() -> u32;
    fn cpp_riscv64_ints_disabled() -> bool;
    fn cpp_riscv64_get_sstatus_fp_v() -> u64;
    fn cpp_riscv64_enter_uspace(iframe: *const Iframe) -> !;
    fn cpp_riscv64_get_general_regs(regs: *mut zx_thread_state_general_regs_t) -> zx_status_t;
    fn cpp_riscv64_set_general_regs(regs: *const zx_thread_state_general_regs_t) -> zx_status_t;
}

#[inline(always)]
fn ints_disabled() -> bool {
    // SAFETY: Reads interrupt status for the current CPU.
    unsafe { cpp_riscv64_ints_disabled() }
}

/// Returns the HART ID of the currently executing hardware thread.
#[inline(always)]
pub fn curr_hart_id() -> u32 {
    // SAFETY: cpp_riscv64_curr_hart_id is a read-only FFI call returning the current HART ID with no side effects.
    unsafe { cpp_riscv64_curr_hart_id() }
}

/// Returns the HART ID of the boot hardware thread.
#[inline(always)]
pub fn boot_hart_id() -> u32 {
    // SAFETY: cpp_riscv64_boot_hart_id is a read-only FFI call returning the boot HART ID with no side effects.
    unsafe { cpp_riscv64_boot_hart_id() }
}

#[repr(C, align(16))]
#[derive(Debug, Default, Clone, Copy)]
pub struct Iframe {
    pub status: u64,
    pub regs: zx_restricted_state_t,
}

pub type SyscallRegs = Iframe;

const _: () = {
    assert!(core::mem::size_of::<Iframe>() == 272);
    assert!(core::mem::align_of::<Iframe>() == 16);
    assert!(core::mem::size_of::<SyscallRegs>() == core::mem::size_of::<Iframe>());
    assert!(core::mem::align_of::<SyscallRegs>() == core::mem::align_of::<Iframe>());
};

#[inline]
pub fn is_user_accessible(va: usize) -> bool {
    (va & RISCV64_CANONICAL_ADDRESS_MASK) == 0
}

pub fn validate_state_pre_restricted_entry(state: &zx_restricted_state_t) -> Result<(), Status> {
    // Validate that PC is within userspace.
    if !is_user_accessible(state.pc as usize) {
        ltracef!("fail due to bad PC {:#x}\n", state.pc);
        return Err(Status::BAD_STATE);
    }
    Ok(())
}

pub fn dump(state: &zx_restricted_state_t) {
    use core::fmt::Write;
    use debug::ltrace::KernelConsoleWriter;
    let mut w = KernelConsoleWriter;
    let _ = write!(
        w,
        "PC: {:#18x}\nRA: {:#18x}\nSP: {:#18x}\nGP: {:#18x}\nTP: {:#18x}\nT0: {:#18x}\nT1: {:#18x}\nT2: {:#18x}\nS0: {:#18x}\nS1: {:#18x}\nA0: {:#18x}\nA1: {:#18x}\nA2: {:#18x}\nA3: {:#18x}\nA4: {:#18x}\nA5: {:#18x}\nA6: {:#18x}\nA7: {:#18x}\nS2: {:#18x}\nS3: {:#18x}\nS4: {:#18x}\nS5: {:#18x}\nS6: {:#18x}\nS7: {:#18x}\nS8: {:#18x}\nS9: {:#18x}\nS10: {:#18x}\nS11: {:#18x}\nT3: {:#18x}\nT4: {:#18x}\nT5: {:#18x}\nT6: {:#18x}\n",
        state.pc,
        state.ra,
        state.sp,
        state.gp,
        state.tp,
        state.t0,
        state.t1,
        state.t2,
        state.s0,
        state.s1,
        state.a0,
        state.a1,
        state.a2,
        state.a3,
        state.a4,
        state.a5,
        state.a6,
        state.a7,
        state.s2,
        state.s3,
        state.s4,
        state.s5,
        state.s6,
        state.s7,
        state.s8,
        state.s9,
        state.s10,
        state.s11,
        state.t3,
        state.t4,
        state.t5,
        state.t6,
    );
}

pub fn save_state_pre_restricted_entry(_state: &mut ArchSavedNormalState) {}

pub fn enter_restricted(state: &zx_restricted_state_t) -> ! {
    debug_assert!(ints_disabled());
    // Create an iframe for restricted mode and set the status to a reasonable initial value. Keep FP
    // and V status since that register state should be preserved when entering/exiting restricted
    // mode.
    // SAFETY: Reads FP/V status from SSTATUS CSR for current CPU.
    let fp_v_status = unsafe { cpp_riscv64_get_sstatus_fp_v() };
    let iframe = Iframe {
        status: RISCV64_CSR_SSTATUS_SPIE | RISCV64_CSR_SSTATUS_UXL_64BIT | fp_v_status,
        regs: *state,
    };

    // Enter userspace.
    // SAFETY: Enters user space in restricted mode using constructed iframe. Does not return.
    unsafe { cpp_riscv64_enter_uspace(&iframe) };
}

pub fn save_restricted_syscall_state(state: &mut zx_restricted_state_t, regs: &SyscallRegs) {
    debug_assert!(ints_disabled());
    *state = regs.regs;
}

pub fn save_restricted_iframe_state(state: &mut zx_restricted_state_t, frame: &Iframe) {
    debug_assert!(ints_disabled());
    // On riscv64, Iframe and SyscallRegs are the same type.
    save_restricted_syscall_state(state, frame);
}

pub fn save_restricted_exception_state(state: &mut zx_restricted_state_t) {
    let mut regs = zx_thread_state_general_regs_t::default();
    // SAFETY: Gets general registers of the current thread.
    let status = unsafe { cpp_riscv64_get_general_regs(&mut regs) };
    // This will only fail if register state has not been saved, but this will always
    // have happened by this stage of exception handling.
    assert_eq!(status, Status::OK.into_raw());
    *state = regs;
}

pub fn redirect_restricted_exception_to_normal(
    _arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    reason: u64,
) {
    let regs = zx_thread_state_general_regs_t {
        pc: vector_table as u64,
        a0: context as u64,
        a1: reason,
        ..Default::default()
    };
    // SAFETY: Sets general registers of the current thread.
    let status = unsafe { cpp_riscv64_set_general_regs(&regs) };
    // This will only fail if register state has not been saved, but this will always
    // have happened by this stage of exception handling.
    assert_eq!(status, Status::OK.into_raw());
}

pub fn enter_full(
    _arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    code: u64,
) -> ! {
    debug_assert!(ints_disabled());
    // Set status to a valid initial value. Keep FP and V status since that register state should be
    // preserved when entering/exiting restricted mode.
    // SAFETY: Reads FP/V status from SSTATUS CSR for current CPU.
    let fp_v_status = unsafe { cpp_riscv64_get_sstatus_fp_v() };
    let iframe = Iframe {
        status: RISCV64_CSR_SSTATUS_SPIE | RISCV64_CSR_SSTATUS_UXL_64BIT | fp_v_status,
        regs: zx_restricted_state_t {
            pc: vector_table as u64,
            a0: context as u64,
            a1: code,
            ..Default::default()
        },
    };

    // Enter normal mode.
    // SAFETY: Enters user space in normal mode using constructed iframe. Does not return.
    unsafe { cpp_riscv64_enter_uspace(&iframe) };
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_validate_state_pre_restricted_entry(
    state: *const zx_restricted_state_t,
) -> zx_status_t {
    // SAFETY: Caller guarantees `state` is a valid pointer.
    let state = unsafe { &*state };
    match validate_state_pre_restricted_entry(state) {
        Ok(()) => Status::OK.into_raw(),
        Err(s) => s.into_raw(),
    }
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_save_state_pre_restricted_entry(
    state: *mut ArchSavedNormalState,
) {
    // SAFETY: Caller guarantees `state` is valid.
    let state = unsafe { &mut *state };
    save_state_pre_restricted_entry(state);
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_enter_restricted(state: *const zx_restricted_state_t) -> ! {
    // SAFETY: Caller guarantees `state` is valid.
    let state = unsafe { &*state };
    enter_restricted(state);
}

/// # Safety
/// Caller guarantees `state` and `regs` are valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_save_restricted_syscall_state(
    state: *mut zx_restricted_state_t,
    regs: *const SyscallRegs,
) {
    // SAFETY: Caller guarantees pointers are valid.
    let state = unsafe { &mut *state };
    let regs = unsafe { &*regs };
    save_restricted_syscall_state(state, regs);
}

/// # Safety
/// Caller guarantees `state` and `frame` are valid pointers.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_save_restricted_iframe_state(
    state: *mut zx_restricted_state_t,
    frame: *const Iframe,
) {
    // SAFETY: Caller guarantees pointers are valid.
    let state = unsafe { &mut *state };
    let frame = unsafe { &*frame };
    save_restricted_iframe_state(state, frame);
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_save_restricted_exception_state(
    state: *mut zx_restricted_state_t,
) {
    // SAFETY: Caller guarantees pointer is valid.
    let state = unsafe { &mut *state };
    save_restricted_exception_state(state);
}

/// # Safety
/// Caller guarantees `arch_state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_redirect_restricted_exception_to_normal(
    arch_state: *const ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    reason: u64,
) {
    // SAFETY: Caller guarantees pointer is valid.
    let arch_state = unsafe { &*arch_state };
    redirect_restricted_exception_to_normal(arch_state, vector_table, context, reason);
}

/// # Safety
/// Caller guarantees `arch_state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_enter_full(
    arch_state: *const ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    code: u64,
) -> ! {
    // SAFETY: Caller guarantees pointer is valid.
    let arch_state = unsafe { &*arch_state };
    enter_full(arch_state, vector_table, context, code);
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_dump(state: *const zx_restricted_state_t) {
    // SAFETY: Caller guarantees `state` is a valid pointer.
    let state = unsafe { &*state };
    dump(state);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_user_accessible() {
        assert!(is_user_accessible(0x00000000_00100000));
        assert!(is_user_accessible(0x0000003f_ffffffff));
        assert!(!is_user_accessible(0x00000040_00000000));
        assert!(!is_user_accessible(0xffffffff_80000000));
    }

    #[test]
    fn test_validate_state_pre_restricted_entry() {
        let mut state = zx_restricted_state_t::default();
        state.pc = 0x1000;
        assert_eq!(validate_state_pre_restricted_entry(&state), Ok(()));

        state.pc = 0xffff_ffff_8000_0000;
        assert_eq!(validate_state_pre_restricted_entry(&state), Err(Status::BAD_STATE));
    }
}
