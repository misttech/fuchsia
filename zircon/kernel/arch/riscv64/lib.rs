// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::arch::asm;
use core::sync::atomic::{Ordering, compiler_fence};

/// Enable interrupts on the current CPU.
#[inline(always)]
pub fn enable_ints() {
    compiler_fence(Ordering::SeqCst);
    // Sets the interrupt enable bit in the `sstatus` register to enable interrupts on the current
    // processor.
    unsafe {
        asm!("csrsi sstatus, 2", options(nostack));
    }
}

/// Disable interrupts on the current CPU.
#[inline(always)]
pub fn disable_ints() {
    // Clears the interrupt enable bit in the `sstatus` register to disable interrupts on the
    // current processor.
    unsafe {
        asm!("csrci sstatus, 2", options(nostack));
    }
    compiler_fence(Ordering::SeqCst);
}

/// Returns true if interrupts are disabled on the current CPU.
#[inline(always)]
pub fn ints_disabled() -> bool {
    let state: u64;
    // Reads the `sstatus` register to check if interrupts are disabled.
    unsafe {
        asm!("csrr {}, sstatus", out(reg) state, options(nostack));
    }
    (state & 2) == 0
}

/// Returns the CPU number of the calling CPU.
#[inline(always)]
pub fn curr_cpu_num() -> u32 {
    let cpu_num: u32;
    // LINT.IfChange(curr_cpu_num)
    // Reads the current CPU number from the `riscv64_percpu` structure pointed to by `s11`.
    unsafe {
        asm!("lwu {:w}, 0(s11)", out(reg) cpu_num, options(nostack, preserves_flags, readonly));
    }
    // LINT.ThenChange(//zircon/kernel/arch/riscv64/include/arch/riscv64/mp.h:riscv64_percpu)
    cpu_num
}

unsafe extern "C" {
    #[link_name = "riscv64_num_cpus"]
    static RISCV64_NUM_CPUS: u32;
}

/// Returns the maximum number of CPUs in the system.
#[inline(always)]
pub fn max_num_cpus() -> u32 {
    // Reads the system-wide constant `RISCV64_NUM_CPUS`.
    unsafe { RISCV64_NUM_CPUS }
}
