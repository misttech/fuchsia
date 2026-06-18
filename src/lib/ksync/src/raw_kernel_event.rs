// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_void;
use pin_init::{PinInit, pin_data};
use zx_status::Status;
use zx_types::{ZX_TIME_INFINITE, zx_instant_mono_t};

unsafe extern "C" {
    fn cpp_event_init(event: *mut c_void, initial: bool);
    fn cpp_event_destroy(event: *mut c_void);
    fn cpp_event_signal(event: *mut c_void, wait_result: i32);
    fn cpp_event_unsignal(event: *mut c_void);
    fn cpp_event_wait(event: *mut c_void, deadline: i64) -> i32;
}

#[repr(C, align(8))]
struct RawEventStorage(zr::OpaqueBytes<72>);

/// Opaque layout block matching the Zircon C++ Event exactly.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct RawEvent {
    storage: RawEventStorage,
}

// SAFETY: RawEvent contains OpaqueBytes which is Send by default, but !Sync because of UnsafeCell.
// We implement Sync manually because Event is safe to share across threads.
unsafe impl Sync for RawEvent {}

zr::unsafe_pinned_drop_ffi!(RawEvent, cpp_event_destroy);

impl RawEvent {
    /// Returns a PinInit block to initialize the raw event in-place.
    pub fn init(initial: bool) -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_event_init, initial)
    }

    #[inline]
    fn as_mut_ptr(&self) -> *mut c_void {
        self as *const Self as *mut Self as *mut c_void
    }
}

/// A wrapper around Zircon's C++ Event.
#[repr(transparent)]
#[pin_data]
pub struct KEvent {
    #[pin]
    raw: RawEvent,
}

impl KEvent {
    /// Creates a new `KEvent` with the given initial signaled state.
    pub fn init(initial: bool) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            raw <- RawEvent::init(initial),
        })
    }

    /// Signals the event.
    ///
    /// Wakes up all waiting threads.
    pub fn signal(&self) {
        unsafe { cpp_event_signal(self.raw.as_mut_ptr(), Status::OK.into_raw()) }
    }

    /// Signals the event with a specific status.
    pub fn signal_etc(&self, status: Status) {
        unsafe { cpp_event_signal(self.raw.as_mut_ptr(), status.into_raw()) }
    }

    /// Unsignals the event.
    pub fn unsignal(&self) {
        unsafe { cpp_event_unsignal(self.raw.as_mut_ptr()) }
    }

    /// Waits for the event to be signaled.
    ///
    /// Returns `Ok(())` if signaled, or an error status.
    pub fn wait(&self) -> Result<(), Status> {
        let status = unsafe { cpp_event_wait(self.raw.as_mut_ptr(), ZX_TIME_INFINITE) };
        Status::ok(status)
    }

    /// Waits for the event to be signaled with a deadline.
    pub fn wait_deadline(&self, deadline: zx_instant_mono_t) -> Result<(), Status> {
        let status = unsafe { cpp_event_wait(self.raw.as_mut_ptr(), deadline) };
        Status::ok(status)
    }
}

const _: () = {
    assert!(core::mem::size_of::<RawEvent>() == 72);
    assert!(core::mem::align_of::<RawEvent>() == 8);
};
