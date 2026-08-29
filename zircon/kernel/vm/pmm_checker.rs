// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::marker::{PhantomData, PhantomPinned};
use pin_init::{PinInit, pin_data};
use pmm_checker_bindings as bindings;
use zr::Opaque;

// Note: The upstream C++ `PmmChecker` comments and documentation are intentionally not copied
// over yet.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PmmChecker {
    raw: Opaque<bindings::PmmChecker>,
    phantom: PhantomData<PhantomPinned>,
}

zr::unsafe_pinned_drop_ffi!(PmmChecker, bindings::cpp_pmm_checker_destroy);

impl PmmChecker {
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(bindings::cpp_pmm_checker_init)
    }
    /// Domain-specific conversion: returns raw pointer for `PmmChecker`.
    pub fn as_raw(&self) -> *mut bindings::PmmChecker {
        self.raw.get()
    }
}
