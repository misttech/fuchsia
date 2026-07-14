// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::RawLock;
use lock_api::RawMutex as _;
use pin_init::{PinInit, pin_init_from_closure};

pub type RawMutex = fuchsia_sync::RawMutex;

impl RawLock for fuchsia_sync::RawMutex {
    type LockEntry = ();
    type GuardState = ();

    #[inline]
    unsafe fn init(
        _class_id: *const core::ffi::c_void,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        unsafe {
            pin_init_from_closure(|slot| {
                core::ptr::write(slot, fuchsia_sync::RawMutex::INIT);
                Ok(())
            })
        }
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut core::ffi::c_void {
        core::ptr::null_mut()
    }

    #[inline]
    unsafe fn acquire(&self, _entry: *mut Self::LockEntry) -> Self::GuardState {
        self.lock();
    }

    #[inline]
    unsafe fn release(&self, _entry: *mut Self::LockEntry, _state: Self::GuardState) {
        // SAFETY: The raw lock is held by the current thread
        unsafe {
            self.unlock();
        }
    }
}
