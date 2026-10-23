// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::channel_dispatcher::ChannelDispatcher;
use super::exceptionate::Exceptionate;
use super::handle::KernelHandle;
use zx_types::{zx_rights_t, zx_status_t};

#[allow(improper_ctypes)]
unsafe extern "C" {
    /// Initializes an `Exceptionate` in-place within `storage`.
    ///
    /// # Safety
    ///
    /// `storage` must point to valid, aligned, uninitialized memory for `Exceptionate`.
    pub(crate) fn cpp_exceptionate_init(storage: *mut core::ffi::c_void, exception_type: u32);

    /// Destroys an `Exceptionate` in-place within `storage`.
    ///
    /// # Safety
    ///
    /// `storage` must point to an initialized `Exceptionate` that will not be used again.
    pub(crate) fn cpp_exceptionate_destroy(storage: *mut core::ffi::c_void);

    /// Sets the backing `ChannelDispatcher` endpoint on `exceptionate`.
    ///
    /// # Safety
    ///
    /// `exceptionate` must point to a valid `Exceptionate`, and `channel` must point to a valid
    /// `KernelHandle<ChannelDispatcher>`.
    pub(crate) fn cpp_exceptionate_set_channel(
        exceptionate: *mut Exceptionate,
        channel: *mut KernelHandle<ChannelDispatcher>,
        thread_rights: zx_rights_t,
        process_rights: zx_rights_t,
    ) -> zx_status_t;

    /// Shuts down the exceptionate.
    ///
    /// # Safety
    ///
    /// `exceptionate` must point to a valid `Exceptionate`.
    pub(crate) fn cpp_exceptionate_shutdown(exceptionate: *mut Exceptionate);
}
