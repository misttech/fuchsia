// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// Architecture-specific saved normal mode state for aarch64.
///
/// Saves the normal mode `tpidr_el0` and `tpidrro_el0` system registers across restricted entry.
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ArchSavedNormalState {
    pub tpidr_el0: u64,
    pub tpidrro_el0: u64,
}

const _: () = {
    assert!(core::mem::size_of::<ArchSavedNormalState>() == 16);
    assert!(core::mem::align_of::<ArchSavedNormalState>() == 8);
};

use core::fmt::Write;
use debug::ltrace::KernelConsoleWriter;
use debug::ltracef;
use zx_status::Status;
#[allow(unused_imports)]
use zx_types::{zx_restricted_state_t, zx_status_t, zx_thread_state_general_regs_t};

const LOCAL_TRACE: u32 = 0;

unsafe extern "C" {
    static arm64_isa_features: u32;
    fn cpp_arm64_ints_disabled() -> bool;
    fn cpp_arm64_get_tpidr_regs(tpidr_el0: *mut u64, tpidrro_el0: *mut u64);
    fn cpp_arm64_set_tpidr_regs(tpidr_el0: u64, tpidrro_el0: u64);
    fn cpp_arm64_enter_restricted_tpidr(tpidr_el0: u64, is_arm32: bool);
    fn cpp_arm64_get_tpidr_el0() -> u64;
    fn cpp_arm64_enter_uspace(iframe: *const Iframe) -> !;
    fn cpp_arm64_get_general_regs(regs: *mut zx_thread_state_general_regs_t) -> zx_status_t;
    fn cpp_arm64_set_general_regs(regs: *const zx_thread_state_general_regs_t) -> zx_status_t;
}

// [arm/v8]: C5.2.19 CPSR / D13.2.112 SPSR_EL1
pub const ARM_NCZV_FLAGS: u32 = 0xf000_0000;
// [arm/v8]: FEAT_BTI (Branch Target Identification) branch type flags.
pub const ARM64_BTYPE_FLAGS: u32 = 0x0000_0c00;
// [arm/v8]: C5.2.19 CPSR M[4] / nRW bit indicating AArch32 execution state.
pub const ARM32_BIT_MODE: u32 = 1 << 4; // 0x10
// [arm/v8]: C5.2.19 CPSR T bit indicating AArch32 Thumb execution state.
pub const ARM32_BIT_THUMB_MODE: u32 = 1 << 5; // 0x20
// [arm/v8]: C5.2.19 CPSR GE[19:16] and Q[27] bits.
pub const ARM32_GE_Q_BITS: u32 = 0x080f_0000;
// [arm/v8]: C5.2.19 CPSR IT[15:10, 26:25] If-Then execution state bits.
pub const ARM32_IT_BITS: u32 = 0x0600_fc00;

pub const ARM64_USER_RESTRICTED_VISIBLE_FLAGS: u32 = ARM_NCZV_FLAGS | ARM64_BTYPE_FLAGS; // 0xf000_0c00
pub const ARM32_USER_RESTRICTED_VISIBLE_FLAGS: u32 =
    ARM_NCZV_FLAGS | ARM32_BIT_MODE | ARM32_BIT_THUMB_MODE | ARM32_GE_Q_BITS | ARM32_IT_BITS; // 0xfe0f_fc30
pub const ARM32_BIT_MODE_REGISTER_COUNT: usize = 15;
pub const ZX_ARM64_FEATURE_ISA_ARM32: u32 = 1 << 5;

#[inline]
fn arm64_feature_test(feature: u32) -> bool {
    // SAFETY: Reads global read-only hardware feature mask initialized at boot.
    unsafe { (arm64_isa_features & feature) != 0 }
}

/// Architectural interrupt frame for aarch64 user space entry/exit.
///
/// Layout must exactly match C++ `iframe_t` (`zircon/kernel/arch/arm64/include/arch/regs.h`).
#[repr(C)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Iframe {
    pub r: [u64; 30], // x0-x29
    pub elr: u64,     // ELR_EL1
    pub spsr: u64,    // SPSR_EL1
    pub lr: u64,      // x30
    pub usp: u64,     // SP_EL0
}

pub type SyscallRegs = Iframe;

