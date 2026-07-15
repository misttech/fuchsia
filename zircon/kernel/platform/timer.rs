// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

pub mod power;

#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstantMono(pub i64);

impl core::ops::Add<i64> for InstantMono {
    type Output = Self;

    #[inline]
    fn add(self, rhs: i64) -> Self {
        Self(self.0 + rhs)
    }
}

#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstantMonoTicks(pub i64);

#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstantBoot(pub i64);

#[repr(transparent)]
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InstantBootTicks(pub i64);

unsafe extern "C" {
    fn cpp_timer_current_mono_ticks() -> InstantMonoTicks;
    fn cpp_timer_current_boot_ticks() -> InstantBootTicks;
    fn cpp_current_mono_time() -> InstantMono;
    fn cpp_current_boot_time() -> InstantBoot;
}

/// Returns the current monotonic time in ticks.
#[inline]
pub fn timer_current_mono_ticks() -> InstantMonoTicks {
    unsafe { cpp_timer_current_mono_ticks() }
}

/// Returns the current boot time in ticks.
#[inline]
pub fn timer_current_boot_ticks() -> InstantBootTicks {
    unsafe { cpp_timer_current_boot_ticks() }
}

/// Current monotonic time in nanoseconds.
#[inline]
pub fn current_mono_time() -> InstantMono {
    unsafe { cpp_current_mono_time() }
}

/// Current boot time in nanoseconds.
#[inline]
pub fn current_boot_time() -> InstantBoot {
    unsafe { cpp_current_boot_time() }
}
