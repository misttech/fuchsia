// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::DispatcherOps;
use fbl::{HasRefCount, Recyclable, RefPtr};
use zx_status::Status;
use zx_types::{zx_handle_t, zx_rights_t};

/// A wrapper around a handle value received from userspace.
#[repr(transparent)]
#[derive(Debug, Copy, Clone, Default, PartialEq, Eq)]
pub struct HandleValue {
    value: zx_handle_t,
}

impl HandleValue {
    /// Constructs a new `HandleValue` from a raw handle value.
    pub const fn new(value: zx_handle_t) -> Self {
        Self { value }
    }

    /// Returns the underlying raw handle value.
    pub fn raw_value(&self) -> zx_handle_t {
        self.value
    }
}

#[repr(transparent)]
pub struct KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    ptr: *const T,
}

impl<T> KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    pub fn new(dispatcher: RefPtr<T>) -> Self {
        Self { ptr: RefPtr::into_raw(dispatcher) }
    }

    /// Casts this handle to a generic Dispatcher handle.
    pub fn cast(self) -> KernelHandle<crate::dispatcher::Dispatcher> {
        let ptr = self.ptr.cast::<crate::dispatcher::Dispatcher>();
        core::mem::forget(self);
        KernelHandle { ptr }
    }

    pub fn release(mut self) -> RefPtr<T> {
        let ref_ptr = self.take_ref_ptr().expect("KernelHandle was empty");
        core::mem::forget(self);
        ref_ptr
    }

    fn take_ref_ptr(&mut self) -> Option<RefPtr<T>> {
        let ptr = core::mem::replace(&mut self.ptr, core::ptr::null());
        if ptr.is_null() {
            None
        } else {
            // SAFETY: ptr came from RefPtr::into_raw.
            Some(unsafe { RefPtr::from_raw(ptr) })
        }
    }

    pub fn dispatcher(&self) -> &T {
        assert!(!self.ptr.is_null());
        // SAFETY: We are holding a reference to the object, which ensures that it lives as long as
        // we do.
        unsafe { &*self.ptr }
    }

    pub fn make_and_add_handle(self, rights: zx_rights_t) -> Result<HandleValue, Status> {
        crate::process_dispatcher::ProcessDispatcher::with_current(|up| {
            up.make_and_add_handle(self, rights)
        })
    }
}

impl<T> Drop for KernelHandle<T>
where
    T: HasRefCount + Recyclable + DispatcherOps,
{
    fn drop(&mut self) {
        if let Some(ref_ptr) = self.take_ref_ptr() {
            ref_ptr.on_zero_handles();
        }
    }
}
