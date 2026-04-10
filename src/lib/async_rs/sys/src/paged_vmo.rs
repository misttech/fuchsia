// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::{zx_handle_t, zx_packet_page_request_t, zx_status_t};

use crate::{async_dispatcher_t, async_state_t};

pub type async_paged_vmo_handler_t = extern "C" fn(
    *mut async_dispatcher_t,
    *mut async_paged_vmo_t,
    zx_status_t,
    *const zx_packet_page_request_t,
);

#[repr(C)]
pub struct async_paged_vmo_t {
    pub state: async_state_t,
    pub handler: async_paged_vmo_handler_t,
    pub pager: zx_handle_t,
    pub vmo: zx_handle_t,
}

unsafe extern "C" {
    pub fn async_create_paged_vmo(
        dispatcher: *mut async_dispatcher_t,
        paged_vmo: *mut async_paged_vmo_t,
        options: u32,
        pager: zx_handle_t,
        vmo_size: u64,
        vmo_out: *mut zx_handle_t,
    ) -> zx_status_t;
    pub fn async_detach_paged_vmo(
        dispatcher: *mut async_dispatcher_t,
        paged_vmo: *mut async_paged_vmo_t,
    ) -> zx_status_t;
}
