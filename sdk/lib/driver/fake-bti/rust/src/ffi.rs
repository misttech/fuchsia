// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use zx::sys::{zx_handle_t, zx_paddr_t, zx_status_t};

// LINT.IfChange

unsafe extern "C" {
    /// Creates a fake BTI object.  `out` is assigned to a handle which can be interacted with using
    /// all of the usual system calls for a BTI object (and can be wrapped in a [`zx::Bti`]).
    ///
    /// The caller takes responsibility for closing `out`.
    ///
    /// # Safety
    ///
    /// `out` must be a valid pointer.
    pub fn fake_bti_create(out: *mut zx_handle_t) -> zx_status_t;

    /// Sets the paddrs used by the fake BTI.  Whenever `zx_bti_pin` is called, these static paddrs
    /// will be written out into the resulting array.  If more physical addresses are needed than
    /// are available in paddrs, `zx_bti_pin` will return an error.
    ///
    /// `bti` must have been created via [`fake_bti_create`], otherwise an error will be returned.
    ///
    /// # Safety
    ///
    /// `paddrs` must be a valid pointer to `count` elements.
    pub fn fake_bti_set_paddrs(
        bti: zx_handle_t,
        paddrs: *const zx_paddr_t,
        count: usize,
    ) -> zx_status_t;
}

// LINT.ThenChange(//sdk/lib/driver/fake-bti/rust/ffi.h)
