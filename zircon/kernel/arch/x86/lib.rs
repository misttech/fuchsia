// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::arch::asm;
use core::sync::atomic::{Ordering, compiler_fence};

/// Enable interrupts on the current CPU.
#[inline(always)]
pub fn enable_ints() {
    compiler_fence(Ordering::SeqCst);
    // Executes the `sti` instruction to enable interrupts on the current processor.
    unsafe {
        asm!("sti", options(nostack));
    }
}

/// Disable interrupts on the current CPU.
#[inline(always)]
pub fn disable_ints() {
    // Executes the `cli` instruction to disable interrupts on the current processor.
    unsafe {
        asm!("cli", options(nostack));
    }
    compiler_fence(Ordering::SeqCst);
}

/// Returns true if interrupts are disabled on the current CPU.
#[inline(always)]
pub fn ints_disabled() -> bool {
    let state: u64;
    // Reads the processor flags register to check if interrupts are disabled.
    unsafe {
        asm!(
            "pushfq",
            "pop {}",
            out(reg) state,
        );
    }
    (state & (1 << 9)) == 0
}

/// Returns the CPU number of the calling CPU.
#[inline(always)]
pub fn curr_cpu_num() -> u32 {
    let cpu_num: u32;
    // LINT.IfChange(curr_cpu_num)
    // Reads the current CPU number from the `x86_percpu` structure at offset `0x58` in the `gs`
    // segment.
    unsafe {
        asm!("mov {:e}, gs:[0x58]", out(reg) cpu_num, options(nostack, preserves_flags));
    }
    // LINT.ThenChange(//zircon/kernel/arch/x86/include/arch/x86/mp.h:x86_percpu)
    cpu_num
}

unsafe extern "C" {
    #[link_name = "x86_num_cpus"]
    static X86_NUM_CPUS: u8;
}

/// Returns the maximum number of CPUs in the system.
#[inline(always)]
pub fn max_num_cpus() -> u32 {
    // Reads the system-wide constant `X86_NUM_CPUS`.
    unsafe { X86_NUM_CPUS as u32 }
}
