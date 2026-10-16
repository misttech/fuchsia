// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::ktrace::{VcpuBlockOp, VcpuExit, VcpuMeta, ktrace_vcpu, ktrace_vcpu_exit};

/// FFI entry point for recording a VCPU block or unblock ktrace duration event.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_vcpu(block_op: VcpuBlockOp, meta: VcpuMeta) {
    ktrace_vcpu(block_op, meta);
}

/// FFI entry point for recording a VCPU exit ktrace duration end event.
#[unsafe(no_mangle)]
pub extern "C" fn rust_ktrace_vcpu_exit(exit: VcpuExit, exit_address: u64) {
    ktrace_vcpu_exit(exit, exit_address);
}