const _: () = {
    assert!(core::mem::size_of::<Iframe>() == 272);
    assert!(core::mem::align_of::<Iframe>() == 8);
    assert!(core::mem::offset_of!(Iframe, r) == 0);
    assert!(core::mem::offset_of!(Iframe, elr) == 240);
    assert!(core::mem::offset_of!(Iframe, spsr) == 248);
    assert!(core::mem::offset_of!(Iframe, lr) == 256);
    assert!(core::mem::offset_of!(Iframe, usp) == 264);
};

/// Check if a virtual address is accessible to user space on aarch64.
///
/// [arm/v8]: D5.2.6 Virtual address splits / TTBR0_EL1 selection bit (VA[55] == 0).
#[inline]
pub fn is_user_accessible(va: usize) -> bool {
    const HIGH_VA_BIT: usize = 55;
    const USER_BIT_MASK: usize = 1usize << HIGH_VA_BIT;
    (va & USER_BIT_MASK) == 0
}

/// Validate that the restricted state is safe and well-formed before entering restricted mode.
///
/// Ensures that the program counter (`pc`) is within user address space, verifies alignment
/// constraints for both AArch64 and AArch32 mode, and checks that only user-settable flags are present in `cpsr`.
///
/// [arm/v8]: C5.2.19 CPSR / D13.2.112 SPSR_EL1
pub fn validate_state_pre_restricted_entry(state: &zx_restricted_state_t) -> Result<(), Status> {
    // Validate that PC is within userspace.
    if !is_user_accessible(state.pc as usize) {
        ltracef!("fail due to bad PC {:#x}\n", state.pc);
        return Err(Status::BAD_STATE);
    }
    // If ARM32_BIT_MODE, perform additional checks.
    if (state.cpsr & ARM32_BIT_MODE) != 0 {
        if !arm64_feature_test(ZX_ARM64_FEATURE_ISA_ARM32) {
            ltracef!("fail due to lack of 32-bit ISA support\n");
            return Err(Status::BAD_STATE);
        }
        // Make sure PC is < 4GB in ARM32_BIT_MODE.
        if state.pc >= (1u64 << 32) {
            ltracef!("fail due to out of range 32-bit PC {:#x}\n", state.pc);
            return Err(Status::BAD_STATE);
        }
        // If CPSR's T bit is not set, then PC[1:0] must be 0 (32-bit aligned).
        if (state.cpsr & ARM32_BIT_THUMB_MODE) == 0 && (state.pc & 0x3) != 0 {
            ltracef!("fail due to unaligned A32 32-bit PC {:#x}\n", state.pc);
            return Err(Status::BAD_STATE);
        }
        // Note: Callers entering 32-bit restricted mode in Thumb state may provide an
        // entry PC with the low bit clear or set due to the interworking convention.
        // We do not reject state when PC[0] is set in Thumb mode.
        // Validate that only the NCZV flags and 32-bit relevant flags of the CPSR are set.
        if (state.cpsr & !ARM32_USER_RESTRICTED_VISIBLE_FLAGS) != 0 {
            ltracef!(
                "fail due to flags outside of ARM32_USER_RESTRICTED_VISIBLE_FLAGS set ({:#x})\n",
                state.cpsr
            );
            return Err(Status::BAD_STATE);
        }
    } else {
        // Validate that only the NCZV and BTYPE flags of the CPSR are set.
        // For aarch64 restricted threads, the flags are the same as normal user threads.
        if (state.cpsr & !ARM64_USER_RESTRICTED_VISIBLE_FLAGS) != 0 {
            ltracef!(
                "fail due to flags outside of ARM64_USER_RESTRICTED_VISIBLE_FLAGS set ({:#x})\n",
                state.cpsr
            );
            return Err(Status::BAD_STATE);
        }
    }
    Ok(())
}

/// Dump architectural restricted state register contents to the kernel console.
pub fn dump(state: &zx_restricted_state_t) {
    let mut w = KernelConsoleWriter;
    let _ = write!(w, "CPSR: {:#18x}\n  PC: {:#18x}\n", state.cpsr, state.pc);
    for i in 0..31 {
        let _ = writeln!(w, " X{:02}: {:#18x}", i, state.r[i]);
    }
    let _ = write!(w, "  SP: {:#18x}\nTPIDR_EL0: {:#18x}\n", state.sp, state.tpidr_el0);
}

