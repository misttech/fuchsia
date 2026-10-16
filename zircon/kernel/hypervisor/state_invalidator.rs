// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor state invalidator facade mirroring `<hypervisor/state_invalidator.h>`.

use super::state_invalidator_ffi::cpp_state_invalidator_invalidate;

/// Allows hypervisor state to be invalidated.
#[repr(C)]
pub struct StateInvalidator {
    _opaque: zr::OpaqueFacade,
}

impl StateInvalidator {
    /// Invalidates the cached hypervisor state.
    #[inline]
    pub fn invalidate(&mut self) {
        // SAFETY: `self` is a valid mutable reference to a C++ `StateInvalidator` object.
        unsafe {
            cpp_state_invalidator_invalidate(self);
        }
    }
}
