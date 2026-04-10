// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::{zx_handle_t, zx_packet_interrupt_t, zx_status_t};

use crate::{async_dispatcher_t, async_state_t};

pub type async_irq_handler_t = extern "C" fn(
    *mut async_dispatcher_t,
    *mut async_irq_t,
    zx_status_t,
    *const zx_packet_interrupt_t,
);

#[repr(C)]
pub struct async_irq_t {
    pub state: async_state_t,
    pub handler: async_irq_handler_t,
    pub object: zx_handle_t,
}

unsafe extern "C" {
    pub fn async_bind_irq(
        dispatcher: *mut async_dispatcher_t,
        irq: *mut async_irq_t,
    ) -> zx_status_t;
    pub fn async_unbind_irq(
        dispatcher: *mut async_dispatcher_t,
        irq: *mut async_irq_t,
    ) -> zx_status_t;
}
