// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor ktrace helpers mirroring `<hypervisor/ktrace.h>`.

use core::mem::{align_of, size_of};
use zr::static_assert;

use super::ktrace_ffi::cpp_ktrace_vcpu;

/// Metadata categories for VCPU blocking operations.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VcpuMeta {
    /// Waiting for an interrupt.
    Interrupt = 0,
    /// Waiting for a port packet.
    Port = 1,
}

static_assert!(size_of::<VcpuMeta>() == size_of::<u32>());
static_assert!(align_of::<VcpuMeta>() == align_of::<u32>());

/// VCPU block/unblock operation type.
#[repr(u32)]
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum VcpuBlockOp {
    /// VCPU begins blocking.
    Block = 0,
    /// VCPU finishes blocking.
    Unblock = 1,
}

static_assert!(size_of::<VcpuBlockOp>() == size_of::<u32>());
static_assert!(align_of::<VcpuBlockOp>() == align_of::<u32>());

/// Records a VCPU block or unblock ktrace duration event.
#[inline]
pub fn ktrace_vcpu(block_op: VcpuBlockOp, meta: VcpuMeta) {
    // SAFETY: `cpp_ktrace_vcpu` is safe to call with any valid `VcpuBlockOp` and `VcpuMeta`.
    unsafe {
        cpp_ktrace_vcpu(block_op, meta);
    }
}
