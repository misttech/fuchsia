// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::{ChannelDispatcher, ChannelDispatcherState};
use super::handle::KernelHandle;
use core::mem::MaybeUninit;
use zx_types::{zx_rights_t, zx_status_t};

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Creates a C++ ChannelDispatcher object wrapping the Rust peered state.
    ///
    /// # Safety
    ///
    /// `holder` must be a valid pointer to a `PeerHolder`. `handle_out` must point to valid
    /// uninitialized memory for `KernelHandle<ChannelDispatcher>`.
    pub fn cpp_channel_dispatcher_create(
        holder: *mut (),
        handle_out: *mut MaybeUninit<KernelHandle<ChannelDispatcher>>,
    ) -> zx_status_t;
}

crate::object::dispatcher::impl_peered_dispatcher_state_init!(
    ChannelDispatcher,
    ChannelDispatcherState,
);

/// Exports `rust_channel_dispatcher_create` to C++.
#[unsafe(no_mangle)]
pub extern "C" fn rust_channel_dispatcher_create(
    handle0: &mut KernelHandle<ChannelDispatcher>,
    handle1: &mut KernelHandle<ChannelDispatcher>,
    rights: &mut zx_rights_t,
) -> zx_status_t {
    match ChannelDispatcher::create() {
        Ok((h0, h1, r)) => {
            *handle0 = h0;
            *handle1 = h1;
            *rights = r;
            zx_types::ZX_OK
        }
        Err(status) => status.into_raw(),
    }
}

/// Retrieves message counts for diagnostics.
#[unsafe(no_mangle)]
pub extern "C" fn rust_channel_dispatcher_get_message_counts(
    disp: &ChannelDispatcher,
    current: &mut u64,
    max: &mut u64,
) {
    let (cur, mx) = disp.get_message_counts();
    *current = cur as u64;
    *max = mx;
}
