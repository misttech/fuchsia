// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::arch::asm;

/// Yield the processor momentarily. This should be used in busy waits.
#[inline(always)]
pub fn r#yield() {
    // SAFETY: The `yield` instruction provides a hint that the thread is in a spin-wait loop and
    // does not modify register state or memory. Omitting `nomem` preserves the `"memory"` clobber
    // of `__yield()` in C++.
    unsafe {
        asm!("yield", options(nostack, preserves_flags));
    }
}

/// Convenience alias for [`r#yield`].
#[inline(always)]
pub fn yield_processor() {
    r#yield();
}

/// Instruction Synchronization Barrier, full system.
///
/// Mirrors `__isb(ARM_MB_SY)` from `zircon/kernel/lib/arch/arm64/include/lib/arch/intrin.h`.
#[inline(always)]
pub fn isb() {
    // SAFETY: A barrier instruction has no operands and no memory safety requirements. The
    // absence of `nomem` preserves the `"memory"` clobber of the C++ macro, which keeps the
    // compiler from reordering memory accesses across the barrier.
    unsafe {
        asm!("isb sy", options(nostack, preserves_flags));
    }
}

/// Waits for an interrupt, putting the CPU in a low power state until one arrives.
///
/// Mirrors `__wfi()` from `zircon/kernel/lib/arch/arm64/include/lib/arch/intrin.h`.
#[inline(always)]
pub fn wfi() {
    // SAFETY: See `isb()`. `wfi` is architecturally permitted to return spuriously, so callers
    // must not rely on an interrupt having been delivered.
    unsafe {
        asm!("wfi", options(nostack, preserves_flags));
    }
}

// TODO(https://fxbug.dev/42126965): Improve the docs on the barrier APIs, maybe rename/refine.

/// Synchronize all memory accesses of all kinds.
#[inline(always)]
pub fn device_memory_barrier() {
    // SAFETY: See `isb()`.
    unsafe {
        asm!("dsb sy", options(nostack, preserves_flags));
    }
}

/// Synchronize the ordering of all memory accesses wrt other CPUs.
#[inline(always)]
pub fn thread_memory_barrier() {
    // SAFETY: See `isb()`.
    unsafe {
        asm!("dmb sy", options(nostack, preserves_flags));
    }
}

/// Ensure all stores that appear before this barrier (in program order) complete before any stores
/// that appear after this barrier.
#[inline(always)]
pub fn store_memory_barrier() {
    // SAFETY: See `isb()`.
    unsafe {
        asm!("dmb ishst", options(nostack, preserves_flags));
    }
}

/// Return the current CPU cycle count.
#[inline(always)]
pub fn cycles() -> u64 {
    let count: u64;
    // SAFETY: Reading `pmccntr_el0` reads the cycle counter register without modifying memory or
    // flags.
    unsafe {
        asm!("mrs {}, pmccntr_el0", out(reg) count, options(nomem, nostack, preserves_flags));
    }
    count
}
