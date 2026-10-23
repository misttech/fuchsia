// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::ChannelDispatcher;
use super::exceptionate_ffi::{cpp_exceptionate_set_channel, cpp_exceptionate_shutdown};
use super::handle::KernelHandle;
use zr::OpaqueFacade;
use zx_status::Status;
use zx_types::zx_rights_t;

/// Facade for the C++ `Exceptionate` class.
#[repr(C)]
pub struct Exceptionate {
    _facade: OpaqueFacade,
}

zr::static_assert!(core::mem::size_of::<Exceptionate>() == 0);

impl Exceptionate {
    /// Sets the backing `ChannelDispatcher` endpoint.
    ///
    /// The exception channel is first-come-first-served, so if there is already a valid channel
    /// in place (i.e. has a live peer) this will fail.
    ///
    /// The `*_rights` arguments give the rights to assign to task handles provided through this
    /// exception channel. A value of 0 indicates that the handle should not be made available
    /// through this channel.
    pub fn set_channel(
        &self,
        mut channel_handle: KernelHandle<ChannelDispatcher>,
        thread_rights: zx_rights_t,
        process_rights: zx_rights_t,
    ) -> Result<(), Status> {
        // SAFETY: `self` and `channel_handle` are valid.
        Status::ok(unsafe {
            cpp_exceptionate_set_channel(
                self as *const Self as *mut Self,
                &mut channel_handle,
                thread_rights,
                process_rights,
            )
        })
    }

    /// Shuts down the exceptionate.
    pub fn shutdown(&self) {
        // SAFETY: `self` is a valid `Exceptionate`.
        unsafe {
            cpp_exceptionate_shutdown(self as *const Self as *mut Self);
        }
    }
}
