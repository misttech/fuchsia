// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::msr::{read_msr_on_cpu, write_msr_on_cpu};
use crate::kernel::mp::cpu_num_t;

/// Reads MSR `msr_id` on CPU `cpu` and returns the 64-bit value, or `u64::MAX` if `cpu` is
/// offline.
///
/// # Safety
///
/// `msr_id` must be an implemented, readable MSR in `cpu`'s current operating mode.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_read_msr_on_cpu(cpu: cpu_num_t, msr_id: u32) -> u64 {
    // SAFETY: Caller guarantees `msr_id` is an implemented, readable MSR on `cpu`.
    unsafe { read_msr_on_cpu(cpu, msr_id) }.unwrap_or(u64::MAX)
}

/// Writes value `val` into MSR `msr_id` on CPU `cpu` if `cpu` is online.
///
/// # Safety
///
/// `msr_id` must be an implemented, writable MSR in `cpu`'s current operating mode, `val` must
/// satisfy all architectural bit and canonical-address constraints for `msr_id`, and writing `val`
/// must not violate memory safety or kernel invariants.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_x86_write_msr_on_cpu(cpu: cpu_num_t, msr_id: u32, val: u64) {
    // SAFETY: Caller guarantees `msr_id` is writable and `val` is valid on `cpu`.
    let _ = unsafe { write_msr_on_cpu(cpu, msr_id, val) };
}
