// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::sampler_dispatcher::{SamplerDispatcher, SamplerDispatcherState};
use zx_types::{zx_sampler_config_t, zx_status_t};

// C++ FFI declarations
unsafe extern "C" {
    /// Calls into C++ implementation to create a SamplerDispatcher.
    pub fn cpp_sampler_dispatcher_create(
        config: *const zx_sampler_config_t,
        handle_out: *mut core::mem::MaybeUninit<KernelHandle<SamplerDispatcher>>,
    ) -> zx_status_t;

    /// Calls into C++ implementation to start sampling for a dispatcher.
    pub fn cpp_sampler_dispatcher_start(dispatcher: *const SamplerDispatcher) -> zx_status_t;

    /// Calls into C++ implementation to stop sampling for a dispatcher.
    pub fn cpp_sampler_dispatcher_stop(dispatcher: *const SamplerDispatcher) -> zx_status_t;

    /// Calls into C++ implementation to read sampled records into user memory.
    pub fn cpp_sampler_dispatcher_read_user(
        dispatcher: *const SamplerDispatcher,
        ptr: *mut core::ffi::c_void,
        len: usize,
        actual_out: *mut usize,
    ) -> zx_status_t;

    /// Checks if thread sampling is enabled.
    pub fn cpp_sampler_enabled() -> bool;
}

/// Returns whether thread sampler feature is enabled.
pub fn sampler_enabled() -> bool {
    // SAFETY: FFI query function reads global boot/feature option and has no side effects.
    unsafe { cpp_sampler_enabled() }
}

// Trampolines from C++ into Rust SamplerDispatcherState

crate::object::dispatcher::impl_dispatcher_state_init!(SamplerDispatcher, SamplerDispatcherState);
