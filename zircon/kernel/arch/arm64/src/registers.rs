// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hardware breakpoints and watchpoints, and the per-thread MDSCR_EL1 debug enables.

use libarch::arm64::{
    BreakpointControlRegister, DEBUG_BANK_SIZE, DebugFeatureRegister0, ID_AA64DFR0_EL1,
    MonitorDebugSystemControlRegister, WatchpointControlRegister, write_dbgbcr, write_dbgbvr,
    write_dbgwcr, write_dbgwvr,
};

use super::isb;
use super::thread::thread_arch;
use super::vm::is_user_accessible;
use crate::kernel::thread::Thread;

/// Kernel tracking of the current state of the debug registers for a particular thread
/// (`arm64_debug_state_t`).
type Arm64DebugState = arm64_thread_bindings::arm64_debug_state_t;

const ARM64_MAX_HW_BREAKPOINTS: usize = arm64_thread_bindings::ARM64_MAX_HW_BREAKPOINTS as usize;
const ARM64_MAX_HW_WATCHPOINTS: usize = arm64_thread_bindings::ARM64_MAX_HW_WATCHPOINTS as usize;

// The bank writers accept any index below `DEBUG_BANK_SIZE`.
zr::static_assert!(ARM64_MAX_HW_BREAKPOINTS == DEBUG_BANK_SIZE);
zr::static_assert!(ARM64_MAX_HW_WATCHPOINTS == DEBUG_BANK_SIZE);

/// `DBGBCR<n>_EL1` of an active breakpoint: enabled, PMC = 0b10 and BAS = 0b1111.
const ARM64_DBGBCR_ACTIVE_MASK: u32 = (BreakpointControlRegister::E_MASK
    | (0b10 << BreakpointControlRegister::PMC_SHIFT)
    | BreakpointControlRegister::BAS_MASK) as u32;

/// The actual addresses bits that we will allow the user to write for a hw breakpoint.
const ARM64_DBGBVR_USER_MASK: u64 = 0xffff_ffff_fffc;

/// The actual fields userspace can write to.
const ARM64_DBGWCR_ACTIVE_MASK: u32 = (WatchpointControlRegister::E_MASK
    | WatchpointControlRegister::LSC_MASK
    | WatchpointControlRegister::BAS_MASK) as u32;

/// The DBGWVR format is as follows (x = writeable):
///
/// ```text
/// |63  ...  49|48           ...             2|10|
/// |0000...0000|xxxxxxxxxxxxxxxxxxxxxxxxxxxxxx|00|
/// ```
const ARM64_DBGWVR_USER_MASK: u64 = 0xffff_ffff_fffc;

// Each mask and field used here has the value of its C macro in `<zircon/hw/debug/arm64.h>` or
// `<arch/arm64/registers.h>`.
zr::static_assert!(ARM64_DBGBCR_ACTIVE_MASK == 0x1e5);
zr::static_assert!(ARM64_DBGWCR_ACTIVE_MASK == 0x1ff9);
zr::static_assert!(BreakpointControlRegister::E_MASK == 1);
zr::static_assert!(WatchpointControlRegister::E_MASK == 1);
zr::static_assert!(
    WatchpointControlRegister::PAC_MASK == 0b11 << 1 && WatchpointControlRegister::PAC_SHIFT == 1
);
zr::static_assert!(
    WatchpointControlRegister::SSC_MASK == 0b11 << 14 && WatchpointControlRegister::SSC_SHIFT == 14
);
zr::static_assert!(
    DebugFeatureRegister0::BRPS_MASK == 0xf000 && DebugFeatureRegister0::BRPS_SHIFT == 12
);
zr::static_assert!(
    DebugFeatureRegister0::WRPS_MASK == 0xf0_0000 && DebugFeatureRegister0::WRPS_SHIFT == 20
);
zr::static_assert!(MonitorDebugSystemControlRegister::MDE_MASK == 1 << 15);
zr::static_assert!(MonitorDebugSystemControlRegister::KDE_MASK == 1 << 13);

