// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::sampler_dispatcher::{SamplerDispatcher, SamplerDispatcherState};
use core::mem::MaybeUninit;
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    /// Calls into C++ implementation to allocate and initialize a `SamplerDispatcher`.
    ///
    /// # Safety
    ///
    /// `handle_out` must point to a valid, writable `MaybeUninit<KernelHandle<SamplerDispatcher>>`
    /// that will be initialized on `ZX_OK`.
    pub fn cpp_sampler_dispatcher_create(
        handle_out: *mut MaybeUninit<KernelHandle<SamplerDispatcher>>,
    ) -> zx_status_t;
}

/// Destroys the global thread sampler state when the last `SamplerDispatcher` handle is closed.
///
/// # Safety
///
/// The caller must ensure that `disp` is a valid, non-null, properly aligned reference to a
/// live [`SamplerDispatcher`] with no concurrent mutable aliases.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_sampler_dispatcher_on_zero_handles(disp: &SamplerDispatcher) {
    disp.on_zero_handles();
}

// Trampolines from C++ into Rust SamplerDispatcherState

crate::object::dispatcher::impl_dispatcher_state_init!(SamplerDispatcher, SamplerDispatcherState);