/// Save normal mode architectural registers before transitioning to restricted mode.
///
/// Saves the current thread's `TPIDR_EL0` and `TPIDRRO_EL0` registers.
///
/// [arm/sysreg]: TPIDR_EL0 / TPIDRRO_EL0
pub fn save_state_pre_restricted_entry(state: &mut ArchSavedNormalState) {
    // Save the thread local storage register(s) from normal mode.
    // SAFETY: Reads TPIDR_EL0 and TPIDRRO_EL0 for current thread.
    unsafe {
        cpp_arm64_get_tpidr_regs(&mut state.tpidr_el0, &mut state.tpidrro_el0);
    }
}

/// Enter user space in restricted mode using the provided restricted architectural state.
///
/// Copies general registers, program counter, stack pointer, and `CPSR` into an interrupt frame,
/// loads restricted `TPIDR_EL0`, and transfers control to user space.
///
/// [arm/sysreg]: TPIDR_EL0 / TPIDRRO_EL0
pub fn enter_restricted(state: &zx_restricted_state_t) -> ! {
    debug_assert!(
        unsafe { cpp_arm64_ints_disabled() },
        "Interrupts must be disabled across restricted state transitions"
    );

    // Copy restricted state to an interrupt frame.
    let mut iframe = Iframe::default();
    let is_arm32 = (state.cpsr & ARM32_BIT_MODE) != 0;
    if is_arm32 {
        // If the thread is in a 32-bit execution mode, then ignore the upper bits of
        // the registers and only copy the registers that map to ARM32 state.r0-r14.
        for i in 0..ARM32_BIT_MODE_REGISTER_COUNT {
            iframe.r[i] = state.r[i] & 0x0000_0000_ffff_ffff;
        }
    } else {
        iframe.r[..30].copy_from_slice(&state.r[..30]);
        iframe.lr = state.r[30];
    }
    iframe.usp = state.sp;
    iframe.elr = state.pc;
    iframe.spsr = state.cpsr as u64;

    // Restore TPIDR_EL0 from restricted mode.
    // TODO(https://fxbug.dev/42076040): Eventually the TPIDR register should be
    // inside the iframe.
    // Mirror to tpidrro_el0 when supporting aarch32.
    // This allows aarch32 userland to read TPIDRURO which is needed
    // by some libc implementations.
    // Load the new state and enter restricted mode.
    // SAFETY: Sets TPIDR registers and enters user space in restricted mode. Does not return.
    unsafe {
        cpp_arm64_enter_restricted_tpidr(state.tpidr_el0, is_arm32);
        cpp_arm64_enter_uspace(&iframe);
    }
}

/// Save restricted mode architectural state upon returning from a syscall (`zx_restricted_enter`).
///
/// Copies general registers from `regs` into `state` and reads the current `TPIDR_EL0`.
///
/// [arm/sysreg]: TPIDR_EL0
pub fn save_restricted_syscall_state(state: &mut zx_restricted_state_t, regs: &SyscallRegs) {
    debug_assert!(
        unsafe { cpp_arm64_ints_disabled() },
        "Interrupts must be disabled across restricted state transitions"
    );

    // Save the registers from restricted mode.
    if (regs.spsr as u32 & ARM32_BIT_MODE) != 0 {
        // If the thread is in a 32-bit execution mode, then ignore the upper bits of
        // the registers and only copy the registers that map to to ARM32 state.r0-r14.
        for i in 0..ARM32_BIT_MODE_REGISTER_COUNT {
            state.r[i] = regs.r[i] & 0x0000_0000_ffff_ffff;
        }
    } else {
        state.r[..30].copy_from_slice(&regs.r[..30]);
        state.r[30] = regs.lr;
    }
    state.sp = regs.usp;
    state.pc = regs.elr;
    // Save only the non-reserved portions of the SPSR.
    state.cpsr = regs.spsr as u32;

    // Save the thread local storage location in restricted mode.
    // SAFETY: Reads TPIDR_EL0 register for current thread.
    unsafe {
        state.tpidr_el0 = cpp_arm64_get_tpidr_el0();
    }
}

