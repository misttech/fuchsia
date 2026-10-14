// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Per-CPU execution helpers for the hypervisor.

use crate::arch_rs::curr_cpu_num;
use crate::kernel::mp::{MpIpiTarget, cpu_mask_t, cpu_num_t, cpu_num_to_mask, sync_exec};
use core::sync::atomic::{AtomicU32, Ordering};
use zx_status::Status;

struct PerCpuState<F> {
    cpu_mask: AtomicU32,
    task: F,
}

impl<F> PerCpuState<F> {
    fn new(task: F) -> Self {
        Self { cpu_mask: AtomicU32::new(0), task }
    }
}

fn percpu_task<F>(state: &PerCpuState<F>)
where
    F: Fn(cpu_num_t) -> Result<(), Status>,
{
    let cpu_num = curr_cpu_num();
    if (state.task)(cpu_num).is_ok() {
        state.cpu_mask.fetch_or(cpu_num_to_mask(cpu_num), Ordering::SeqCst);
    }
}

/// Executes a task on each online CPU, and returns a CPU mask containing each
/// CPU the task was successfully run on.
pub fn percpu_exec<F>(task: F) -> cpu_mask_t
where
    F: Fn(cpu_num_t) -> Result<(), Status> + Sync,
{
    let state = PerCpuState::new(task);
    sync_exec(MpIpiTarget::All, 0, || percpu_task(&state));
    state.cpu_mask.load(Ordering::SeqCst)
}
