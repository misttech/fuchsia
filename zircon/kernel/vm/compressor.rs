// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use compressor_bindings as bindings;
use core::marker::{PhantomData, PhantomPinned};
use zr::Opaque;

// Note: The upstream C++ `VmCompressor` comments and documentation are intentionally not copied
// over yet.
#[repr(C)]
pub struct VmCompressor {
    raw: Opaque<bindings::VmCompressor>,
    phantom: PhantomData<PhantomPinned>,
}

impl VmCompressor {
    /// Domain-specific conversion: returns raw pointer for `VmCompressor`.
    pub fn as_raw(&self) -> *mut bindings::VmCompressor {
        self.raw.get()
    }
}
