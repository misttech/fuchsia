// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::{zx_handle_t, zx_packet_user_t, zx_status_t, zx_time_t, zx_vaddr_t};

use crate::{
    async_guest_bell_trap_t, async_irq_t, async_paged_vmo_t, async_receiver_t, async_sequence_id_t,
    async_task_t, async_wait_t,
};

#[repr(C)]
pub struct async_state_t {
    pub reserved: [usize; 2],
}

pub type async_ops_version_t = u32;

pub const ASYNC_OPS_V1: async_ops_version_t = 1;
pub const ASYNC_OPS_V2: async_ops_version_t = 2;
pub const ASYNC_OPS_V3: async_ops_version_t = 3;

#[repr(C)]
pub struct async_ops_v1_t {
    pub now: Option<extern "C" fn(*mut async_dispatcher_t) -> zx_time_t>,
    pub begin_wait:
        Option<extern "C" fn(*mut async_dispatcher_t, *mut async_wait_t) -> zx_status_t>,
    pub cancel_wait:
        Option<extern "C" fn(*mut async_dispatcher_t, *mut async_wait_t) -> zx_status_t>,
    pub post_task: Option<extern "C" fn(*mut async_dispatcher_t, *mut async_task_t) -> zx_status_t>,
    pub cancel_task:
        Option<extern "C" fn(*mut async_dispatcher_t, *mut async_task_t) -> zx_status_t>,
    pub queue_packet: Option<
        extern "C" fn(
            *mut async_dispatcher_t,
            *mut async_receiver_t,
            *const zx_packet_user_t,
        ) -> zx_status_t,
    >,
    pub set_guest_bell_trap: Option<
        extern "C" fn(
            *mut async_dispatcher_t,
            *mut async_guest_bell_trap_t,
            zx_handle_t,
            zx_vaddr_t,
            usize,
        ) -> zx_status_t,
    >,
}

#[repr(C)]
pub struct async_ops_v2_t {
    pub bind_irq: Option<extern "C" fn(*mut async_dispatcher_t, *mut async_irq_t) -> zx_status_t>,
    pub unbind_irq: Option<extern "C" fn(*mut async_dispatcher_t, *mut async_irq_t) -> zx_status_t>,
    pub create_paged_vmo: Option<
        extern "C" fn(
            *mut async_dispatcher_t,
            *mut async_paged_vmo_t,
            u32,
            zx_handle_t,
            u64,
            zx_handle_t,
        ) -> zx_status_t,
    >,
    pub detach_paged_vmo:
        Option<extern "C" fn(*mut async_dispatcher_t, *mut async_paged_vmo_t) -> zx_status_t>,
}

#[repr(C)]
pub struct async_ops_v3_t {
    pub get_sequence_id: Option<
        extern "C" fn(
            *mut async_dispatcher_t,
            *mut async_sequence_id_t,
            *mut *const u8,
        ) -> zx_status_t,
    >,
    pub check_sequence_id: Option<
        extern "C" fn(*mut async_dispatcher_t, async_sequence_id_t, *mut *const u8) -> zx_status_t,
    >,
}

#[repr(C)]
pub struct async_ops_t {
    pub version: async_ops_version_t,
    pub reserved: u32,
    pub v1: async_ops_v1_t,
    pub v2: async_ops_v2_t,
    pub v3: async_ops_v3_t,
}

#[repr(C)]
pub struct async_dispatcher_t {
    pub ops: *const async_ops_t,
}
