// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Restricted execution mode state management, transitions, and register handling for x86_64.

use debug::ltracef;
use kprint::kprint;
use zx_status::Status;
use zx_types::{zx_restricted_state_t, zx_thread_state_general_regs_t};

use super::registers::{
    X86_FLAGS_IF, X86_FLAGS_USER, X86_MSR_IA32_FS_BASE, X86_MSR_IA32_KERNEL_GS_BASE,
};
use super::restricted_ffi::{arch_enter_uspace, arch_get_general_regs, arch_set_general_regs};
use super::x86::{read_msr, write_msr};
use super::{Iframe, SyscallRegs, USER_CODE_64_SELECTOR, USER_DATA_SELECTOR, feature, vm};
use crate::kernel::thread::current_get;

const LOCAL_TRACE: u32 = 0;

/// Architecture-specific saved normal mode state for x86_64.
///
/// Saves the normal mode `fs_base` and `gs_base` MSR values across restricted mode entry.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchSavedNormalState {
    pub(super) normal_fs_base: u64,
    pub(super) normal_gs_base: u64,
}

zr::static_assert!(core::mem::size_of::<ArchSavedNormalState>() == 16);
zr::static_assert!(core::mem::align_of::<ArchSavedNormalState>() == 8);

/// Helper routines to read/write user fs and gs base registers using the optimal
/// mechanism. Must be called with interrupts disabled around the swapgs sequence.
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled (`ints_disabled()`) so an interrupt cannot
/// fire while `%gs.base` is swapped to user `gs_base`, and that `CR4.FSGSBASE` is enabled when
/// `X86_FEATURE_FSGSBASE` is supported.
#[inline]
unsafe fn get_fsgsbase() -> (u64, u64) {
    debug_assert!(crate::arch_rs::ints_disabled());

    // read the fs/gs base out of the MSRs
    if feature::x86_feature_fsgsbase() {
        let fsbase: u64;
        let gsbase: u64;
        // the user and kernel base have been swapped, use swapgs to temporarily
        // gain access to the gs register.
        // SAFETY: Interrupts are disabled and `CR4.FSGSBASE` is enabled when
        // `X86_FEATURE_FSGSBASE` is supported. The user and kernel `gs` base have been swapped, so
        // `swapgs` temporarily swaps user `gs` base into `%gs.base`, reads it with `rdgsbase`, and
        // immediately swaps kernel `%gs.base` back within a single atomic inline assembly block.
        unsafe {
            core::arch::asm!(
                "rdfsbase {fs}",
                "swapgs",
                "rdgsbase {gs}",
                "swapgs",
                fs = out(reg) fsbase,
                gs = out(reg) gsbase,
                options(nostack, preserves_flags),
            );
        }
        (fsbase, gsbase)
    } else {
        core::hint::cold_path();
        // SAFETY: `IA32_FS_BASE` is an architectural x86-64 MSR.
        let fsbase = unsafe { read_msr(X86_MSR_IA32_FS_BASE) };
        // SAFETY: `IA32_KERNEL_GS_BASE` is an architectural x86-64 MSR.
        let gsbase = unsafe { read_msr(X86_MSR_IA32_KERNEL_GS_BASE) };
        (fsbase, gsbase)
    }
}

