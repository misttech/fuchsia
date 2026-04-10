// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::{zx_status_t, zx_time_t};

use crate::{async_dispatcher_t, async_state_t};

pub type async_task_handler_t =
    extern "C" fn(*mut async_dispatcher_t, *mut async_task_t, zx_status_t);

#[repr(C)]
pub struct async_task_t {
    pub state: async_state_t,
    pub handler: async_task_handler_t,
    pub deadline: zx_time_t,
}

unsafe extern "C" {
    pub fn async_post_task(
        dispatcher: *mut async_dispatcher_t,
        task: *mut async_task_t,
    ) -> zx_status_t;
    pub fn async_cancel_task(
        dispatcher: *mut async_dispatcher_t,
        task: *mut async_task_t,
    ) -> zx_status_t;
}
