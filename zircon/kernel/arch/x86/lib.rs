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
