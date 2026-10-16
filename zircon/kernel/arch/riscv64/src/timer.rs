// Copyright 2023 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! RISC-V 64 timer management (rdtime / SSTC / SBI timer).

use affine::Ratio;
use debug::dprintf;
use zx_status::Status;
use zx_types::zx_ticks_t;

use super::arch::{
    RISCV64_CSR_SIE, RISCV64_CSR_SIE_STIE, RISCV64_CSR_STIMECMP, RISCV64_CSR_TIME,
    arch_ints_disabled, riscv64_csr_clear, riscv64_csr_read, riscv64_csr_set, riscv64_csr_write,
};
use super::{feature, sbi};
use crate::platform_rs::timer::{
    timer_set_initial_ticks, timer_set_ticks_to_time_ratio, timer_tick, zx_sec,
};

/// Driver configuration passed from the ZBI.
pub use zbi::DcfgRiscvGenericTimerDriver;

const _: () = assert!(core::mem::size_of::<DcfgRiscvGenericTimerDriver>() == 8);
const _: () = assert!(core::mem::align_of::<DcfgRiscvGenericTimerDriver>() == 4);

/// Read the current architectural timer ticks from the `time` CSR.
#[inline(always)]
pub fn riscv_sbi_current_ticks() -> zx_ticks_t {
    // SAFETY: Reading the time CSR has no side effects and returns the hardware timebase counter.
    (unsafe { riscv64_csr_read::<RISCV64_CSR_TIME>() }) as zx_ticks_t
}

/// Program the next oneshot timer deadline in ticks.
pub fn riscv_sbi_set_oneshot_timer(mut deadline: zx_ticks_t) -> Result<(), Status> {
    debug_assert!(arch_ints_disabled());

    if deadline < 0 {
        deadline = 0;
    }

    // If the sstc feature is present, directly set the compare register instead of
    // making a call to SBI.
    if feature::has_sstc() {
        // SAFETY: Programming supervisor timer compare register for the local CPU.
        unsafe { riscv64_csr_write::<RISCV64_CSR_STIMECMP>(deadline as u64) };
    } else {
        sbi::sbi_set_timer(deadline as u64);
    }

    // Enable the timer interrupt.
    // SAFETY: Enabling supervisor timer interrupt in SIE CSR.
    unsafe { riscv64_csr_set::<RISCV64_CSR_SIE>(RISCV64_CSR_SIE_STIE) };

    Ok(())
}

/// Stop the hardware timer interrupt on the current CPU.
pub fn riscv_sbi_timer_stop() -> Result<(), Status> {
    // SAFETY: Disabling supervisor timer interrupt in SIE CSR.
    unsafe { riscv64_csr_clear::<RISCV64_CSR_SIE>(RISCV64_CSR_SIE_STIE) };
    Ok(())
}

/// Shutdown the hardware timer on the current CPU.
pub fn riscv_sbi_timer_shutdown() -> Result<(), Status> {
    debug_assert!(arch_ints_disabled());
    // SAFETY: Disabling supervisor timer interrupt in SIE CSR.
    unsafe { riscv64_csr_clear::<RISCV64_CSR_SIE>(RISCV64_CSR_SIE_STIE) };
    Ok(())
}

/// Interrupt handler called when a supervisor timer exception occurs.
#[unsafe(no_mangle)]
pub extern "C" fn riscv64_timer_exception() {
    // SAFETY: Acknowledging timer interrupt by masking STIE in SIE CSR.
    unsafe { riscv64_csr_clear::<RISCV64_CSR_SIE>(RISCV64_CSR_SIE_STIE) };
    // SAFETY: this is the timer interrupt, which runs with interrupts disabled.
    unsafe { timer_tick() };
}

static TIMER_INITIALIZED: core::sync::atomic::AtomicBool =
    core::sync::atomic::AtomicBool::new(false);

/// Returns true once the timer driver has completed early initialization: the
/// ticks-to-time ratio and the initial ticks are set, so a raw ticks read can be
/// placed on the timelines.
#[inline(always)]
pub fn timer_is_initialized() -> bool {
    TIMER_INITIALIZED.load(core::sync::atomic::Ordering::Relaxed)
}

/// Early initialization of the RISC-V generic timer driver from the driver
/// configuration physboot handed off.
///
/// # Safety
///
/// Must be called once, on the boot CPU with interrupts disabled, before
/// secondary CPUs are started and before anything converts ticks to time.
pub unsafe fn riscv_generic_timer_init_early(config: &DcfgRiscvGenericTimerDriver) {
    let initial_ticks = riscv_sbi_current_ticks() as u64;
    let ticks_to_time = Ratio::new(zx_sec(1) as u32, config.freq_hz);
    dprintf!(
        SPEW,
        "riscv generic timer cntpct_per_nsec: {}/{}\n",
        ticks_to_time.numerator(),
        ticks_to_time.denominator()
    );
    // SAFETY: the caller upholds the one-time, boot-CPU contract above.
    unsafe {
        timer_set_ticks_to_time_ratio(ticks_to_time);
        timer_set_initial_ticks(initial_ticks);
    }
    // Only now is the timer initialized: the ratio and initial ticks are set,
    // so raw ticks may be reported instead of zero.
    TIMER_INITIALIZED.store(true, core::sync::atomic::Ordering::Release);
}
