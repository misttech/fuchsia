// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::zx_time_t;

use crate::async_dispatcher_t;

unsafe extern "C" {
    pub fn async_now(dispatcher: *mut async_dispatcher_t) -> zx_time_t;
}