/// Helper routine to write user `fs` and `gs` base registers using the optimal mechanism.
/// Must be called with interrupts disabled around the `swapgs` sequence.
///
/// # Safety
///
/// The caller must ensure that interrupts are disabled (`ints_disabled()`), that `fsbase` and
/// `gsbase` are canonical virtual addresses, and that `CR4.FSGSBASE` is enabled when
/// `X86_FEATURE_FSGSBASE` is supported.
#[inline]
unsafe fn set_fsgsbase(fsbase: u64, gsbase: u64) {
    debug_assert!(crate::arch_rs::ints_disabled());
    debug_assert!(vm::is_vaddr_canonical(fsbase as usize));
    debug_assert!(vm::is_vaddr_canonical(gsbase as usize));

    if feature::x86_feature_fsgsbase() {
        // the user and kernel base have been swapped, use swapgs to temporarily
        // gain access to the gs register.
        // SAFETY: Caller guarantees interrupts are disabled, `CR4.FSGSBASE` is enabled when
        // `X86_FEATURE_FSGSBASE` is supported, and `fsbase` and `gsbase` are canonical virtual
        // addresses. The user and kernel `gs` base have been swapped, so `swapgs` temporarily
        // swaps user `gs` base into `%gs.base`, writes `gsbase` with `wrgsbase`, and immediately
        // swaps kernel `%gs.base` back within a single atomic inline assembly block.
        unsafe {
            core::arch::asm!(
                "wrfsbase {fs}",
                "swapgs",
                "wrgsbase {gs}",
                "swapgs",
                fs = in(reg) fsbase,
                gs = in(reg) gsbase,
                options(nostack, preserves_flags),
            );
        }
    } else {
        core::hint::cold_path();
        // SAFETY: Caller guarantees `fsbase` and `gsbase` are canonical addresses for
        // `IA32_FS_BASE` and `IA32_KERNEL_GS_BASE`.
        unsafe {
            write_msr(X86_MSR_IA32_FS_BASE, fsbase);
            write_msr(X86_MSR_IA32_KERNEL_GS_BASE, gsbase);
        }
    }
}

/// Validates the x86_64 register state before entering restricted mode.
pub fn validate_state_pre_restricted_entry(state: &zx_restricted_state_t) -> Result<(), Status> {
    // validate that RIP is within user space
    if !vm::is_user_accessible(state.ip as usize) {
        core::hint::cold_path();
        ltracef!("fail due to bad ip {:#x}\n", state.ip);
        return Err(Status::BAD_STATE);
    }

    // validate that the rflags saved only contain user settable flags
    if (state.flags & !X86_FLAGS_USER) != 0 {
        core::hint::cold_path();
        ltracef!("fail due to flags outside of X86_FLAGS_USER set ({:#x})\n", state.flags);
        return Err(Status::BAD_STATE);
    }

    // fs and gs base must be canonical
    if !vm::is_vaddr_canonical(state.fs_base as usize) {
        core::hint::cold_path();
        ltracef!("fail due to bad fs base {:#x}\n", state.fs_base);
        return Err(Status::BAD_STATE);
    }
    if !vm::is_vaddr_canonical(state.gs_base as usize) {
        core::hint::cold_path();
        ltracef!("fail due to bad gs base {:#x}\n", state.gs_base);
        return Err(Status::BAD_STATE);
    }

    // everything else can be whatever value it wants to be. worst case it immediately faults
    // in restricted mode and that's okay.
    Ok(())
}

/// Dumps the restricted state registers for debugging.
#[sanitize(address = "off")]
pub fn dump(state: &zx_restricted_state_t) {
    kprint!(
        concat!(
            " RIP: {:#18x}  FL: {:#18x}\n",
            " RAX: {:#18x} RBX: {:#18x} RCX: {:#18x} RDX: {:#18x}\n",
            " RSI: {:#18x} RDI: {:#18x} RBP: {:#18x} RSP: {:#18x}\n",
            "  R8: {:#18x}  R9: {:#18x} R10: {:#18x} R11: {:#18x}\n",
            " R12: {:#18x} R13: {:#18x} R14: {:#18x} R15: {:#18x}\n",
            "fs base {:#18x} gs base {:#18x}\n",
        ),
        state.ip,
        state.flags,
        state.rax,
        state.rbx,
        state.rcx,
        state.rdx,
        state.rsi,
        state.rdi,
        state.rbp,
        state.rsp,
        state.r8,
        state.r9,
        state.r10,
        state.r11,
        state.r12,
        state.r13,
        state.r14,
        state.r15,
        state.fs_base,
        state.gs_base,
    );
}

