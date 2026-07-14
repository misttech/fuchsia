// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::dispatcher::Dispatcher;

unsafe extern "C" {
    pub(crate) fn cpp_dispatcher_on_zero_handles(dispatcher: *const Dispatcher);
    pub(crate) fn cpp_dispatcher_update_state(
        dispatcher: *const Dispatcher,
        clear_mask: u32,
        set_mask: u32,
    );
    pub(crate) fn cpp_dispatcher_update_state_locked(
        dispatcher: *const Dispatcher,
        clear_mask: u32,
        set_mask: u32,
    );
    pub(crate) fn cpp_dispatcher_get_ref_counted(
        dispatcher: *const Dispatcher,
    ) -> *mut core::ffi::c_void;
    pub(crate) fn cpp_dispatcher_recycle(dispatcher: *const Dispatcher);
}
