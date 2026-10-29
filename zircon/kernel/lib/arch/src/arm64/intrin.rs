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

/// Sends an event to every PE, waking any that wait in WFE.
///
/// Mirrors `__sev()` from `<arm_acle.h>`.
#[inline(always)]
pub fn sev() {
    // SAFETY: See `isb()`. SEV only signals an event.
    unsafe {
        asm!("sev", options(nostack, preserves_flags));
    }
}

/// The shareability domain and access types that a DMB or DSB orders: the `ARM_MB_*` values in
/// `zircon/kernel/lib/arch/arm64/include/lib/arch/intrin.h`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarrierOption {
    /// Outer Shareable, loads.
    OshLd = 0x1,
    /// Outer Shareable, stores.
    OshSt = 0x2,
    /// Outer Shareable, all accesses.
    Osh = 0x3,
    /// Non-shareable, loads.
    NshLd = 0x5,
    /// Non-shareable, stores.
    NshSt = 0x6,
    /// Non-shareable, all accesses.
    Nsh = 0x7,
    /// Inner Shareable, loads.
    IshLd = 0x9,
    /// Inner Shareable, stores.
    IshSt = 0xa,
    /// Inner Shareable, all accesses.
    Ish = 0xb,
    /// Full system, loads.
    Ld = 0xd,
    /// Full system, stores.
    St = 0xe,
    /// Full system, all accesses.
    Sy = 0xf,
}

/// Emits `$insn` with `$option` as its option. The option must be an immediate, so each one gets
/// its own `asm!`; with a constant argument the `match` folds away.
macro_rules! barrier_with_option {
    ($insn:literal, $option:expr) => {
        // SAFETY: See `isb()`.
        unsafe {
            match $option {
                BarrierOption::OshLd => {
                    asm!(concat!($insn, " oshld"), options(nostack, preserves_flags))
                }
                BarrierOption::OshSt => {
                    asm!(concat!($insn, " oshst"), options(nostack, preserves_flags))
                }
                BarrierOption::Osh => {
                    asm!(concat!($insn, " osh"), options(nostack, preserves_flags))
                }
                BarrierOption::NshLd => {
                    asm!(concat!($insn, " nshld"), options(nostack, preserves_flags))
                }
                BarrierOption::NshSt => {
                    asm!(concat!($insn, " nshst"), options(nostack, preserves_flags))
                }
                BarrierOption::Nsh => {
                    asm!(concat!($insn, " nsh"), options(nostack, preserves_flags))
                }
                BarrierOption::IshLd => {
                    asm!(concat!($insn, " ishld"), options(nostack, preserves_flags))
                }
                BarrierOption::IshSt => {
                    asm!(concat!($insn, " ishst"), options(nostack, preserves_flags))
                }
                BarrierOption::Ish => {
                    asm!(concat!($insn, " ish"), options(nostack, preserves_flags))
                }
                BarrierOption::Ld => asm!(concat!($insn, " ld"), options(nostack, preserves_flags)),
                BarrierOption::St => asm!(concat!($insn, " st"), options(nostack, preserves_flags)),
                BarrierOption::Sy => asm!(concat!($insn, " sy"), options(nostack, preserves_flags)),
            }
        }
    };
}

/// Data Synchronization Barrier over `option`'s domain and access types.
///
/// Mirrors `__dsb(ARM_MB_*)` from `zircon/kernel/lib/arch/arm64/include/lib/arch/intrin.h`.
#[inline(always)]
pub fn dsb(option: BarrierOption) {
    barrier_with_option!("dsb", option);
}

/// Data Memory Barrier over `option`'s domain and access types.
///
/// Mirrors `__dmb(ARM_MB_*)` from `zircon/kernel/lib/arch/arm64/include/lib/arch/intrin.h`.
#[inline(always)]
pub fn dmb(option: BarrierOption) {
    barrier_with_option!("dmb", option);
}

/// Invalidates this CPU's instruction and data TLBs, at EL2 when running there.
///
/// The caller must perform an instruction barrier ([`isb()`]) before relying on the operation
/// being complete.
///
/// Mirrors the C++ `arch::InvalidateLocalTlbs()` in `<lib/arch/cache.h>`.
#[inline(always)]
pub fn invalidate_local_tlbs() {
    if super::CURRENT_EL.read().el() == 2 {
        // SAFETY: Dropping cached translations only makes later accesses walk the page tables
        // again. The block is not `nomem`, so earlier page table stores stay before it.
        unsafe { asm!("tlbi alle2", options(nostack, preserves_flags)) };
    } else {
        // SAFETY: As above.
        unsafe { asm!("tlbi vmalle1", options(nostack, preserves_flags)) };
    }
    dsb(BarrierOption::Nsh);
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
