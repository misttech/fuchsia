// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::{zx_packet_user_t, zx_status_t};

use crate::{async_dispatcher_t, async_state_t};

pub type async_receiver_handler_t = extern "C" fn(
    *mut async_dispatcher_t,
    *mut async_receiver_t,
    zx_status_t,
    *const zx_packet_user_t,
);

#[repr(C)]
pub struct async_receiver_t {
    pub state: async_state_t,
    pub handler: async_receiver_handler_t,
}

unsafe extern "C" {
    pub fn async_queue_packet(
        dispatcher: *mut async_dispatcher_t,
        receiver: *mut async_receiver_t,
        data: *const zx_packet_user_t,
    ) -> zx_status_t;
}
