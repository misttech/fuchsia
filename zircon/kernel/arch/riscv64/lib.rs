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
