// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::mp::{self, MpIpiTarget};
use syscalls_macro::syscall;

/// Issue a data membarrier on all running threads within this process.
#[syscall]
pub fn sys_membarrier_sync_process_data() {
    // The membarrier operations are defined to operate on at least all running threads in
    // the calling process (specifically the calling thread's futex context). For now, just
    // issue a barrier to all running CPUs via kernel::mp::sync_exec.
    mp::sync_exec(MpIpiTarget::All, 0, || {
        libarch::thread_memory_barrier();
    });
}

/// Issue an instruction membarrier on all running threads within this process.
#[syscall]
pub fn sys_membarrier_sync_process_insn() {
    // The membarrier operations are defined to operate on at least all running threads in
    // the calling process (specifically the calling thread's futex context). For now, just
    // issue a barrier to all running CPUs via kernel::mp::sync_exec.
    mp::sync_exec(MpIpiTarget::All, 0, || {
        // All architectures require a memory barrier before serializing the instruction stream.
        libarch::thread_memory_barrier();
        libarch::serialize_instructions();
    });
}