/// Saves normal-mode `fs_base` and `gs_base` before entering restricted mode.
pub fn save_state_pre_restricted_entry(state: &mut ArchSavedNormalState) {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: `save_state_pre_restricted_entry` is called with interrupts disabled prior to
    // entering restricted mode, and `CR4.FSGSBASE` is enabled on CPUs with `X86_FEATURE_FSGSBASE`.
    (state.normal_fs_base, state.normal_gs_base) = unsafe { get_fsgsbase() };
}

/// Enters restricted mode with the provided state. Does not return.
pub fn enter_restricted(state: &zx_restricted_state_t) -> ! {
    debug_assert!(crate::arch_rs::ints_disabled());
    // SAFETY: `state` was validated by `validate_state_pre_restricted_entry`, so `fs_base` and
    // `gs_base` are canonical and interrupts are disabled on entry to `enter_restricted`.
    unsafe {
        set_fsgsbase(state.fs_base, state.gs_base);
    }

    let iframe = Iframe {
        rdi: state.rdi,
        rsi: state.rsi,
        rbp: state.rbp,
        rbx: state.rbx,
        rdx: state.rdx,
        rcx: state.rcx,
        rax: state.rax,
        r8: state.r8,
        r9: state.r9,
        r10: state.r10,
        r11: state.r11,
        r12: state.r12,
        r13: state.r13,
        r14: state.r14,
        r15: state.r15,
        vector: 0,
        err_code: 0,
        ip: state.ip,
        cs: USER_CODE_64_SELECTOR,
        flags: state.flags | X86_FLAGS_IF,
        user_sp: state.rsp,
        user_ss: USER_DATA_SELECTOR,
    };

    // SAFETY: Enters user space using the constructed iframe with interrupts disabled. Does not
    // return.
    unsafe { arch_enter_uspace(&iframe) };
}

/// Saves restricted mode register state upon a syscall from restricted mode.
pub fn save_restricted_syscall_state(state: &mut zx_restricted_state_t, regs: &SyscallRegs) {
    state.rdi = regs.rdi;
    state.rsi = regs.rsi;
    state.rbp = regs.rbp;
    state.rbx = regs.rbx;
    state.rdx = regs.rdx;
    state.rcx = regs.rcx;
    state.rax = regs.rax;
    state.rsp = regs.rsp;
    state.r8 = regs.r8;
    state.r9 = regs.r9;
    state.r10 = regs.r10;
    state.r11 = regs.r11;
    state.r12 = regs.r12;
    state.r13 = regs.r13;
    state.r14 = regs.r14;
    state.r15 = regs.r15;
    state.ip = regs.rip;
    state.flags = regs.rflags & X86_FLAGS_USER;

    // SAFETY: `save_restricted_syscall_state` is called from syscall entry with interrupts
    // disabled, and `CR4.FSGSBASE` is enabled on CPUs with `X86_FEATURE_FSGSBASE`.
    (state.fs_base, state.gs_base) = unsafe { get_fsgsbase() };
}

/// Saves restricted mode register state from an interrupt/exception frame.
pub fn save_restricted_iframe_state(state: &mut zx_restricted_state_t, frame: &Iframe) {
    state.rdi = frame.rdi;
    state.rsi = frame.rsi;
    state.rbp = frame.rbp;
    state.rbx = frame.rbx;
    state.rdx = frame.rdx;
    state.rcx = frame.rcx;
    state.rax = frame.rax;
    state.r8 = frame.r8;
    state.r9 = frame.r9;
    state.r10 = frame.r10;
    state.r11 = frame.r11;
    state.r12 = frame.r12;
    state.r13 = frame.r13;
    state.r14 = frame.r14;
    state.r15 = frame.r15;
    state.ip = frame.ip;
    state.flags = frame.flags & X86_FLAGS_USER;
    state.rsp = frame.user_sp;

    // SAFETY: `save_restricted_iframe_state` is called from interrupt/exception entry with
    // interrupts disabled, and `CR4.FSGSBASE` is enabled on CPUs with `X86_FEATURE_FSGSBASE`.
    (state.fs_base, state.gs_base) = unsafe { get_fsgsbase() };
}

