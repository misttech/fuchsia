// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

#[unsafe(no_mangle)]
pub extern "C" fn test_rust_interrupt_ops() -> bool {
    let initially_disabled = arch_rs::ints_disabled();
    if initially_disabled {
        return false;
    }
    arch_rs::disable_ints();
    if !arch_rs::ints_disabled() {
        return false;
    }
    arch_rs::enable_ints();
    if arch_rs::ints_disabled() {
        return false;
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_rust_curr_cpu_num() -> u32 {
    arch_rs::curr_cpu_num()
}

#[unsafe(no_mangle)]
pub extern "C" fn test_rust_max_num_cpus() -> u32 {
    arch_rs::max_num_cpus()
}
