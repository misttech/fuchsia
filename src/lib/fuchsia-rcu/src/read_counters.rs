// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(feature = "rseq_backend")]

use fuchsia_rseq::{Rseq, RseqCriticalSection};
use std::arch::global_asm;
use zx::sys::{zx_rseq_t, zx_system_get_num_cpus};

/// Counters for a single CPU.
///
/// This struct is `repr(C)` to ensure a stable layout, which is important for RSEQ operations that
/// might access these fields from assembly or specific offsets.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct PerCpuCount {
    /// The number of times a reader has started a critical section.
    begin: usize,

    /// The number of times a reader has ended a critical section.
    end: usize,
}

/// Per-CPU state containing the counters.
#[derive(Debug, Clone, Copy)]
#[repr(C)]
struct PerCpuState {
    /// Two sets of counters are maintained, one for even generations and one for odd generations.
    counts: [PerCpuCount; 2],
}

impl PerCpuState {
    /// Creates a new `PerCpuState` with zeroed counters.
    const fn new() -> Self {
        Self { counts: [PerCpuCount { begin: 0, end: 0 }; 2] }
    }

    /// Returns a pointer to the `begin` counter for the given index.
    fn begin_counter(&self, index: usize) -> *mut usize {
        &self.counts[index].begin as *const usize as *mut usize
    }

    /// Returns a pointer to the `end` counter for the given index.
    fn end_counter(&self, index: usize) -> *mut usize {
        &self.counts[index].end as *const usize as *mut usize
    }
}

/// The maximum number of CPUs that the RCU implementation supports.
///
/// This is a compile-time constant because the `per_cpu_counts` array is statically allocated.
const MAX_CPUS: u32 = 16;

/// Manages RCU read-side critical section counters across all CPUs.
pub(crate) struct RcuReadCounters {
    /// Array of per-CPU states.
    per_cpu_counts: [PerCpuState; MAX_CPUS as usize],
}

impl RcuReadCounters {
    /// Creates a new `RcuReadCounters`.
    pub(crate) const fn new() -> Self {
        Self { per_cpu_counts: [PerCpuState::new(); MAX_CPUS as usize] }
    }

    /// Returns the state for a specific CPU.
    fn get_state(&self, cpu: u32) -> &PerCpuState {
        &self.per_cpu_counts[cpu as usize]
    }

    /// Signals the start of a read-side critical section.
    ///
    /// This increments the `begin` counter for the current CPU. It uses RSEQ to ensure the
    /// increment is atomic with respect to the current CPU.
    pub(crate) fn begin(&self, index: usize) {
        unsafe {
            let rseq = Rseq::get();
            loop {
                let cpu = rseq.current_cpu();
                let counter = self.get_state(cpu).begin_counter(index);
                if rseq_add(&rseq, counter, 1, cpu) {
                    break;
                }
            }
        }
    }

    /// Signals the end of a read-side critical section.
    ///
    /// This increments the `end` counter for the current CPU. It uses RSEQ to ensure the increment
    /// is atomic with respect to the current CPU.
    pub(crate) fn end(&self, index: usize) {
        unsafe {
            let rseq = Rseq::get();
            loop {
                let cpu = rseq.current_cpu();
                let counter = self.get_state(cpu).end_counter(index);
                if rseq_add(&rseq, counter, 1, cpu) {
                    break;
                }
            }
        }
    }

    /// Checks if there are any active readers for a given index.
    ///
    /// This aggregates the counters across all CPUs. It reads the `end` counters first, executes a
    /// memory barrier, and then reads the `begin` counters. If the sum of `begin - end` is
    /// non-zero, it means there are active readers.
    ///
    /// This function can have false positives, which causes grace periods to take longer than
    /// necessary. However, it will never have false negatives.
    pub(crate) fn has_active(&self, index: usize) -> bool {
        let num_cpus = unsafe { zx_system_get_num_cpus() };

        let mut sum = 0;

        // Phase 1: Subtract Ends (read before barrier)
        for cpu in 0..num_cpus {
            sum -= self.get_state(cpu).counts[index].end;
        }

        // TODO: Add a zx_barrier() for RSEQ here.

        // Phase 2: Add Begins (read after barrier)
        for cpu in 0..num_cpus {
            sum += self.get_state(cpu).counts[index].begin;
        }

        sum != 0
    }
}

