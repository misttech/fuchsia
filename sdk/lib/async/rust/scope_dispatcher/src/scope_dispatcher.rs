// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ptr::NonNull;
use fuchsia_async::{EHandle, Scope};
use libasync_dispatcher::{AsAsyncDispatcherRef, AsyncDispatcherRef};
use libasync_sys::async_dispatcher_t;
use std::sync::Arc;

use crate::ops;

/// Implements a C++-compatible [`async_dispatcher_t`] around a [`fuchsia_async::Scope`].
#[derive(Debug)]
#[repr(C)]
pub struct ScopeDispatcher {
    // Safety Note: this must go first in this struct for the callbacks to work correctly.
    dispatcher: async_dispatcher_t,
    executor: EHandle,
    scope: Scope,
}

// SAFETY: The API of async_dispatcher_t is expected to be thread safe.
unsafe impl Send for ScopeDispatcher {}
// SAFETY: The API of async_dispatcher_t is expected to be thread safe.
unsafe impl Sync for ScopeDispatcher {}

impl ScopeDispatcher {
    /// Creates a new [`ScopeDispatcher`] on the currently running fuchsia-async executor.
    ///
    /// # Panics
    ///
    /// Panics if this is not run on a fuchsia-async executor context.
    pub fn new() -> Arc<Self> {
        Self::new_on_executor(EHandle::local())
    }

    /// Creates a new [`ScopeDispatcher`] with a new [`Scope`] on the given `executor`.
    pub fn new_on_executor(executor: EHandle) -> Arc<Self> {
        let scope = executor.global_scope().new_child();
        let dispatcher = async_dispatcher_t { ops: &ops::ASYNC_OPS };
        Arc::new(Self { dispatcher, executor, scope })
    }

    /// Get the pointer to the dispatcher callback struct for passing through FFI layers.
    pub fn as_ptr(&self) -> *const async_dispatcher_t {
        (self as *const Self).cast()
    }

    /// Gets the global executor handle for this dispatcher.
    pub fn global_handle(&self) -> &EHandle {
        &self.executor
    }

    /// Gets the [`fuchsia_async::Scope`] of this dispatcher.
    pub fn as_scope(&self) -> &Scope {
        &self.scope
    }

    /// Gets the Scope from a dispatcher pointer. Used in the callbacks.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the dispatcher pointer is a valid pointer originally obtained
    /// through [`ScopeDispatcher::as_ptr`], and is still alive.
    pub(crate) unsafe fn from_ptr<'a>(dispatcher_ptr: *mut async_dispatcher_t) -> &'a Self {
        let this = dispatcher_ptr.cast::<ScopeDispatcher>();
        // Safety: the caller promises that this is a valid pointer to what was originally a
        // ScopeDispatcher object.
        unsafe { this.as_ref() }.expect("null dispatcher pointer")
    }
}

impl AsAsyncDispatcherRef for ScopeDispatcher {
    fn as_async_dispatcher_ref(&self) -> AsyncDispatcherRef<'_> {
        // SAFETY: We know this pointer is valid because it is a member of `&self`, which is a valid
        // reference.
        let ptr = unsafe { NonNull::new_unchecked(self.as_ptr().cast_mut()) };
        // SAFETY: The dispatcher ref's lifetime is tied to `self`, of which the dispatcher
        // structure and callbacks are members, so will not outlive them.
        unsafe { AsyncDispatcherRef::from_raw(ptr) }
    }
}
