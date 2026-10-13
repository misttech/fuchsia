// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg_attr(not(test), no_std)]

pub mod arm64;
pub mod riscv64;
pub mod x86;

#[cfg(target_arch = "aarch64")]
pub use arm64::{
    cycles, device_memory_barrier, store_memory_barrier, thread_memory_barrier, r#yield,
    yield_processor,
};
#[cfg(target_arch = "riscv64")]
pub use riscv64::{
    cycles, device_memory_barrier, store_memory_barrier, thread_memory_barrier, r#yield,
    yield_processor,
};
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
pub use x86::{
    cycles, device_memory_barrier, store_memory_barrier, thread_memory_barrier, r#yield,
    yield_processor,
};
