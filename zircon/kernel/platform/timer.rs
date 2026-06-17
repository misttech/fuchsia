// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

unsafe extern "C" {
    fn rust_timer_current_mono_ticks() -> i64;
    fn rust_timer_current_boot_ticks() -> i64;
}

/// Returns the current monotonic time in ticks.
#[inline]
pub fn timer_current_mono_ticks() -> i64 {
    unsafe { rust_timer_current_mono_ticks() }
}

/// Returns the current boot time in ticks.
#[inline]
pub fn timer_current_boot_ticks() -> i64 {
    unsafe { rust_timer_current_boot_ticks() }
}
