// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::arch::asm;

pub(super) unsafe fn load_u64(addr: *const u64) -> u64 {
    let result: u64;
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        asm!("ld {result}, ({addr})", addr = in(reg) addr, result = out(reg) result)
    };
    result
}

pub(super) unsafe fn store_u64(addr: *mut u64, value: u64) {
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        asm!("sd {value}, ({addr})", addr = in(reg) addr, value = in(reg) value)
    };
}

pub(super) unsafe fn load_u32(addr: *const u32) -> u32 {
    let result: u32;
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        asm!("lw {result}, ({addr})", addr = in(reg) addr, result = out(reg) result)
    };
    result
}

pub(super) unsafe fn store_u32(addr: *mut u32, value: u32) {
    #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
    unsafe {
        asm!("sw {value}, ({addr})", addr = in(reg) addr, value = in(reg) value)
    };
}
