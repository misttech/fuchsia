// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::handle::{HandleValue, KernelHandle};
use crate::process_dispatcher_ffi::{
    cpp_process_dispatcher_current, cpp_process_dispatcher_make_and_add_handle,
};
use zx_status::Status;
use zx_types::zx_rights_t;

#[repr(C)]
pub struct ProcessDispatcher {
    _facade: fbl::OpaqueRefCountedFacade<crate::dispatcher::Dispatcher>,
}

impl ProcessDispatcher {
    /// Executes the given function with a reference to the current process.
    pub fn with_current<R>(f: impl FnOnce(&ProcessDispatcher) -> R) -> R {
        // SAFETY: The current process is guaranteed to be valid for the duration of the call.
        let proc = unsafe { &*cpp_process_dispatcher_current() };
        f(proc)
    }

    /// Creates a handle for the given dispatcher in this process's handle table.
    pub fn make_and_add_handle<T>(
        &self,
        handle: KernelHandle<T>,
        rights: zx_rights_t,
    ) -> Result<HandleValue, Status>
    where
        T: fbl::HasRefCount + fbl::Recyclable + crate::DispatcherOps,
    {
        let mut handle = handle.cast();
        let mut out = HandleValue::default();
        let status = unsafe {
            cpp_process_dispatcher_make_and_add_handle(
                self as *const _,
                &mut handle,
                rights,
                &mut out,
            )
        };
        Status::ok(status)?;
        Ok(out)
    }
}
