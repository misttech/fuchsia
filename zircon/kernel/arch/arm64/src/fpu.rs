// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015 Google Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! arm64 FPU state save and restore for user threads.

use core::mem::offset_of;

use arm64_thread_bindings::{
    FPSTATE_FFR_OFFSET, FPSTATE_FPCR_OFFSET, FPSTATE_FPSR_OFFSET, FPSTATE_P_REGS_OFFSET,
    FPSTATE_REGS_OFFSET,
};
use debug::ltracef;
use zx_types::ZX_ARM64_FEATURE_ISA_SVE;

use super::arm64_feature_test;
use super::mp::arch_curr_cpu_num;
use super::thread::{thread_arch, thread_arch_const};
use crate::kernel::thread::{Thread, is_user_thread};

const LOCAL_TRACE: u32 = 0;

/// Saved FPU state (`struct fpstate`).
type Fpstate = arm64_thread_bindings::fpstate;

// fpu_asm.S reaches the fields at these offsets.
zr::static_assert!(offset_of!(Fpstate, regs) == FPSTATE_REGS_OFFSET as usize);
zr::static_assert!(offset_of!(Fpstate, fpcr) == FPSTATE_FPCR_OFFSET as usize);
zr::static_assert!(offset_of!(Fpstate, fpsr) == FPSTATE_FPSR_OFFSET as usize);
zr::static_assert!(offset_of!(Fpstate, p_regs) == FPSTATE_P_REGS_OFFSET as usize);
zr::static_assert!(offset_of!(Fpstate, ffr) == FPSTATE_FFR_OFFSET as usize);
// The asm moves q0-q31 into `regs` and p0-p15 into `p_regs`, and each array ends where the next
// field begins.
zr::static_assert!(offset_of!(Fpstate, fpcr) - offset_of!(Fpstate, regs) == 16 * 32);
zr::static_assert!(offset_of!(Fpstate, ffr) - offset_of!(Fpstate, p_regs) == 16 * 2);

unsafe extern "C" {
    /// Loads q0-q31 from `fpstate.regs`; if `sve`, FFR from `fpstate.ffr` and p0-p15 from
    /// `fpstate.p_regs`; then FPCR from `fpstate.fpcr` and FPSR from `fpstate.fpsr`.
    fn arm64_fpu_restore_fpstate(fpstate: *const Fpstate, sve: bool);

    /// Stores q0-q31 to `fpstate.regs`; if `sve`, p0-p15 to `fpstate.p_regs` and FFR to
    /// `fpstate.ffr`; then FPCR to `fpstate.fpcr` and FPSR to `fpstate.fpsr`.
    fn arm64_fpu_save_fpstate(fpstate: *mut Fpstate, sve: bool);
}

/// Loads `t`'s saved FPU state into the FP / vector registers, the SVE predicate registers and
/// FFR if the CPU has SVE, FPCR and FPSR.
///
/// # Safety
///
/// `t` points to a live `Thread` whose lock the caller holds.
pub(super) unsafe fn arm64_fpu_restore_state(t: *const Thread) {
    // SAFETY: `t` points to a live `Thread`, per the caller's contract.
    let arch = unsafe { thread_arch_const(t) };
    // SAFETY: `arch` is the thread's live `arch_thread`, and taking the field's address reads
    // nothing.
    let fpstate = unsafe { &raw const (*arch).fpstate };

    ltracef!(
        "cpu {}, thread {}, load fpstate {:p}\n",
        arch_curr_cpu_num(),
        // SAFETY: `t` points to a live `Thread`, per the caller's contract.
        unsafe { Thread::name_copy_ptr(t) },
        fpstate
    );

    // SAFETY: `fpstate` is `t`'s saved FPU state. Once `t` is constructed, every access to it holds
    // `t`'s lock, as the caller does. The load replaces only registers in which kernel code keeps
    // no values.
    unsafe { arm64_fpu_restore_fpstate(fpstate, arm64_feature_test(ZX_ARM64_FEATURE_ISA_SVE)) };
}

/// Saves the FP / vector registers, the SVE predicate registers and FFR if the CPU has SVE, FPCR
/// and FPSR to `t`'s FPU state.
///
/// # Safety
///
/// `t` points to a live `Thread` whose lock the caller holds.
pub(super) unsafe fn arm64_fpu_save_state(t: *mut Thread) {
    // SAFETY: `t` points to a live `Thread`, per the caller's contract.
    let arch = unsafe { thread_arch(t) };
    // SAFETY: `arch` is the thread's live `arch_thread`, and taking the field's address reads
    // nothing.
    let fpstate = unsafe { &raw mut (*arch).fpstate };

    ltracef!(
        "cpu {}, thread {}, save fpstate {:p}\n",
        arch_curr_cpu_num(),
        // SAFETY: `t` points to a live `Thread`, per the caller's contract.
        unsafe { Thread::name_copy_ptr(t) },
        fpstate
    );

    // SAFETY: `fpstate` is `t`'s saved FPU state. Once `t` is constructed, every access to it holds
    // `t`'s lock, as the caller does.
    unsafe { arm64_fpu_save_fpstate(fpstate, arm64_feature_test(ZX_ARM64_FEATURE_ISA_SVE)) };

    ltracef!(
        "thread {}, fpcr {:x}, fpsr {:x}\n",
        // SAFETY: `t` points to a live `Thread`, per the caller's contract.
        unsafe { Thread::name_copy_ptr(t) },
        // SAFETY: The caller holds `t`'s lock, so nothing writes `fpstate` meanwhile.
        unsafe { (*fpstate).fpcr },
        // SAFETY: As above.
        unsafe { (*fpstate).fpsr }
    );
}

/// Saves `oldthread`'s FPU state if it is a user thread, and loads `newthread`'s if it is one.
///
/// # Safety
///
/// `oldthread` and `newthread` point to live `Thread`s whose locks the caller holds.
pub(super) unsafe fn arm64_fpu_context_switch(oldthread: *mut Thread, newthread: *mut Thread) {
    // The kernel itself does not use the FPU outside of managing state for user space threads.
    // Thus the only threads that can have relevant FPU state to save or restore are those that
    // have user space threads. This code only saves the FPU state when switching away from a user
    // space thread and only loads the FPU register state when switching to a user space thread.
    // Note: When running a kernel-only thread, FPU state from the most recent
    // user space thread may be resident in the hardware registers.
    // SAFETY: `oldthread` points to a live `Thread`, per the caller's contract.
    if unsafe { is_user_thread(oldthread) } {
        // SAFETY: `oldthread` points to a live `Thread` whose lock the caller holds.
        unsafe { arm64_fpu_save_state(oldthread) };
    }
    // SAFETY: `newthread` points to a live `Thread`, per the caller's contract.
    if unsafe { is_user_thread(newthread) } {
        // SAFETY: `newthread` points to a live `Thread` whose lock the caller holds.
        unsafe { arm64_fpu_restore_state(newthread) };
    }
}
