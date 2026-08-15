// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ffi::c_void;
use pin_init::{PinInit, pin_data};
use zx_status::Status;
use zx_types::{ZX_OK, ZX_TIME_INFINITE, zx_instant_mono_t};

unsafe extern "C" {
    fn cpp_event_init(event: *mut c_void, initial: bool);
    fn cpp_event_destroy(event: *mut c_void);
    fn cpp_event_signal(event: *mut c_void, wait_result: i32);
    fn cpp_event_signal_etc(event: *mut c_void, wait_result: i32, queue_to_own: *mut c_void);
    fn cpp_event_unsignal(event: *mut c_void);
    fn cpp_event_wait(event: *mut c_void, deadline: i64) -> i32;
    fn cpp_event_wait_deadline(event: *mut c_void, deadline: *const Deadline) -> i32;
}

#[repr(u32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlackMode {
    Center = 0,
    Early = 1,
    Late = 2,
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimerSlack {
    pub amount: zx_types::zx_duration_t,
    pub mode: SlackMode,
}

impl TimerSlack {
    pub const fn none() -> Self {
        Self { amount: 0, mode: SlackMode::Center }
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Deadline {
    pub when: zx_instant_mono_t,
    pub slack: TimerSlack,
}

impl Deadline {
    pub const fn new(when: zx_instant_mono_t, slack: TimerSlack) -> Self {
        Self { when, slack }
    }

    pub const fn no_slack(when: zx_instant_mono_t) -> Self {
        Self { when, slack: TimerSlack::none() }
    }

    pub const fn infinite() -> Self {
        Self { when: ZX_TIME_INFINITE, slack: TimerSlack::none() }
    }
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
    /// Returns a PinInit block to initialize an unsignaled raw event in-place.
    pub fn init_unsignaled() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_event_init, false)
    }

    /// Returns a PinInit block to initialize a signaled raw event in-place.
    pub fn init_signaled() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(cpp_event_init, true)
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
    /// Creates a new `KEvent` that is initially unsignaled.
    pub fn init_unsignaled() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            raw <- RawEvent::init_unsignaled(),
        })
    }

    /// Creates a new `KEvent` that is initially signaled.
    pub fn init_signaled() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(Self {
            raw <- RawEvent::init_signaled(),
        })
    }

    /// Alias for [`Self::init_unsignaled`].
    #[inline]
    pub fn init_unsignalled() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init_unsignaled()
    }

    /// Alias for [`Self::init_signaled`].
    #[inline]
    pub fn init_signalled() -> impl PinInit<Self, core::convert::Infallible> {
        Self::init_signaled()
    }

    /// Signals the event.
    ///
    /// Wakes up all waiting threads.
    pub fn signal(&self) {
        unsafe { cpp_event_signal(self.raw.as_mut_ptr(), ZX_OK) }
    }

    /// Signals the event with a specific status and optional queue to own.
    ///
    /// # Safety
    ///
    /// `queue_to_own` must be null or a valid OwnedWaitQueue pointer.
    pub unsafe fn signal_etc(&self, wait_result: Status, queue_to_own: *mut c_void) {
        unsafe {
            cpp_event_signal_etc(self.raw.as_mut_ptr(), wait_result.into_raw(), queue_to_own);
        }
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

    /// Waits for the event to be signaled with a `Deadline`.
    pub fn wait_deadline(&self, deadline: &Deadline) -> Result<(), Status> {
        let status = unsafe { cpp_event_wait_deadline(self.raw.as_mut_ptr(), deadline) };
        Status::ok(status)
    }
}

zr::static_assert!(core::mem::size_of::<RawEvent>() == 72);
zr::static_assert!(core::mem::align_of::<RawEvent>() == 8);
zr::static_assert!(core::mem::size_of::<KEvent>() == 72);
zr::static_assert!(core::mem::align_of::<KEvent>() == 8);
zr::static_assert!(core::mem::size_of::<TimerSlack>() == 16);
zr::static_assert!(core::mem::align_of::<TimerSlack>() == 8);
zr::static_assert!(core::mem::size_of::<Deadline>() == 24);
zr::static_assert!(core::mem::align_of::<Deadline>() == 8);