/// Adds a value to a counter using Restartable Sequences (RSEQ).
///
/// This function attempts to atomically add `value` to the memory location pointed to by `counter`.
/// It succeeds only if the current CPU matches the expected `cpu` throughout the critical section.
///
/// # Safety
///
/// The caller must ensure that `counter` points to a per-CPU counter for the given CPU.
unsafe fn rseq_add(rseq: &Rseq, counter: *mut usize, value: usize, cpu: u32) -> bool {
    unsafe {
        let start = &rcu_rseq_add_start as *const u8 as u64;
        let post = &rcu_rseq_add_post as *const u8 as u64;
        let abort = &rcu_rseq_add_abort as *const u8 as u64;

        let cs = RseqCriticalSection::new(start, post - start, abort);
        let _scope = rseq.activate(cs);
        rcu_rseq_add(counter, value, rseq.as_ptr(), cpu)
    }
}

// These symbols are defined in assembly below.
unsafe extern "C" {
    static rcu_rseq_add_start: u8;
    static rcu_rseq_add_post: u8;
    static rcu_rseq_add_abort: u8;
    fn rcu_rseq_add(counter: *mut usize, val: usize, rseq_ptr: *mut zx_rseq_t, cpu: u32) -> bool;
}

#[cfg(target_arch = "x86_64")]
global_asm!(
    ".pushsection .text.rcu_rseq_add,\"ax\",@progbits",
    ".globl rcu_rseq_add",
    ".globl rcu_rseq_add_start",
    ".globl rcu_rseq_add_post",
    ".globl rcu_rseq_add_abort",
    ".type rcu_rseq_add, @function",
    "rcu_rseq_add:",
    // RDI=counter, RSI=val, RDX=rseq_ptr, RCX=cpu
    "rcu_rseq_add_start:",
    "mov r8d, [rdx]",
    "cmp r8d, ecx",
    "jne rcu_rseq_add_abort",
    "add [rdi], rsi",
    "rcu_rseq_add_post:",
    "mov eax, 1",
    "ret",
    "rcu_rseq_add_abort:",
    "xor eax, eax",
    "ret",
    ".size rcu_rseq_add, . - rcu_rseq_add",
    ".popsection"
);

#[cfg(target_arch = "aarch64")]
global_asm!(
    ".pushsection .text.rcu_rseq_add,\"ax\",@progbits",
    ".globl rcu_rseq_add",
    ".globl rcu_rseq_add_start",
    ".globl rcu_rseq_add_post",
    ".globl rcu_rseq_add_abort",
    ".type rcu_rseq_add, @function",
    "rcu_rseq_add:",
    // x0=counter, x1=val, x2=rseq, x3=cpu
    "rcu_rseq_add_start:",
    "ldr w4, [x2]",
    "cmp w4, w3",
    "b.ne rcu_rseq_add_abort",
    "ldr x5, [x0]",
    "add x5, x5, x1",
    "str x5, [x0]",
    "rcu_rseq_add_post:",
    "mov w0, 1",
    "ret",
    "rcu_rseq_add_abort:",
    "mov w0, 0",
    "ret",
    ".size rcu_rseq_add, . - rcu_rseq_add",
    ".popsection"
);

#[cfg(target_arch = "riscv64")]
global_asm!(
    ".pushsection .text.rcu_rseq_add,\"ax\",@progbits",
    ".globl rcu_rseq_add",
    ".globl rcu_rseq_add_start",
    ".globl rcu_rseq_add_post",
    ".globl rcu_rseq_add_abort",
    ".type rcu_rseq_add, @function",
    "rcu_rseq_add:",
    // a0=counter, a1=val, a2=rseq, a3=cpu
    "rcu_rseq_add_start:",
    "lw t0, 0(a2)",
    "bne t0, a3, rcu_rseq_add_abort",
    "ld t1, 0(a0)",
    "add t1, t1, a1",
    "sd t1, 0(a0)",
    "rcu_rseq_add_post:",
    "li a0, 1",
    "ret",
    "rcu_rseq_add_abort:",
    "li a0, 0",
    "ret",
    ".size rcu_rseq_add, . - rcu_rseq_add",
    ".popsection"
);
