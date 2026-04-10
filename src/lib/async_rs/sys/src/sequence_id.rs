// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_char;

use zx_types::zx_status_t;

use crate::async_dispatcher_t;

#[repr(C)]
pub struct async_sequence_id_t {
    pub value: u64,
}

unsafe extern "C" {
    pub fn async_get_sequence_id(
        dispatcher: *mut async_dispatcher_t,
        out_sequence_id: *mut async_sequence_id_t,
        out_error: *mut *const c_char,
    ) -> zx_status_t;
    pub fn async_check_sequence_id(
        dispatcher: *mut async_dispatcher_t,
        sequence_id: async_sequence_id_t,
        out_error: *mut *const c_char,
    ) -> zx_status_t;
}
