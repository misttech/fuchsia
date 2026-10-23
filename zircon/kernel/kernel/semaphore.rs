// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Counting semaphore facade mirroring `<kernel/semaphore.h>`.

use core::convert::Infallible;
use core::ffi::c_void;
use core::mem::{align_of, size_of};
use pin_init::{PinInit, pin_data};
use zr::{OpaqueBytes, static_assert};
use zx_status::Status;
use zx_types::zx_status_t;

use crate::kernel::deadline::Deadline;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::kernel::wait_queue::WaitQueue;

unsafe extern "C" {
    fn cpp_semaphore_init(semaphore: *mut Semaphore, initial_count: i64);
    fn cpp_semaphore_destroy(semaphore: *mut Semaphore);
    fn cpp_semaphore_post(semaphore: *mut Semaphore);
    /// `queue_to_own` is null or an `OwnedWaitQueue*` (passed as `void*` because the Rust
    /// `OwnedWaitQueue` is `!Unpin` and therefore not FFI-safe in `extern` signatures).
    fn cpp_semaphore_post_etc(semaphore: *mut Semaphore, queue_to_own: *mut c_void);
    fn cpp_semaphore_wait_deadline(
        semaphore: *mut Semaphore,
        deadline: *const Deadline,
    ) -> zx_status_t;
}

const SEMAPHORE_SIZE: usize = 8 + size_of::<WaitQueue>();

/// A basic counting semaphore used to control access to a shared resource.
#[pin_data(PinnedDrop)]
#[repr(C, align(8))]
pub struct Semaphore {
    _opaque: OpaqueBytes<SEMAPHORE_SIZE>,
}

static_assert!(size_of::<Semaphore>() == SEMAPHORE_SIZE);
static_assert!(align_of::<Semaphore>() == 8);

zr::unsafe_pinned_drop_ffi!(Semaphore, cpp_semaphore_destroy);

impl Semaphore {
    /// Creates an in-place pinned initializer for a `Semaphore` with `initial_count`.
    pub fn new(initial_count: i64) -> impl PinInit<Self, Infallible> {
        zr::pin_init_ffi!(cpp_semaphore_init, initial_count)
    }

    /// Unblocks a single waiting thread if any are waiting, or increments the semaphore count.
    #[inline]
    pub fn post(&self) {
        // SAFETY: `self` is a valid initialized `Semaphore`.
        unsafe {
            cpp_semaphore_post(core::ptr::from_ref(self).cast_mut());
        }
    }

    /// Like [`Self::post`], but if a thread is unblocked and `queue_to_own` is non-null, assigns
    /// ownership of `queue_to_own` to the unblocked thread (mirrors
    /// `Semaphore::Post(queue_to_own)`).
    ///
    /// # Safety
    ///
    /// `queue_to_own` must either be null or point to a valid `OwnedWaitQueue`.
    #[inline]
    pub unsafe fn post_etc(&self, queue_to_own: *mut OwnedWaitQueue) {
        // SAFETY: `self` is a valid initialized `Semaphore` and the caller guarantees
        // `queue_to_own` is valid or null.
        unsafe {
            cpp_semaphore_post_etc(core::ptr::from_ref(self).cast_mut(), queue_to_own.cast());
        }
    }

    /// Waits interruptibly until `deadline` for the semaphore count to be positive and
    /// decrements it.
    #[inline]
    pub fn wait(&self, deadline: &Deadline) -> Result<(), Status> {
        // SAFETY: `self` and `deadline` are valid initialized references.
        Status::ok(unsafe {
            cpp_semaphore_wait_deadline(core::ptr::from_ref(self).cast_mut(), deadline)
        })
    }
}
