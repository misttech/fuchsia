// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx;

// Options for zx_system_barrier
// source: zircon/system/public/zircon/syscalls-next.h
// TODO(https://fxbug.dev/297526152): When this API is stabilized, move the definitions for these
// constants into the zx crate.
const ZX_SYSTEM_BARRIER_DATA_MEMORY: u32 = 0;
const ZX_SYSTEM_BARRIER_INSTRUCTION_STREAM: u32 = 1;

pub enum BarrierType {
    /// Issues a data memory barrier on all running threads
    DataMemory,

    /// Issues a memory barrier and serializes the instruction stream on all running threads.
    InstructionStream,
}

/// Issues a barrier of the requested type on all running threads.
///
/// Wraps the `zx_system_barrier` syscall.
pub fn system_barrier(barrier_type: BarrierType) {
    let status = match barrier_type {
        BarrierType::DataMemory => {
            // SAFETY: This wraps the zx_system_barrier call which is safe.
            unsafe { zx::sys::zx_system_barrier(ZX_SYSTEM_BARRIER_DATA_MEMORY) }
        }
        BarrierType::InstructionStream => {
            // SAFETY: This wraps the zx_system_barrier call which is safe.
            unsafe { zx::sys::zx_system_barrier(ZX_SYSTEM_BARRIER_INSTRUCTION_STREAM) }
        }
    };
    assert_eq!(status, zx::sys::ZX_OK);
}