/// Enables or disables the HW debug functionalities for `thread`.
///
/// # Safety
///
/// `thread` points to a live `Thread` whose lock the caller holds and whose user state is saved:
/// `arch_set_debug_regs` in debugger.cc checks `suspended_general_regs` under that lock, and the
/// field is cleared only under it before a return to user mode. Exception entry from user mode and
/// the return to it access `mdscr_el1` without the lock; the saved user state keeps the thread in
/// the kernel, and restoring it needs the lock, so neither runs meanwhile.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_set_debug_state_for_thread(thread: *mut Thread, active: bool) {
    // SAFETY: `thread` points to a live `Thread`, per the caller's contract.
    let arch = unsafe { thread_arch(thread) };
    // SAFETY: `arch` is the thread's live `arch_thread`. The caller holds the thread's lock, which
    // keeps out the other accesses that take it, and the thread stays in the kernel with its user
    // state saved, away from the unlocked ones on exception entry and return, while the reference
    // lives.
    let mdscr_el1 = unsafe { &mut (*arch).mdscr_el1 };
    let mut mdscr = MonitorDebugSystemControlRegister::from(*mdscr_el1);
    if active {
        mdscr.set_mde(true); // MDE enables hardware exceptions.
        mdscr.set_kde(true); // KDE enables local debugging in EL0.
    } else {
        mdscr.set_mde(false); // MDE enables hardware exceptions.
        mdscr.set_kde(false); // KDE enables local debugging in EL0.
    }
    *mdscr_el1 = mdscr.bits();
}

fn arm64_validate_hw_breakpoints(state: &mut Arm64DebugState) -> Option<u32> {
    let mut breakpoint_count = 0;

    // Validate that the addresses are valid. HW breakpoints that are beyond the
    // arch resource count will be ignored. This is because zircon will never
    // update or use those values in any way, so they will always be zero,
    // independent of what the user provided within those.
    let hw_bp_count = usize::from(arm64_hw_breakpoint_count());
    for hw_bp in state.hw_bps.iter_mut().take(hw_bp_count) {
        let dbgbcr = hw_bp.dbgbcr;
        let dbgbvr = hw_bp.dbgbvr;

        // Verify that the breakpoint refers to userspace.
        if dbgbvr != 0 && !is_user_accessible(dbgbvr as usize) {
            return None;
        }
        hw_bp.dbgbvr &= ARM64_DBGBVR_USER_MASK;

        // If the address is valid and the breakpoint is activated, we mask in
        // the other necessary value and count it for bookkeepping.
        if BreakpointControlRegister::from(u64::from(dbgbcr)).e() {
            hw_bp.dbgbcr = ARM64_DBGBCR_ACTIVE_MASK;
            breakpoint_count += 1;
        } else {
            hw_bp.dbgbcr = 0;
        }
    }

    Some(breakpoint_count)
}

fn arm64_validate_hw_watchpoints(state: &mut Arm64DebugState) -> Option<u32> {
    let mut watchpoint_count = 0;
    // Validate that the addresses are valid. HW watchpoint that are beyond the
    // arch resource count will be ignored. This is because zircon will never
    // update or use those values in any way, so they will always be zero,
    // independent of what the user provided within those.
    let hw_wp_count = usize::from(arm64_hw_watchpoint_count());
    for hw_wp in state.hw_wps.iter_mut().take(hw_wp_count) {
        let dbgwcr = &mut hw_wp.dbgwcr;
        let dbgwvr = &mut hw_wp.dbgwvr;

        // Verify that the breakpoint refers to userspace.
        if *dbgwvr != 0 && !is_user_accessible(*dbgwvr as usize) {
            return None;
        }
        *dbgwvr &= ARM64_DBGWVR_USER_MASK;

        // If the address is valid and the watchpoint is active, we mask in
        // the other necessary bits.
        if WatchpointControlRegister::from(u64::from(*dbgwcr)).e() {
            *dbgwcr &= ARM64_DBGWCR_ACTIVE_MASK;

            // See zircon/hw/debug/arm64.h for details on the fields set for PAC, HMC and SSC.
            let mut wcr = WatchpointControlRegister::from(u64::from(*dbgwcr));
            wcr.set_pac(0b10);
            wcr.set_ssc(0b01);
            *dbgwcr = wcr.bits() as u32;

            watchpoint_count += 1;
        }
    }

    Some(watchpoint_count)
}