/// Saves restricted mode register state during exception handling.
pub fn save_restricted_exception_state(state: &mut zx_restricted_state_t) {
    let mut regs = zx_thread_state_general_regs_t::default();
    // SAFETY: Reads general registers of the current thread into `regs`, which is handling an
    // exception with saved user state.
    let status = unsafe { arch_get_general_regs(current_get(), &mut regs) };
    assert_eq!(Status::ok(status), Ok(()));

    state.rdi = regs.rdi;
    state.rsi = regs.rsi;
    state.rbp = regs.rbp;
    state.rbx = regs.rbx;
    state.rdx = regs.rdx;
    state.rcx = regs.rcx;
    state.rax = regs.rax;
    state.rsp = regs.rsp;
    state.r8 = regs.r8;
    state.r9 = regs.r9;
    state.r10 = regs.r10;
    state.r11 = regs.r11;
    state.r12 = regs.r12;
    state.r13 = regs.r13;
    state.r14 = regs.r14;
    state.r15 = regs.r15;
    state.ip = regs.rip;
    state.flags = regs.rflags & X86_FLAGS_USER;

    debug_assert!(!crate::arch_rs::ints_disabled());
    crate::arch_rs::disable_ints();
    // SAFETY: Interrupts are explicitly disabled above around `get_fsgsbase`, and `CR4.FSGSBASE`
    // is enabled on CPUs with `X86_FEATURE_FSGSBASE`.
    (state.fs_base, state.gs_base) = unsafe { get_fsgsbase() };
    crate::arch_rs::enable_ints();
}

/// Redirects an exception in restricted mode back to normal mode's `vector_table`.
pub fn redirect_restricted_exception_to_normal(
    arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    reason: u64,
) {
    let regs = zx_thread_state_general_regs_t {
        rax: 0,
        rbx: 0,
        rcx: 0,
        rdx: 0,
        rsi: reason,
        rdi: context as u64,
        rbp: 0,
        rsp: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        rip: vector_table as u64,
        rflags: X86_FLAGS_IF,
        fs_base: arch_state.normal_fs_base,
        gs_base: arch_state.normal_gs_base,
    };
    // SAFETY: Sets general registers on the current thread during exception handling.
    let status = unsafe { arch_set_general_regs(current_get(), &regs) };
    debug_assert_eq!(Status::ok(status), Ok(()));
}

/// Returns to normal mode (`enter_full`) at `vector_table` with `(context, code)`. Does not return.
pub fn enter_full(
    arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    code: u64,
) -> ! {
    // SAFETY: Restores canonical normal-mode user `fs_base` and `gs_base` with interrupts
    // disabled.
    unsafe {
        set_fsgsbase(arch_state.normal_fs_base, arch_state.normal_gs_base);
    }

    let iframe = Iframe {
        rdi: context as u64,
        rsi: code,
        rbp: 0,
        rbx: 0,
        rdx: 0,
        rcx: 0,
        rax: 0,
        r8: 0,
        r9: 0,
        r10: 0,
        r11: 0,
        r12: 0,
        r13: 0,
        r14: 0,
        r15: 0,
        vector: 0,
        err_code: 0,
        ip: vector_table as u64,
        cs: USER_CODE_64_SELECTOR,
        flags: X86_FLAGS_IF,
        user_sp: 0,
        user_ss: USER_DATA_SELECTOR,
    };

    // SAFETY: Enters user space using constructed iframe with interrupts disabled. Does not
    // return.
    unsafe { arch_enter_uspace(&iframe) };
}
