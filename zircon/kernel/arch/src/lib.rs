// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

unsafe extern "C" {
    fn cpp_enable_ints();
    fn cpp_disable_ints();
    fn cpp_ints_disabled() -> bool;
    fn cpp_curr_cpu_num() -> u32;
    fn cpp_max_num_cpus() -> u32;
}

/// Enable interrupts on the current CPU.
#[inline(always)]
pub fn enable_ints() {
    unsafe { cpp_enable_ints() }
}

/// Disable interrupts on the current CPU.
#[inline(always)]
pub fn disable_ints() {
    unsafe { cpp_disable_ints() }
}

/// Returns true if interrupts are disabled on the current CPU.
#[inline(always)]
pub fn ints_disabled() -> bool {
    unsafe { cpp_ints_disabled() }
}

/// Returns the CPU number of the calling CPU.
#[inline(always)]
pub fn curr_cpu_num() -> u32 {
    unsafe { cpp_curr_cpu_num() }
}

/// Returns the maximum number of CPUs in the system.
#[inline(always)]
pub fn max_num_cpus() -> u32 {
    unsafe { cpp_max_num_cpus() }
}
