// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
#![no_main]
#![allow(internal_features)]
#![feature(abort_immediate)]
mod rust_panic;

unsafe extern "C" {
    fn foo() -> i64;
}

#[allow(non_snake_case)]
#[unsafe(no_mangle)]
pub fn TestStart() -> i64 {
    (unsafe { foo() }) + 6
}
