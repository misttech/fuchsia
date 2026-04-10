// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::{zx_handle_t, zx_packet_guest_bell_t, zx_status_t, zx_vaddr_t};

use crate::{async_dispatcher_t, async_state_t};

pub type async_guest_bell_trap_handler_t = extern "C" fn(
    *mut async_dispatcher_t,
    *mut async_guest_bell_trap_t,
    zx_status_t,
    *const zx_packet_guest_bell_t,
);

#[repr(C)]
pub struct async_guest_bell_trap_t {
    pub state: async_state_t,
    pub handler: async_guest_bell_trap_handler_t,
}

unsafe extern "C" {
    pub fn async_set_guest_bell_trap(
        dispatcher: *mut async_dispatcher_t,
        trap: *mut async_guest_bell_trap_t,
        guest: zx_handle_t,
        addr: zx_vaddr_t,
        length: usize,
    ) -> zx_status_t;
}
