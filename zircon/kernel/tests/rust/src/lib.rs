// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use test_macro::plus_one;

#[unsafe(no_mangle)]
pub extern "C" fn add_one_in_rust(x: i32) -> i32 {
    plus_one!(x)
}

use core::panic::PanicInfo;

#[panic_handler]
fn panic(_info: &PanicInfo<'_>) -> ! {
    loop {}
}
