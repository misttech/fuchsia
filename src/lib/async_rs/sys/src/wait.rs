// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::{zx_handle_t, zx_packet_signal_t, zx_signals_t, zx_status_t};

use crate::{async_dispatcher_t, async_state_t};

pub type async_wait_handler_t = extern "C" fn(
    *mut async_dispatcher_t,
    *mut async_wait_t,
    zx_status_t,
    *const zx_packet_signal_t,
);

#[repr(C)]
pub struct async_wait_t {
    pub state: async_state_t,
    pub handler: async_wait_handler_t,
    pub object: zx_handle_t,
    pub trigger: zx_signals_t,
    pub options: u32,
}

unsafe extern "C" {
    pub fn async_begin_wait(
        dispatcher: *mut async_dispatcher_t,
        wait: *mut async_wait_t,
    ) -> zx_status_t;
    pub fn async_cancel_wait(
        dispatcher: *mut async_dispatcher_t,
        wait: *mut async_wait_t,
    ) -> zx_status_t;
}
