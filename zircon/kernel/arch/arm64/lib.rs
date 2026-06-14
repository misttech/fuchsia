// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::arch::asm;
use core::sync::atomic::{Ordering, compiler_fence};

/// Enable interrupts on the current CPU.
#[inline(always)]
pub fn enable_ints() {
    compiler_fence(Ordering::SeqCst);
    // Clears the DAIF interrupt mask bits to enable interrupts on the current processor.
    unsafe {
        asm!("msr daifclr, #6", options(nostack));
    }
}

/// Disable interrupts on the current CPU.
#[inline(always)]
pub fn disable_ints() {
    // Sets the DAIF interrupt mask bits to disable interrupts on the current processor.
    unsafe {
        asm!("msr daifset, #6", options(nostack));
    }
    compiler_fence(Ordering::SeqCst);
}

/// Returns true if interrupts are disabled on the current CPU.
#[inline(always)]
pub fn ints_disabled() -> bool {
    let state: u64;
    // Reads the DAIF interrupt mask register to check if interrupts are disabled.
    unsafe {
        asm!("mrs {}, daif", out(reg) state, options(nostack));
    }
    (state & (1 << 7)) != 0
}

/// Returns the CPU number of the calling CPU.
#[inline(always)]
pub fn curr_cpu_num() -> u32 {
    let cpu_num: u32;
    // LINT.IfChange(curr_cpu_num)
    // Reads the current CPU number from the `arm64_percpu` structure pointed to by `x20`.
    unsafe {
        asm!("ldr {:w}, [x20]", out(reg) cpu_num, options(nostack, preserves_flags));
    }
    // LINT.ThenChange(//zircon/kernel/arch/arm64/include/arch/arm64/mp.h:arm64_percpu)
    cpu_num
}

unsafe extern "C" {
    #[link_name = "arm_num_cpus"]
    static ARM_NUM_CPUS: u32;
}

/// Returns the maximum number of CPUs in the system.
#[inline(always)]
pub fn max_num_cpus() -> u32 {
    // Reads the system-wide constant `ARM_NUM_CPUS`.
    unsafe { ARM_NUM_CPUS }
}
