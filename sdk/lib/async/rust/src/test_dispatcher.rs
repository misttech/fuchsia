// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::pin::{Pin, pin};
use core::ptr::{NonNull, addr_of_mut};
use core::task::{Context, Poll, Waker};
use std::collections::VecDeque;
use std::sync::Mutex;

use libasync_sys::{
    async_dispatcher_t, async_ops_t, async_ops_v1, async_ops_v2, async_ops_v3, async_state_t,
    async_task_t,
};
use zx::sys::{ZX_ERR_CANCELED, ZX_ERR_NOT_FOUND, ZX_OK, ZX_TIME_INFINITE, zx_status_t};

use crate::{AsyncDispatcher, AsyncDispatcherRef};

#[derive(Default)]
struct Tasks {
    pending: VecDeque<*mut async_task_t>,
    ready: VecDeque<*mut async_task_t>,
}

#[repr(C)]
pub struct TestDispatcher {
    async_dispatcher: async_dispatcher_t,
    tasks: Mutex<Tasks>,
}

// SAFETY: `TestDispatcher` is safe to send between threads.
unsafe impl Send for TestDispatcher {}
// SAFETY: `TestDispatcher` is safe to share between threads.
unsafe impl Sync for TestDispatcher {}

impl Drop for TestDispatcher {
    fn drop(&mut self) {
        let mut tasks = core::mem::take(&mut *self.tasks.lock().unwrap());

        for task in tasks.ready.drain(..) {
            // SAFETY: The API of libasync guarantees that `task` is valid to
            // dereference as long as it has been posted to the dispatcher and
            // has not been successfully canceled.
            let handler = unsafe { (*task).handler };
            // SAFETY:
            // - `handler` is guaranteed to point to a valid callback
            // - `self.as_dispatcher()` is a valid `async_dispatcher_t`
            // - `task` is the task the callback was provided for
            unsafe {
                (handler.unwrap())(self.as_dispatcher(), task, ZX_ERR_CANCELED);
            }
        }

        for task in tasks.pending.drain(..) {
            // SAFETY: The API of libasync guarantees that `task` is valid to
            // dereference as long as it has been posted to the dispatcher and
            // has not been successfully canceled.
            let handler = unsafe { (*task).handler };
            // SAFETY:
            // - `handler` is guaranteed to point to a valid callback
            // - `self.as_dispatcher()` is a valid `async_dispatcher_t`
            // - `task` is the task the callback was provided for
            unsafe {
                (handler.unwrap())(self.as_dispatcher(), task, ZX_ERR_CANCELED);
            }
        }
    }
}

impl TestDispatcher {
    const OPS: async_ops_t = async_ops_t {
        version: 1,
        reserved: 0,
        v1: async_ops_v1 {
            now: None,
            begin_wait: None,
            cancel_wait: None,
            post_task: Some(Self::post_task),
            cancel_task: Some(Self::cancel_task),
            queue_packet: None,
            set_guest_bell_trap: None,
        },
        v2: async_ops_v2 {
            bind_irq: None,
            unbind_irq: None,
            create_paged_vmo: None,
            detach_paged_vmo: None,
        },
        v3: async_ops_v3 { get_sequence_id: None, check_sequence_id: None },
    };

    unsafe extern "C" fn post_task(
        dispatcher: *mut async_dispatcher_t,
        task: *mut async_task_t,
    ) -> zx_status_t {
        // SAFETY: The API of libasync guarantees that `post_task` will only
        // ever be called with a valid `TestDispatcher`.
        let this = unsafe { &*dispatcher.cast::<Self>() };
        let mut tasks = this.tasks.lock().unwrap();

        // Perform a spurious write to `state` so that we trigger MIRI if
        // appropriate

        // SAFETY: `task` is guaranteed to be valid to dereference per the API
        // of libasync.
        let state = unsafe { addr_of_mut!((*task).state) };
        // SAFETY: We may write to `state` because it may only be written to by
        // the dispatcher per the API of libasync.
        unsafe {
            state.write(async_state_t { reserved: [0; 2] });
        }

        assert!(!tasks.pending.contains(&task));
        assert!(!tasks.ready.contains(&task));

        // SAFETY: `task` is guaranteed to be valid to dereference per the API
        // of libasync.
        let deadline = unsafe { (*task).deadline };
        if deadline == ZX_TIME_INFINITE {
            tasks.pending.push_back(task);
        } else {
            tasks.ready.push_back(task);
        }

        ZX_OK
    }

