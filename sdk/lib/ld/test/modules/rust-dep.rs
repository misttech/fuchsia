// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
#![allow(internal_features)]
#![feature(abort_immediate)]
mod rust_panic;

#[unsafe(no_mangle)]
pub fn foo() -> i64 {
    17
}
