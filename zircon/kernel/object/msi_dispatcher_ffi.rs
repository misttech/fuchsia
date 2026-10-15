// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::handle::KernelHandle;
use super::msi_allocation::MsiAllocation;
use super::msi_dispatcher::{MsiDispatcher, MsiDispatcherState};
use core::mem::MaybeUninit;
use zx_types::zx_status_t;

// C++ FFI declarations
unsafe extern "C" {
    // `MsiAllocation` is passed as an opaque pointer through C++ to
    // `rust_msi_dispatcher_state_init`; C++ never inspects its internal `fn` field.
    #[allow(improper_ctypes)]
    pub(crate) fn cpp_msi_dispatcher_create(
        msi_alloc: *mut MsiAllocation,
        handle_out: *mut MaybeUninit<KernelHandle<MsiDispatcher>>,
    ) -> zx_status_t;
}

// FFI trampolines for C++ calling into Rust MsiDispatcherState
crate::object::dispatcher::impl_dispatcher_state_init!(
    MsiDispatcher,
    MsiDispatcherState,
    msi_alloc_raw: *mut MsiAllocation,
);
