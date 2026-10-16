// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! The `<platform/timer.h>` entry points for the generic RISC-V 64 platform,
//! backed by the SBI timer in `arch_rs::riscv64::timer`.  Raw ticks read as
//! zero until that timer has been initialized.

use crate::arch_rs::riscv64::feature;
use crate::arch_rs::riscv64::timer::{
    riscv_sbi_current_ticks, riscv_sbi_set_oneshot_timer, riscv_sbi_timer_shutdown,
    riscv_sbi_timer_stop, timer_is_initialized,
};
use crate::platform_rs::timer::timer_get_mono_ticks_offset;
use core::arch::asm;
use zx_status::Status;
use zx_types::{zx_instant_mono_ticks_t, zx_ticks_t};

// The `GetTicksSyncFlag` bits of <platform/timer.h>: which of the surrounding
// memory accesses a raw ticks observation must be ordered against.
const GET_TICKS_SYNC_FLAG_NONE: u8 = 0;
const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS: u8 = 1 << 0;
const GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES: u8 = 1 << 1;
const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS: u8 = 1 << 2;
const GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES: u8 = 1 << 3;

/// Reads the raw platform ticks, ordered against the surrounding memory
/// accesses as the `GET_TICKS_SYNC_FLAG_*` bits in `FLAGS` demand.
///
/// This function is required to return zero if it is invoked before the timer
/// hardware is initialized (see <platform/timer.h>).
#[inline(always)]
fn platform_current_raw_ticks_synchronized<const FLAGS: u8>() -> zx_ticks_t {
    if !timer_is_initialized() {
        return 0;
    }

    // If the caller requested that the read of the current raw ticks be synchronized with respect to
    // previous loads and/or stores, ensure that we emit a fence instruction guaranteeing this as
    // described in section 6.1.1 ("CSR Access Ordering") in "The RISC-V Instruction Set Manual,
    // Volume I, Unprivileged Architecture" Version 20250508. The specification provides more detail
    // on this approach, but in summary:
    // * The timer is read via a CSR, which is modeled as a device input operation in the RISC-V
    //   memory model. As such, the successor set must consist of the "i" operand.
    // * Depending on the GetTicksSyncFlag, the predecessor set must be "r", "w", or "rw".
    const AFTER_PREVIOUS_LOADS_AND_STORES: u8 =
        GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS | GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES;
    if FLAGS & AFTER_PREVIOUS_LOADS_AND_STORES == AFTER_PREVIOUS_LOADS_AND_STORES {
        // SAFETY: a fence takes no operands and only orders the surrounding
        // accesses, which is exactly what the caller asked for.
        unsafe { asm!("fence rw, i", options(nostack)) };
    } else if FLAGS & GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS != GET_TICKS_SYNC_FLAG_NONE {
        // SAFETY: as above.
        unsafe { asm!("fence r, i", options(nostack)) };
    } else if FLAGS & GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES != GET_TICKS_SYNC_FLAG_NONE {
        // SAFETY: as above.
        unsafe { asm!("fence w, i", options(nostack)) };
    }

    // Redirect the current ticks call to the arch timer.
    let ticks = riscv_sbi_current_ticks();

    // If the caller requested that the read of the current raw ticks be synchronized with respect
    // to subsequent loads and/or stores, then once again ensure that we emit a fence instruction.
    // Once again, timer reads are modeled as a device input operation, so:
    // * The predecessor set must consist of the "i" operand.
    // * Depending on the GetTicksSyncFlag, the successor set must be "r", "w", or "rw".
    const BEFORE_SUBSEQUENT_LOADS_AND_STORES: u8 =
        GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    if FLAGS & BEFORE_SUBSEQUENT_LOADS_AND_STORES == BEFORE_SUBSEQUENT_LOADS_AND_STORES {
        // SAFETY: as above.
        unsafe { asm!("fence i, rw", options(nostack)) };
    } else if FLAGS & GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS != GET_TICKS_SYNC_FLAG_NONE {
        // SAFETY: as above.
        unsafe { asm!("fence i, r", options(nostack)) };
    } else if FLAGS & GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES != GET_TICKS_SYNC_FLAG_NONE {
        // SAFETY: as above.
        unsafe { asm!("fence i, w", options(nostack)) };
    }
    ticks
}