/// Checks whether `state` is valid to install on a running thread.
///
/// Validates that every DBGBVR and DBGWVR value is either 0 or a valid userspace address, and
/// masks out reserved values in the control registers. The masking is for the caller's
/// convenience, since there is no good mechanism to tell the user what went wrong with the call.
///
/// Values beyond the CPU's register count are ignored. Zircon never uses them, so they are
/// always zero, whatever the user provided in them.
///
/// On success, `active_breakpoints` and `active_watchpoints` hold the number of activated
/// breakpoints and watchpoints in `state`.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_validate_debug_state(
    state: &mut Arm64DebugState,
    active_breakpoints: &mut u32,
    active_watchpoints: &mut u32,
) -> bool {
    let Some(breakpoints) = arm64_validate_hw_breakpoints(state) else {
        return false;
    };
    *active_breakpoints = breakpoints;
    let Some(watchpoints) = arm64_validate_hw_watchpoints(state) else {
        return false;
    };
    *active_watchpoints = watchpoints;
    true
}

/// Returns the amount of HW breakpoints present in this CPU.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_hw_breakpoint_count() -> u8 {
    let count = ID_AA64DFR0_EL1.read().brps() + 1;
    debug_assert!(usize::from(count) <= ARM64_MAX_HW_BREAKPOINTS);
    count
}

/// Returns the amount of HW watchpoints present in this CPU.
#[unsafe(no_mangle)]
pub extern "C" fn arm64_hw_watchpoint_count() -> u8 {
    let count = ID_AA64DFR0_EL1.read().wrps() + 1;
    debug_assert!(usize::from(count) <= ARM64_MAX_HW_WATCHPOINTS);
    count
}

// Writing Debug State ---------------------------------------------------------------------------

/// Writes breakpoint `index`'s control and value registers.
///
/// The bank writers panic on an `index` of `ARM64_MAX_HW_BREAKPOINTS` or more, which no caller
/// passes: the count it stays below is a 4-bit field plus one.
fn arm64_write_hw_breakpoint_by_index(index: usize, dbgbcr: u32, dbgbvr: u64) {
    write_dbgbcr(index, BreakpointControlRegister::from(u64::from(dbgbcr)));
    write_dbgbvr(index, dbgbvr);
    isb();
}

/// Writes watchpoint `index`'s control and value registers.
///
/// The bank writers panic on an `index` of `ARM64_MAX_HW_WATCHPOINTS` or more, which no caller
/// passes: the count it stays below is a 4-bit field plus one.
fn arm64_write_hw_watchpoint_by_index(index: usize, dbgwcr: u32, dbgwvr: u64) {
    write_dbgwcr(index, WatchpointControlRegister::from(u64::from(dbgwcr)));
    write_dbgwvr(index, dbgwvr);
    isb();
}

/// Writes `debug_state` into the CPU registers.
///
/// This is used in the context switch, so no validation is done, just writing. In any other
/// context (e.g. setting debug values from a syscall), the state must pass
/// `arm64_validate_debug_state` first.
///
/// # Safety
///
/// `debug_state` must have passed `arm64_validate_debug_state`, as a thread's saved debug state did
/// when it was set: unvalidated values can arm breakpoints and watchpoints on kernel addresses.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn arm64_write_hw_debug_regs(debug_state: &Arm64DebugState) {
    // Write the HW Breakpoints.
    let bps_count = usize::from(arm64_hw_breakpoint_count());
    for (i, hw_bp) in debug_state.hw_bps.iter().enumerate().take(bps_count) {
        let dbgbcr = hw_bp.dbgbcr;
        let dbgbvr = hw_bp.dbgbvr;
        arm64_write_hw_breakpoint_by_index(i, dbgbcr, dbgbvr);
    }

    // Write the HW Watchpoints.
    let wps_count = usize::from(arm64_hw_watchpoint_count());
    for (i, hw_wp) in debug_state.hw_wps.iter().enumerate().take(wps_count) {
        let dbgwcr = hw_wp.dbgwcr;
        let dbgwvr = hw_wp.dbgwvr;
        arm64_write_hw_watchpoint_by_index(i, dbgwcr, dbgwvr);
    }
}
