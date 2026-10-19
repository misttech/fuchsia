// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::KernelHandle;
use super::guest::Guest;
use super::guest_dispatcher::{GuestDispatcher, GuestDispatcherState};
use core::mem::MaybeUninit;
use fbl::UniquePtr;
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    pub(crate) fn cpp_guest_dispatcher_create(
        guest_raw: *mut Guest,
        guest_handle_out: *mut MaybeUninit<KernelHandle<GuestDispatcher>>,
    ) -> zx_status_t;
}

// Rust FFI trampolines for C++ calling into Rust GuestDispatcher

/// Initializes a `GuestDispatcherState` in-place.
///
/// # Safety
///
/// `state` must point to valid uninitialized memory for `GuestDispatcherState`.
/// `guest_raw` must be a valid, non-null owning pointer to a C++ `Guest` instance released from
/// `ktl::unique_ptr<Guest>`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_guest_dispatcher_state_init(
    state: *mut GuestDispatcherState,
    dispatcher: *const GuestDispatcher,
    guest_raw: *mut Guest,
) {
    // SAFETY: `guest_raw` is a valid owning pointer transferred from
    // `ktl::unique_ptr<Guest>::release()`.
    let guest = unsafe { UniquePtr::from_raw(guest_raw) };
    let init = GuestDispatcherState::init(dispatcher, guest);
    // SAFETY: `state` points to uninitialized memory allocated for `GuestDispatcherState`.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(init, state);
    }
}
