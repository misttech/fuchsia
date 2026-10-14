// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI bindings for `hypervisor::cpu`.

use super::cpu::percpu_exec;
use crate::kernel::mp::{cpu_mask_t, cpu_num_t};
use core::ffi::c_void;
use zx_status::Status;

type PerCpuTaskFn =
    unsafe extern "C" fn(context: *mut c_void, cpu_num: cpu_num_t) -> Result<(), Status>;

struct SyncContext(*mut c_void);

impl SyncContext {
    fn get(&self) -> *mut c_void {
        self.0
    }
}

// SAFETY: The caller of `rust_hypervisor_percpu_exec` guarantees that `context` is safe to access
// concurrently across all online CPUs during `task` execution.
unsafe impl Sync for SyncContext {}

/// Executes `task(context, cpu_num)` on each online CPU and returns a bitmask of
/// CPUs where `task` returned `ZX_OK`.
///
/// # Safety
///
/// `task` must be safe to invoke concurrently on all online CPUs with `context`, and
/// `context` must remain valid for the duration of this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_hypervisor_percpu_exec(
    task: PerCpuTaskFn,
    context: *mut c_void,
) -> cpu_mask_t {
    let ctx = SyncContext(context);
    // SAFETY: The caller guarantees `task` is valid to call concurrently on each CPU with
    // `context`.
    percpu_exec(|cpu_num| unsafe { task(ctx.get(), cpu_num) })
}
