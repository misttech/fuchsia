// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use starnix_uapi::uapi;
use starnix_uapi::user_address::ArchSpecific;

pub fn canonicalize_ioctl_request(current_task: &CurrentTask, request: u32) -> u32 {
    if current_task.is_arch32() {
        match request {
            uapi::arch32::BLKGETSIZE64 => uapi::BLKGETSIZE64,
            _ => request,
        }
    } else {
        request
    }
}
