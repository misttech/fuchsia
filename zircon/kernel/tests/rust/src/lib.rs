// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use test_macro::plus_one;

#[unsafe(no_mangle)]
pub static kConstVarDefinedInRust: i32 = 17;

#[unsafe(no_mangle)]
pub static mut gVarDefinedInRust: i32 = 42;

unsafe extern "C" {
    static kConstVarExportedToRust: i32;
    static mut gVarExportedToRust: i32;
} // extern "C"

#[unsafe(no_mangle)]
pub extern "C" fn add_one_in_rust(x: i32) -> i32 {
    plus_one!(x)
}

#[unsafe(no_mangle)]
pub extern "C" fn get_const_var_exported_to_rust() -> i32 {
    unsafe { kConstVarExportedToRust }
}

#[unsafe(no_mangle)]
pub extern "C" fn fetch_add_var_exported_to_rust(x: i32) -> i32 {
    unsafe {
        let old = gVarExportedToRust;
        gVarExportedToRust += x;
        old
    }
}

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
