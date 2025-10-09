// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_core::task::{CurrentTask, ExitStatus};
use starnix_sync::{Locked, Unlocked};

pub fn enter(locked: &mut Locked<Unlocked>, current_task: &mut CurrentTask) -> ExitStatus {
    starnix_core::execution::actually_enter_syscall_loop(locked, current_task)
}
