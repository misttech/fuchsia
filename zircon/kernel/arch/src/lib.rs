// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

unsafe extern "C" {
    fn cpp_ints_disabled() -> bool;
    fn cpp_disable_ints();
    fn cpp_enable_ints();
    fn cpp_interrupt_save() -> InterruptSavedState;
    fn cpp_interrupt_restore(state: InterruptSavedState);
    fn cpp_curr_cpu_num() -> u32;
    fn cpp_max_num_cpus() -> u32;
}

/// Returns true if interrupts are disabled on the current CPU.
#[inline(always)]
pub fn ints_disabled() -> bool {
    unsafe { cpp_ints_disabled() }
}

/// Disable interrupts on the current CPU.
#[inline(always)]
pub fn disable_ints() {
    unsafe { cpp_disable_ints() }
}

/// Enable interrupts on the current CPU.
#[inline(always)]
pub fn enable_ints() {
    unsafe { cpp_enable_ints() }
}

/// The saved interrupt state, representing architecture-specific interrupt flags.
#[cfg(target_arch = "x86_64")]
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptSavedState(usize);

/// The saved interrupt state, representing architecture-specific interrupt flags.
#[cfg(any(target_arch = "aarch64", target_arch = "riscv64"))]
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InterruptSavedState(bool);

/// Save the current interrupt state (specifically, whether interrupts are enabled)
/// and disable interrupts on the current CPU.
#[inline(always)]
pub fn arch_interrupt_save() -> InterruptSavedState {
    unsafe { cpp_interrupt_save() }
}

/// Restore the interrupt state on the current CPU to a previously saved state.
#[inline(always)]
pub fn arch_interrupt_restore(state: InterruptSavedState) {
    unsafe { cpp_interrupt_restore(state) }
}

/// A guard that disables interrupts on the current CPU when created,
/// and restores the previous interrupt state when dropped.
pub struct InterruptDisableGuard {
    state: InterruptSavedState,
}

impl InterruptDisableGuard {
    #[inline(always)]
    pub fn new() -> Self {
        Self { state: arch_interrupt_save() }
    }
}

impl Drop for InterruptDisableGuard {
    #[inline(always)]
    fn drop(&mut self) {
        arch_interrupt_restore(self.state);
    }
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
