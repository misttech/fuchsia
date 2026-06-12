// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

#[cfg(target_arch = "x86_64")]
#[path = "../x86/lib.rs"]
mod x86;
#[cfg(target_arch = "x86_64")]
pub use x86::*;

#[cfg(target_arch = "aarch64")]
#[path = "../arm64/lib.rs"]
mod arm64;
#[cfg(target_arch = "aarch64")]
pub use arm64::*;

#[cfg(target_arch = "riscv64")]
#[path = "../riscv64/lib.rs"]
mod riscv64;
#[cfg(target_arch = "riscv64")]
pub use riscv64::*;
