// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "512"]

use tracing_mutex as _;

use {async_utils as _, fidl_fuchsia_power_suspend as _};
pub mod syscalls;
pub mod task;
pub mod time;
pub mod vdso;
pub mod vfs;

pub mod testing;

// This allows macros to use paths within this crate
// by referring to them by the external crate name.
extern crate self as starnix_core;

// This is a temporary forwarding module to handle the transition from //src/starnix/kernel to
// //src/starnix/kernel/core. It needs a weird name to avoid colliding with Rust's `core` library.
#[path = "core/mod.rs"]
mod core__;
pub use core__::*;