/// Save restricted mode architectural state upon returning from an interrupt or exception via `Iframe`.
///
/// [arm/sysreg]: TPIDR_EL0
pub fn save_restricted_iframe_state(state: &mut zx_restricted_state_t, frame: &Iframe) {
    // On arm64, iframe_t and syscall_regs_t have identical register representations.
    save_restricted_syscall_state(state, frame);
}

/// Save restricted mode architectural state upon receiving an exception.
///
/// Reads general registers via `cpp_arm64_get_general_regs` and stores them in `state`.
///
/// [arm/sysreg]: TPIDR_EL0
pub fn save_restricted_exception_state(state: &mut zx_restricted_state_t) {
    let mut regs = zx_thread_state_general_regs_t::default();
    // SAFETY: cpp_arm64_get_general_regs retrieves saved register state for current thread.
    let status = unsafe { cpp_arm64_get_general_regs(&mut regs) };
    debug_assert_eq!(status, Status::OK.into_raw());

    // Save the registers from restricted mode.
    if (regs.cpsr as u32 & ARM32_BIT_MODE) != 0 {
        for i in 0..ARM32_BIT_MODE_REGISTER_COUNT {
            state.r[i] = regs.r[i] & 0x0000_0000_ffff_ffff;
        }
    } else {
        state.r[..30].copy_from_slice(&regs.r[..30]);
        state.r[30] = regs.lr;
    }
    state.sp = regs.sp;
    state.pc = regs.pc;
    state.cpsr = regs.cpsr as u32;

    // Save the thread local storage location in restricted mode.
    // SAFETY: Reads TPIDR_EL0 register for current thread.
    unsafe {
        state.tpidr_el0 = cpp_arm64_get_tpidr_el0();
    }
}

/// Redirect an exception in restricted mode back to normal mode.
///
/// Sets up the normal mode general registers with `context` and `reason` in `r[0]` and `r[1]`,
/// restores `TPIDR_EL0` and `TPIDRRO_EL0`, and points the program counter `pc` to `vector_table`.
///
/// [arm/sysreg]: TPIDR_EL0 / TPIDRRO_EL0
pub fn redirect_restricted_exception_to_normal(
    arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    reason: u64,
) {
    let mut regs = zx_thread_state_general_regs_t::default();
    // Pass through the context and exception reason as arguments.
    regs.r[0] = context as u64;
    regs.r[1] = reason;

    // Set the PC such that we return to the vector_table after entering normal mode.
    regs.pc = vector_table as u64;

    // SAFETY: Overwrites current thread general registers to jump to vector_table in normal mode.
    let status = unsafe { cpp_arm64_set_general_regs(&regs) };
    debug_assert_eq!(status, Status::OK.into_raw());

    // Restore TPIDR_EL0 and TPIDRRO_EL0 registers from saved normal state and update thread state.
    unsafe {
        cpp_arm64_set_tpidr_regs(arch_state.tpidr_el0, arch_state.tpidrro_el0);
    }
}

