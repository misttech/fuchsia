// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::ChannelDispatcher;
use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use core::ffi::c_void;
use pin_init::PinInit;
use zx_status::Status;
use zx_types::{zx_rights_t, zx_status_t};

/// Initializes an `Exceptionate` in-place within `storage`.
///
/// # Safety
///
/// `storage` must point to valid, properly aligned, uninitialized memory of size
/// `kExceptionateStorageSize` that will remain pinned until `rust_exceptionate_drop_storage` is
/// called.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_exceptionate_init_storage(storage: *mut c_void, type_: u32) {
    let init = Exceptionate::new(type_);
    // SAFETY: `storage` is a valid, aligned pointer to uninitialized `Exceptionate` storage, and
    // `Exceptionate::new` is infallible.
    unsafe {
        let _ = init.__pinned_init(storage.cast::<Exceptionate>());
    }
}

/// Drops an `Exceptionate` in-place within `storage`.
///
/// # Safety
///
/// `storage` must point to a valid, initialized `Exceptionate` previously initialized by
/// `rust_exceptionate_init_storage`, and must not be used after this call.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_exceptionate_drop_storage(storage: *mut c_void) {
    // SAFETY: The caller guarantees `storage` points to a valid, initialized `Exceptionate`.
    unsafe {
        core::ptr::drop_in_place(storage.cast::<Exceptionate>());
    }
}

/// Sets the backing `ChannelDispatcher` endpoint on `exceptionate`, consuming `*channel`.
///
/// # Safety
///
/// `channel` must point to a valid C++ `KernelHandle<ChannelDispatcher>`.
#[allow(improper_ctypes_definitions)]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_exceptionate_set_channel(
    exceptionate: &Exceptionate,
    channel: *mut KernelHandle<ChannelDispatcher>,
    thread_rights: zx_rights_t,
    process_rights: zx_rights_t,
) -> zx_status_t {
    // SAFETY: `channel` points to a valid `KernelHandle<ChannelDispatcher>`, which has the exact
    // same transparent single-pointer layout in C++ and Rust. `core::mem::take` moves out the
    // handle and leaves the C++ `KernelHandle` empty.
    let handle = unsafe { core::mem::take(&mut *channel) };
    Status::result_into_raw(exceptionate.set_channel(handle, thread_rights, process_rights))
}

/// Shuts down `exceptionate`, closing any active channel endpoint.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exceptionate_shutdown(exceptionate: &Exceptionate) {
    exceptionate.shutdown();
}

/// Returns whether `exceptionate` currently holds a channel with a live peer.
#[unsafe(no_mangle)]
pub extern "C" fn rust_exceptionate_has_valid_channel(exceptionate: &Exceptionate) -> bool {
    exceptionate.has_valid_channel()
}
