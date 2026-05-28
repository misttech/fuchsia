// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use libasync_sys::*;

use core::ptr::null_mut;
use core::sync::atomic::{AtomicPtr, Ordering};
use std::sync::{Arc, Weak};

use zx_status::Status;

use crate::AsyncDispatcherRef;

/// Holds an [`Arc`] that has a lifetime tied to a specific dispatcher through a callback registered
/// against the dispatcher.
#[repr(C)]
pub struct DispatcherArc<T> {
    // The position of this struct is significant as it is used as the base pointer to
    // re-derive the original object from the shutdown callback.
    shutdown_task: async_task,
    arc: Arc<T>,
}

/// A trait that must be implemented on the inner type of a [`DispatcherArc`] so that it can prepare
/// its value for the dispatcher shutting down (by invalidating a pointer, for example).
pub trait DispatcherShutdown {
    /// This will be called when the dispatcher is shutting down, before the inner value is dropped.
    /// The intended purpose of this callback is to allow for preempting further attempts to
    /// obtain a strong reference to the inner object by replacing it with a sentinel value (ie.
    /// setting an [`AtomicPtr`] to [`core::mem::null_mut`], which is implemented by default).
    fn shutting_down(&self);
}

/// Implements shutting down by replacing the atomic pointer with a null pointer using
/// [`Ordering::Release`] ordering. The user of this value should use [`Ordering::Acquire`]
/// ordering to access it.
impl<T> DispatcherShutdown for AtomicPtr<T> {
    fn shutting_down(&self) {
        self.store(null_mut(), Ordering::Release);
    }
}

impl<T: DispatcherShutdown> DispatcherArc<T> {
    /// Creates a new DispatcherArc that can have its lifetime bound by the lifetime of a dispatcher
    /// through a shutdown callback.
    pub fn new(inner: T) -> Self {
        Self {
            shutdown_task: async_task {
                state: Default::default(),
                handler: Some(Self::shutdown),
                deadline: zx_types::ZX_TIME_INFINITE,
            },
            arc: Arc::new(inner),
        }
    }

    /// Posts the shutdown task to the dispatcher and returns a [`Weak`] reference to the inner
    /// object that will only resolve as long as the dispatcher is alive.
    ///
    /// The caller should be careful not to hold a strong [`Arc`] to this value for long periods of
    /// time, as the object may not be able to be [`Drop`]ed until all other strong references are
    /// released.
    pub fn into_weak(self, dispatcher: AsyncDispatcherRef<'_>) -> Weak<T> {
        let weak = Arc::downgrade(&self.arc);
        let shutdown_task_ptr = Box::into_raw(Box::new(self)).cast();
        // SAFETY: If the task posting succeeds, the callback will be called exactly once, when the
        // dispatcher is shutting down, so the box we pass will only be accessed again in that
        // callback where it will be dropped. If it fails we will free it below.
        let res = unsafe { Status::ok(async_post_task(dispatcher.0.as_ptr(), shutdown_task_ptr)) };
        if res.is_err() {
            // SAFETY: `TaskFunc::call` will never be called now so dispose of
            // the long-lived reference we just created.
            drop(unsafe { Box::from_raw(shutdown_task_ptr) });
        }
        weak
    }

    extern "C" fn shutdown(_dispatcher: *mut async_dispatcher, task: *mut async_task, status: i32) {
        // this should only ever be called on dispatcher shutdown, as we set our timeout to the
        // infinite future.
        assert_eq!(status, zx_types::ZX_ERR_CANCELED);
        // SAFETY: the async api promises that this function will only be called up to once,
        // and the task pointer will point to the object we originally gave it, so we can safely
        // take ownership of the data in that pointer as our original `DispatcherArc` object.
        let this: Box<Self> = unsafe { Box::from_raw(task.cast()) };
        // spin-loop until we obtain exclusive ownership of the Arc and unwrap it.
        let mut arc = this.arc;
        arc.shutting_down();
        loop {
            arc = match Arc::try_unwrap(arc) {
                Ok(_inner) => return, // drop the inner value and exit the loop.
                Err(arc) => arc,      // continue the loop
            };
            core::hint::spin_loop();
        }
    }
}
