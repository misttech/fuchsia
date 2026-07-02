// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx_types::zx_handle_t;

/// A wrapper around a handle value received from userspace.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct HandleValue {
    value: zx_handle_t,
}

impl HandleValue {
    /// Constructs a new `HandleValue` from a raw handle value.
    pub const fn new(value: zx_handle_t) -> Self {
        Self { value }
    }

    /// Returns the underlying raw handle value.
    pub fn raw_value(&self) -> zx_handle_t {
        self.value
    }
}