// One C ABI entry point per form of synchronized tick access: the C++
// `platform_current_raw_ticks_synchronized<Flags>()` specializations in
// generic_riscv64_ffi.cc forward to these, so the flags stay compile-time on
// both sides.
macro_rules! define_raw_ticks_exports {
    ($($name:ident = $flags:expr;)*) => {$(
        #[doc = concat!("`platform_current_raw_ticks_synchronized` for flags ", stringify!($flags), ".")]
        #[unsafe(no_mangle)]
        pub extern "C" fn $name() -> zx_ticks_t {
            platform_current_raw_ticks_synchronized::<{ $flags }>()
        }
    )*};
}

define_raw_ticks_exports! {
    rust_platform_current_raw_ticks_synchronized_0 = GET_TICKS_SYNC_FLAG_NONE;
    rust_platform_current_raw_ticks_synchronized_1 = GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS;
    rust_platform_current_raw_ticks_synchronized_2 = GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES;
    rust_platform_current_raw_ticks_synchronized_3 =
        GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS | GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES;
    rust_platform_current_raw_ticks_synchronized_4 = GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS;
    rust_platform_current_raw_ticks_synchronized_5 =
        GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS;
    rust_platform_current_raw_ticks_synchronized_6 =
        GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS;
    rust_platform_current_raw_ticks_synchronized_7 = GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS
        | GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS;
    rust_platform_current_raw_ticks_synchronized_8 = GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    rust_platform_current_raw_ticks_synchronized_9 =
        GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    rust_platform_current_raw_ticks_synchronized_10 =
        GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    rust_platform_current_raw_ticks_synchronized_11 = GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS
        | GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    rust_platform_current_raw_ticks_synchronized_12 =
        GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    rust_platform_current_raw_ticks_synchronized_13 = GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    rust_platform_current_raw_ticks_synchronized_14 = GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
    rust_platform_current_raw_ticks_synchronized_15 = GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_LOADS
        | GET_TICKS_SYNC_FLAG_AFTER_PREVIOUS_STORES
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_LOADS
        | GET_TICKS_SYNC_FLAG_BEFORE_SUBSEQUENT_STORES;
}

/// Converts an `arch::EarlyTicks` sample (its `time` field, a raw `rdtime`
/// value) to monotonic ticks.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_convert_early_ticks(sample_time: u64) -> zx_instant_mono_ticks_t {
    // The C++ adds a uint64_t and a zx_ticks_t, so the sum wraps.
    (sample_time as zx_ticks_t).wrapping_add(timer_get_mono_ticks_offset())
}

/// Arms the oneshot timer of the current CPU to fire at `deadline` raw ticks.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_set_oneshot_timer(deadline: zx_ticks_t) -> Result<(), Status> {
    debug_assert!(timer_is_initialized());
    riscv_sbi_set_oneshot_timer(deadline)
}

/// Stops the timer of the current CPU.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_stop_timer() {
    debug_assert!(timer_is_initialized());
    let _ = riscv_sbi_timer_stop();
}

/// Shuts the timer of the current CPU down, ahead of taking the CPU offline.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_shutdown_timer() {
    debug_assert!(timer_is_initialized());
    let _ = riscv_sbi_timer_shutdown();
}

/// Suspending the timer of the current CPU is not supported on this platform.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_suspend_timer_curr_cpu() -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// Resuming the timer of the current CPU is not supported on this platform.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_resume_timer_curr_cpu() -> Result<(), Status> {
    Err(Status::NOT_SUPPORTED)
}

/// Whether user mode may read the tick registers directly.
#[unsafe(no_mangle)]
pub extern "C" fn rust_platform_usermode_can_access_tick_registers() -> bool {
    // If the cpu claims to have Zicntr support, then it's relatively cheap for user
    // space to access the time CSR via rdtime instruction.
    feature::has_zicntr()
}
