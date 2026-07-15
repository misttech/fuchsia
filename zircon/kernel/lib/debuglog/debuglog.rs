// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

use zx_status::Status;

unsafe extern "C" {
    fn cpp_dlog_shutdown(deadline: platform_rs::InstantMono) -> i32;
}

/// Shutdown the debuglog subsystem.
///
/// Blocks, waiting up to |deadline|, for dlog threads to terminate.
pub fn shutdown(deadline: platform_rs::InstantMono) -> Result<(), Status> {
    Status::ok(unsafe { cpp_dlog_shutdown(deadline) })
}
