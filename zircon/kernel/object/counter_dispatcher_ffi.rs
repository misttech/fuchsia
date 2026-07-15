// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::counter_dispatcher::{
    CounterDispatcher, CounterDispatcherState, DISPATCHER_COUNTER_DESTROY_COUNT,
};
use crate::dispatcher::Dispatcher;
use crate::handle::KernelHandle;

use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    pub(crate) fn cpp_dispatcher_update_state(
        dispatcher: *const Dispatcher,
        clear_mask: u32,
        set_mask: u32,
    );

    pub(crate) fn cpp_counter_dispatcher_create(
        handle_out: *mut KernelHandle<CounterDispatcher>,
    ) -> zx_status_t;
}

// FFI trampolines for C++ calling into Rust CounterDispatcherState

/// # Safety
///
/// The caller must ensure `ptr` points to uninitialized memory of at least
/// `size_of::<CounterDispatcherState>()` bytes with proper alignment, and `dispatcher`
/// points to the enclosing C++ `CounterDispatcher`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_counter_dispatcher_state_init(
    ptr: *mut CounterDispatcherState,
    dispatcher: *const CounterDispatcher,
) {
    unsafe {
        let _ = pin_init::PinInit::__pinned_init(CounterDispatcherState::init(), ptr);
        cpp_dispatcher_update_state(
            dispatcher as *const Dispatcher,
            0,
            zx_types::ZX_COUNTER_NON_POSITIVE,
        );
    }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `CounterDispatcherState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_counter_dispatcher_state_destroy(ptr: *mut CounterDispatcherState) {
    unsafe {
        DISPATCHER_COUNTER_DESTROY_COUNT.add(1);
        core::ptr::drop_in_place(ptr);
    }
}