/// Enter user space in normal mode, restoring saved normal `TPIDR_EL0` and `TPIDRRO_EL0` registers
/// and transferring control to `vector_table` with `context` and `code` in `x0` and `x1`.
///
/// [arm/sysreg]: TPIDR_EL0 / TPIDRRO_EL0
pub fn enter_full(
    arch_state: &ArchSavedNormalState,
    vector_table: usize,
    context: usize,
    code: u64,
) -> ! {
    debug_assert!(
        unsafe { cpp_arm64_ints_disabled() },
        "Interrupts must be disabled across restricted state transitions"
    );

    // Set up a mostly empty iframe and return back to normal mode.
    let mut iframe = Iframe::default();
    // Pass through the context and return code as arguments.
    iframe.r[0] = context as u64;
    iframe.r[1] = code;
    // Set the ELR such that we return to the vector_table after entering normal
    // mode.
    iframe.elr = vector_table as u64;
    // Load the new state and exit.
    // SAFETY: Restores normal TPIDR registers and enters user space. Does not return.
    unsafe {
        // Restore TPIDR_EL0 from saved normal state.
        // TODO(https://fxbug.dev/42076040): Eventually the TPIDR register should be
        // inside the iframe.
        cpp_arm64_set_tpidr_regs(arch_state.tpidr_el0, arch_state.tpidrro_el0);
        cpp_arm64_enter_uspace(&iframe);
    }
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_dump(state: *const zx_restricted_state_t) {
    debug_assert!(!state.is_null(), "state pointer passed across FFI must not be null");
    // SAFETY: Caller guarantees `state` is a valid pointer.
    let state = unsafe { &*state };
    dump(state);
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_validate_state_pre_restricted_entry(
    state: *const zx_restricted_state_t,
) -> zx_status_t {
    debug_assert!(!state.is_null(), "state pointer passed across FFI must not be null");
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
    debug_assert!(!state.is_null(), "state pointer passed across FFI must not be null");
    // SAFETY: Caller guarantees `state` is valid.
    let state = unsafe { &mut *state };
    save_state_pre_restricted_entry(state);
}

/// # Safety
/// Caller guarantees `state` is a valid pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_arch_enter_restricted(state: *const zx_restricted_state_t) -> ! {
    debug_assert!(!state.is_null(), "state pointer passed across FFI must not be null");
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
    debug_assert!(!state.is_null(), "state pointer passed across FFI must not be null");
    debug_assert!(!regs.is_null(), "regs pointer passed across FFI must not be null");
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
    debug_assert!(!state.is_null(), "state pointer passed across FFI must not be null");
    debug_assert!(!frame.is_null(), "frame pointer passed across FFI must not be null");
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
    debug_assert!(!state.is_null(), "state pointer passed across FFI must not be null");
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
    debug_assert!(!arch_state.is_null(), "arch_state pointer passed across FFI must not be null");
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
    debug_assert!(!arch_state.is_null(), "arch_state pointer passed across FFI must not be null");
    // SAFETY: Caller guarantees pointer is valid.
    let arch_state = unsafe { &*arch_state };
    enter_full(arch_state, vector_table, context, code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_is_user_accessible() {
        assert!(is_user_accessible(0x0000_7fff_ffff_ffff));
        assert!(!is_user_accessible(0x0080_0000_0000_0000));
        assert!(!is_user_accessible(0xffff_ffff_8000_0000));
    }

    #[test]
    fn test_validate_state_pre_restricted_entry_aarch64() {
        let mut state = zx_restricted_state_t::default();
        state.pc = 0x1000;
        state.cpsr = ARM64_USER_RESTRICTED_VISIBLE_FLAGS;
        assert_eq!(validate_state_pre_restricted_entry(&state), Ok(()));

        // Invalid PC (> user accessible address space)
        state.pc = 1usize << 55 as u64;
        assert_eq!(validate_state_pre_restricted_entry(&state), Err(Status::BAD_STATE));
        state.pc = 0x1000;

        // Invalid flag in CPSR for AArch64
        state.cpsr = 0x0001_0000;
        assert_eq!(validate_state_pre_restricted_entry(&state), Err(Status::BAD_STATE));
    }

    #[test]
    fn test_validate_state_pre_restricted_entry_aarch32() {
        let mut state = zx_restricted_state_t::default();
        state.pc = 0x1000;
        state.cpsr = ARM32_BIT_MODE | ARM_NCZV_FLAGS;
        // Note: feature_test requires ARM32 ISA support in global feature mask.
        // If arm64_feature_test returns true in test harness, we test the alignment and bounds.
        if arm64_feature_test(ZX_ARM64_FEATURE_ISA_ARM32) {
            assert_eq!(validate_state_pre_restricted_entry(&state), Ok(()));

            // Out-of-range 32-bit PC
            state.pc = 1u64 << 32;
            assert_eq!(validate_state_pre_restricted_entry(&state), Err(Status::BAD_STATE));
            state.pc = 0x1000;

            // Unaligned A32 PC (must be 4-byte aligned when T bit == 0)
            state.pc = 0x1003;
            assert_eq!(validate_state_pre_restricted_entry(&state), Err(Status::BAD_STATE));

            // In Thumb mode (T bit == 1), an address with bit 0 set (interworking convention)
            // is accepted to match C++ behavior.
            state.cpsr |= ARM32_BIT_THUMB_MODE;
            state.pc = 0x1001;
            assert_eq!(validate_state_pre_restricted_entry(&state), Ok(()));
            state.pc = 0x1002;
            assert_eq!(validate_state_pre_restricted_entry(&state), Ok(()));
        }
    }
}
