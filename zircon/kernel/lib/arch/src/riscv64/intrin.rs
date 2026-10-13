// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::arch::asm;

/// Yield the processor momentarily. This should be used in busy waits.
#[inline(always)]
pub fn r#yield() {
    // SAFETY: The `pause` instruction provides a hint to improve spin-wait loop performance
    // and does not modify register state or memory.
    unsafe {
        asm!("pause", options(nomem, nostack, preserves_flags));
    }
}

/// Convenience alias for [`r#yield`].
#[inline(always)]
pub fn yield_processor() {
    r#yield();
}

// TODO(https://fxbug.dev/42126965): Improve the docs on the barrier APIs, maybe rename/refine.

/// Synchronize all memory accesses of all kinds.
#[inline(always)]
pub fn device_memory_barrier() {
    // SAFETY: `fence iorw, iorw` orders device I/O and memory reads/writes without modifying
    // memory or register state.
    unsafe {
        asm!("fence iorw, iorw", options(nostack, preserves_flags));
    }
}

/// Synchronize the ordering of all memory accesses wrt other CPUs.
#[inline(always)]
pub fn thread_memory_barrier() {
    device_memory_barrier();
}

/// Ensure all stores that appear before this barrier (in program order) complete before any stores
/// that appear after this barrier.
#[inline(always)]
pub fn store_memory_barrier() {
    // SAFETY: `fence w, w` orders all preceding memory writes before subsequent memory writes
    // without modifying memory or register state.
    unsafe {
        asm!("fence w, w", options(nostack, preserves_flags));
    }
}

/// Return the current CPU cycle count.
#[inline(always)]
pub fn cycles() -> u64 {
    let time: u64;
    // SAFETY: `rdcycle` reads the current CPU cycle counter without modifying memory or flags.
    unsafe {
        asm!("rdcycle {}", out(reg) time, options(nomem, nostack, preserves_flags));
    }
    time
}
