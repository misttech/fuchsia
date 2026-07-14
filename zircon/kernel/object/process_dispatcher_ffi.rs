// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::dispatcher::Dispatcher;
use crate::handle::{HandleValue, KernelHandle};
use crate::process_dispatcher::ProcessDispatcher;
use zx_types::{zx_rights_t, zx_status_t};

unsafe extern "C" {
    pub(crate) fn cpp_process_dispatcher_current() -> *const ProcessDispatcher;
    pub(crate) fn cpp_process_dispatcher_make_and_add_handle(
        process: *const ProcessDispatcher,
        handle: *mut KernelHandle<Dispatcher>,
        rights: zx_rights_t,
        out_handle: *mut HandleValue,
    ) -> zx_status_t;
}
