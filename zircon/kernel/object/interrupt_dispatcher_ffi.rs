// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ptr::NonNull;
use zx_status::Status;
use zx_types::{zx_status_t, zx_time_t};

use super::interrupt_dispatcher::{
    InterruptDispatcher, InterruptDispatcherState, WakeVectorDiagnostics,
};

type InterruptKindInitFn = unsafe extern "C" fn(disp: &InterruptDispatcher, ctx: *mut u8);

/// # Safety
///
/// `state` must point to uninitialized memory of at least `size_of::<InterruptDispatcherState>()`
/// bytes with proper alignment, `dispatcher` must point to the enclosing C++ `InterruptDispatcher`,
/// and `init_fn` must initialize `InterruptKind` from `ctx`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_state_init(
    dispatcher: NonNull<InterruptDispatcher>,
    state: *mut InterruptDispatcherState,
    flags: u32,
    options: u32,
    ctx: *mut u8,
    init_fn: InterruptKindInitFn,
) {
    // SAFETY: `state` points to valid uninitialized storage in `InterruptDispatcher`.
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(
            InterruptDispatcherState::new(dispatcher, flags, options),
            state,
        );
    }
    // SAFETY: `InterruptDispatcherState` is now initialized at `state`, and `init_fn`
    // initializes `(*state).kind` from `ctx`.
    unsafe {
        init_fn(dispatcher.as_ref(), ctx);
    }
}

/// # Safety
///
/// `disp` must be a valid reference to an initialized `InterruptDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_on_zero_handles(disp: &InterruptDispatcher) {
    disp.on_zero_handles();
}

/// # Safety
///
/// `disp` must be a valid reference to an initialized `InterruptDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_is_virtual(disp: &InterruptDispatcher) -> bool {
    disp.state().is_virtual()
}

/// # Safety
///
/// `disp` and `diagnostics_out` must be valid references.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_get_diagnostics(
    disp: &InterruptDispatcher,
    diagnostics_out: &mut WakeVectorDiagnostics,
) {
    disp.get_diagnostics(diagnostics_out);
}

/// # Safety
///
/// `disp` and `out_timestamp` must be valid.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_wait_for_interrupt(
    disp: &InterruptDispatcher,
    out_timestamp: *mut zx_time_t,
) -> zx_status_t {
    match disp.wait_for_interrupt() {
        Ok(ts) => {
            // SAFETY: `out_timestamp` is a valid non-null pointer provided by C++.
            unsafe {
                *out_timestamp = ts;
            }
            zx_status::sys::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// # Safety
///
/// `disp` must be a valid reference to an initialized `InterruptDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_interrupt_dispatcher_ack(disp: &InterruptDispatcher) -> zx_status_t {
    Status::result_into_raw(disp.ack())
}