    unsafe extern "C" fn cancel_task(
        dispatcher: *mut async_dispatcher_t,
        task: *mut async_task_t,
    ) -> zx_status_t {
        // SAFETY: The API of libasync guarantees that `post_task` will only
        // ever be called with a valid `TestDispatcher`.
        let this = unsafe { &*dispatcher.cast::<Self>() };
        let mut tasks = this.tasks.lock().unwrap();

        // Perform a spurious write to `state` so that we trigger MIRI if
        // appropriate

        // SAFETY: `task` is guaranteed to be valid to dereference per the API
        // of libasync.
        let state = unsafe { addr_of_mut!((*task).state) };
        // SAFETY: We may write to `state` because it may only be written to by
        // the dispatcher per the API of libasync.
        unsafe {
            state.write(async_state_t { reserved: [0; 2] });
        }

        if let Some(position) = tasks.pending.iter().position(|t| *t == task) {
            tasks.pending.swap_remove_back(position);
            ZX_OK
        } else if let Some(position) = tasks.ready.iter().position(|t| *t == task) {
            tasks.ready.swap_remove_back(position);
            ZX_OK
        } else {
            ZX_ERR_NOT_FOUND
        }
    }

    fn as_dispatcher(&self) -> *mut async_dispatcher_t {
        (self as *const Self).cast_mut().cast()
    }

    /// Returns a new `TestDispatcher`.
    pub fn new() -> Self {
        Self {
            async_dispatcher: async_dispatcher_t { ops: &Self::OPS },
            tasks: Mutex::new(Tasks::default()),
        }
    }

    /// Runs the given future as a task until it finishes or the dispatcher runs
    /// out of work to do.
    ///
    /// Returns `Some` of the output if the task finished, or `None` if the
    /// dispatcher ran out of work to do before the task finished.
    pub fn run_until_stalled<F>(self: Pin<&mut Self>, future: F) -> Option<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        let mut context = Context::from_waker(Waker::noop());
        let mut future = pin!(future);

        loop {
            if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
                return Some(result);
            }

            let task = self.tasks.lock().unwrap().ready.pop_front()?;

            // SAFETY:
            // - `handler` is guaranteed to point to a valid callback
            // - `self.as_dispatcher()` is a valid `async_dispatcher_t`
            // - `task` is the task the callback was provided for
            unsafe {
                ((*task).handler.unwrap())(self.as_dispatcher(), task, ZX_OK);
            }
        }
    }

    /// Runs the given future as a task until it finishes.
    ///
    /// Returns the output of the task. Panics if the dispatcher runs out of
    /// work before the task finishes.
    pub fn run_to_completion<F>(self: Pin<&mut Self>, future: F) -> F::Output
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.run_until_stalled(future).expect("future did not complete before the executor stalled")
    }
}

impl AsyncDispatcher for Pin<&TestDispatcher> {
    fn as_async_dispatcher_ref(&self) -> AsyncDispatcherRef<'_> {
        // SAFETY: `TestDispatcher` is pinned and so a pointer to it will remain
        // valid for the duration of its lifetime.
        unsafe { AsyncDispatcherRef::from_raw(NonNull::new_unchecked(self.as_dispatcher())) }
    }
}

impl AsyncDispatcher for Pin<&mut TestDispatcher> {
    fn as_async_dispatcher_ref(&self) -> AsyncDispatcherRef<'_> {
        // SAFETY: `TestDispatcher` is pinned and so a pointer to it will remain
        // valid for the duration of its lifetime.
        unsafe { AsyncDispatcherRef::from_raw(NonNull::new_unchecked(self.as_dispatcher())) }
    }
}
