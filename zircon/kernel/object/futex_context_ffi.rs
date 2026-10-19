// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::futex_context::{FutexContext, ProcessType};
use super::thread_dispatcher::{CoreThreadObservation, ThreadDispatcher};
use core::ffi::c_void;
use core::mem::MaybeUninit;
use core::pin::Pin;
use pin_init::PinInit;
use zx_status::Status;
use zx_types::zx_status_t;

// C++ `ThreadDispatcher` shims (in `thread_dispatcher_ffi.cc`) for members that C++ only exposes
// to `FutexContext` via `friend class FutexContext`.
unsafe extern "C" {
    /// Returns the blocking futex ID of `thread`.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(super) fn cpp_thread_dispatcher_blocking_futex_id(thread: *const ThreadDispatcher)
    -> usize;

    /// Sets the blocking futex ID of `thread`.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher`.
    pub(super) fn cpp_thread_dispatcher_set_blocking_futex_id(
        thread: *mut ThreadDispatcher,
        id: usize,
    );

    /// Acquires `thread`'s `core_thread_lock_` and initializes `out_obs` with a
    /// `CoreThreadObservation` via `ThreadDispatcher::ObserveCoreThread()`.
    ///
    /// # Safety
    ///
    /// `thread` must point to a valid `ThreadDispatcher` whose dispatcher lock is held, and
    /// `out_obs` must point to valid uninitialized memory for `CoreThreadObservation`.
    pub(super) fn cpp_thread_dispatcher_observe_core_thread(
        thread: *mut ThreadDispatcher,
        out_obs: *mut MaybeUninit<CoreThreadObservation>,
    );
}

/// Initializes a `FutexContext` in-place within `storage`.
///
/// # Safety
///
/// `storage` must point to valid, properly aligned, uninitialized memory of size
/// `kFutexContextStorageSize` that will remain pinned until `rust_futex_context_drop_storage` is
/// called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_futex_context_init_storage(storage: *mut c_void) {
    let init = FutexContext::new();
    // SAFETY: `storage` is a valid, aligned pointer to uninitialized `FutexContext` storage, and
    // `FutexContext::new()` is infallible.
    unsafe {
        let _ = init.__pinned_init(storage.cast::<FutexContext>());
    }
}

/// Drops a `FutexContext` in-place within `storage`.
///
/// # Safety
///
/// `storage` must point to a valid, initialized `FutexContext` previously initialized by
/// `rust_futex_context_init_storage`, and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_futex_context_drop_storage(storage: *mut c_void) {
    // SAFETY: The caller guarantees `storage` points to a valid, initialized `FutexContext`.
    unsafe {
        core::ptr::drop_in_place(storage.cast::<FutexContext>());
    }
}

/// Initializes the active futex bucket array of `FutexContext` for the given `process_type`.
///
/// # Safety
///
/// `ctx` must reference a valid, initialized, pinned `FutexContext` with exclusive access during
/// process initialization.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_futex_context_init(
    ctx: &mut FutexContext,
    process_type: ProcessType,
) -> zx_status_t {
    // SAFETY: `ctx` is a valid, pinned `FutexContext` embedded in `ShareableProcessState`.
    let ctx = unsafe { Pin::new_unchecked(ctx) };
    Status::result_into_raw(ctx.init(process_type))
}

/// Shrinks the `FutexState` free pool by two states.
#[unsafe(no_mangle)]
pub extern "C" fn rust_futex_context_shrink_pool(ctx: &FutexContext) {
    ctx.shrink_futex_state_pool();
}
