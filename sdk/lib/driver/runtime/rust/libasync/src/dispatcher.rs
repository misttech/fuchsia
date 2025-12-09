// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe bindings for the C libasync async dispatcher library

use libasync_sys::*;
use zx::sys::ZX_OK;

use core::cell::UnsafeCell;
use core::future::Future;
use core::marker::PhantomData;
use core::ptr::NonNull;
use core::task::Context;
use fuchsia_sync::Mutex;
use std::sync::{Arc, Weak};

use zx::Status;

use futures::future::{BoxFuture, FutureExt};
use futures::task::{ArcWake, waker_ref};

mod after_deadline;

pub use after_deadline::*;

/// An unowned reference to a driver runtime dispatcher such as is produced by calling
/// [`AsyncDispatcher::release`]. When this object goes out of scope it won't shut down the dispatcher,
/// leaving that up to the driver runtime or another owner.
#[derive(Debug)]
pub struct AsyncDispatcherRef<'a>(NonNull<async_dispatcher_t>, PhantomData<&'a async_dispatcher_t>);

unsafe impl<'a> Send for AsyncDispatcherRef<'a> {}
unsafe impl<'a> Sync for AsyncDispatcherRef<'a> {}

impl<'a> AsyncDispatcherRef<'a> {
    /// Creates a dispatcher ref from a raw ptr.
    ///
    /// # Safety
    ///
    /// Caller is responsible for ensuring that the given ptr is valid for
    /// the lifetime `'a`.
    pub unsafe fn from_raw(ptr: NonNull<async_dispatcher_t>) -> Self {
        // SAFETY: Caller promises the ptr is valid.
        Self(ptr, PhantomData)
    }

    /// Gets the inner pointer to the dispatcher struct.
    pub fn inner(&self) -> NonNull<async_dispatcher_t> {
        self.0
    }
}

impl<'a> Clone for AsyncDispatcherRef<'a> {
    fn clone(&self) -> Self {
        Self(self.0, PhantomData)
    }
}

/// A trait for things that can be represented as an [`AsyncDispatcherRef`].
pub trait AsyncDispatcher: Send + Sync {
    /// Gets an [`AsyncDispatcherRef`] corresponding to this object.
    fn as_async_dispatcher_ref(&self) -> AsyncDispatcherRef<'_>;

    /// Schedules the callback [`p`] to be run on this dispatcher later.
    fn post_task_sync(&self, p: impl TaskCallback) -> Result<(), Status> {
        #[expect(clippy::arc_with_non_send_sync)]
        let task_arc = Arc::new(UnsafeCell::new(TaskFunc {
            task: async_task { handler: Some(TaskFunc::call), ..Default::default() },
            func: Box::new(p),
        }));

        let task_cell = Arc::into_raw(task_arc);
        // SAFETY: we need a raw mut pointer to give to async_post_task. From
        // when we call that function to when the task is cancelled or the
        // callback is called, the driver runtime owns the contents of that
        // object and we will not manipulate it. So even though the Arc only
        // gives us a shared reference, it's fine to give the runtime a
        // mutable pointer to it.
        let res = unsafe {
            let task_ptr = &raw mut (*UnsafeCell::raw_get(task_cell)).task;
            Status::ok(async_post_task(self.as_async_dispatcher_ref().0.as_ptr(), task_ptr))
        };
        if res.is_err() {
            // SAFETY: `TaskFunc::call` will never be called now so dispose of
            // the long-lived reference we just created.
            unsafe { Arc::decrement_strong_count(task_cell) }
        }
        res
    }

    /// Returns the current time on the dispatcher's timeline
    fn now(&self) -> zx::MonotonicInstant {
        let async_dispatcher = self.as_async_dispatcher_ref().0.as_ptr();
        let now_nanos = unsafe { async_now(async_dispatcher) };
        zx::MonotonicInstant::from_nanos(now_nanos)
    }
}

impl<'a> AsyncDispatcher for AsyncDispatcherRef<'a> {
    fn as_async_dispatcher_ref(&self) -> AsyncDispatcherRef<'_> {
        self.clone()
    }
}

/// A trait that can be used to access a lifetime-constrained dispatcher in a generic way.
pub trait OnDispatcher: Clone + Send + Sync {
    /// Runs the function `f` with a lifetime-bound [`AsyncDispatcherRef`] for this object's dispatcher.
    /// If the dispatcher is no longer valid, the callback will be given [`None`].
    ///
    /// Note that it is *very important* that no blocking work be done in this callback to prevent
    /// long lived strong references to dispatchers that might be shutting down.
    fn on_dispatcher<R>(&self, f: impl FnOnce(Option<AsyncDispatcherRef<'_>>) -> R) -> R;

    /// Helper version of [`OnDispatcher::on_dispatcher`] that translates an invalidated dispatcher
    /// handle into a [`Status::BAD_STATE`] error instead of giving the callback [`None`].
    ///
    /// Note that it is *very important* that no blocking work be done in this callback to prevent
    /// long lived strong references to dispatchers that might be shutting down.
    fn on_maybe_dispatcher<R, E: From<Status>>(
        &self,
        f: impl FnOnce(AsyncDispatcherRef<'_>) -> Result<R, E>,
    ) -> Result<R, E> {
        self.on_dispatcher(|dispatcher| {
            let dispatcher = dispatcher.ok_or(Status::BAD_STATE)?;
            f(dispatcher)
        })
    }

    /// Spawn an asynchronous task on this dispatcher. If this returns [`Ok`] then the task
    /// has successfully been scheduled and will run or be cancelled and dropped when the dispatcher
    /// shuts down.
    fn spawn_task(&self, future: impl Future<Output = ()> + Send + 'static) -> Result<(), Status>
    where
        Self: 'static,
    {
        let task =
            Arc::new(Task { future: Mutex::new(Some(future.boxed())), dispatcher: self.clone() });
        task.queue()
    }

    /// Returns a future that will fire when after the given deadline time.
    ///
    /// This can be used instead of the fuchsia-async timer primitives in situations where
    /// there isn't a currently active fuchsia-async executor running on that dispatcher for some
    /// reason (ie. the rust code does not own the dispatcher) or for cases where the small overhead
    /// of fuchsia-async compatibility is too much.
    fn after_deadline(&self, deadline: zx::MonotonicInstant) -> AfterDeadline<Self> {
        AfterDeadline::new(self, deadline)
    }
}

impl<D: AsyncDispatcher> OnDispatcher for &D {
    fn on_dispatcher<R>(&self, f: impl FnOnce(Option<AsyncDispatcherRef<'_>>) -> R) -> R {
        f(Some(D::as_async_dispatcher_ref(*self)))
    }
}

impl<'a> OnDispatcher for AsyncDispatcherRef<'a> {
    fn on_dispatcher<R>(&self, f: impl FnOnce(Option<AsyncDispatcherRef<'_>>) -> R) -> R {
        f(Some(self.clone()))
    }
}

impl<T: AsyncDispatcher> OnDispatcher for Arc<T> {
    fn on_dispatcher<R>(&self, f: impl FnOnce(Option<AsyncDispatcherRef<'_>>) -> R) -> R {
        f(Some(self.as_async_dispatcher_ref()))
    }
}

impl<T: AsyncDispatcher> OnDispatcher for Weak<T> {
    fn on_dispatcher<R>(&self, f: impl FnOnce(Option<AsyncDispatcherRef<'_>>) -> R) -> R {
        let dispatcher = Weak::upgrade(self);
        match dispatcher {
            Some(dispatcher) => f(Some(dispatcher.as_async_dispatcher_ref())),
            None => f(None),
        }
    }
}

/// A marker trait for a callback that can be used with [`Dispatcher::post_task_sync`].
pub trait TaskCallback: FnOnce(Status) + 'static + Send {}
impl<T> TaskCallback for T where T: FnOnce(Status) + 'static + Send {}

struct Task<D> {
    future: Mutex<Option<BoxFuture<'static, ()>>>,
    dispatcher: D,
}

impl<D: OnDispatcher + 'static> ArcWake for Task<D> {
    fn wake_by_ref(arc_self: &Arc<Self>) {
        match arc_self.queue() {
            Err(e) if e == Status::BAD_STATE => {
                // the dispatcher is shutting down so drop the future, if there
                // is one, to cancel it.
                let future_slot = arc_self.future.lock().take();
                core::mem::drop(future_slot);
            }
            res => res.expect("Unexpected error waking dispatcher task"),
        }
    }
}

impl<D: OnDispatcher + 'static> Task<D> {
    /// Posts a task to progress the currently stored future. The task will
    /// consume the future if the future is ready after the next poll.
    /// Otherwise, the future is kept to be polled again after being woken.
    fn queue(self: &Arc<Self>) -> Result<(), Status> {
        let arc_self = self.clone();
        self.dispatcher.on_maybe_dispatcher(move |dispatcher| {
            dispatcher
                .post_task_sync(move |status| {
                    let mut future_slot = arc_self.future.lock();
                    // if we're cancelled, drop the future we're waiting on.
                    if status != Status::from_raw(ZX_OK) {
                        core::mem::drop(future_slot.take());
                        return;
                    }

                    let Some(mut future) = future_slot.take() else {
                        return;
                    };
                    let waker = waker_ref(&arc_self);
                    let context = &mut Context::from_waker(&waker);
                    if future.as_mut().poll(context).is_pending() {
                        *future_slot = Some(future);
                    }
                })
                .map(|_| ())
        })
    }
}

#[repr(C)]
struct TaskFunc {
    task: async_task,
    func: Box<dyn TaskCallback>,
}

impl TaskFunc {
    extern "C" fn call(_dispatcher: *mut async_dispatcher, task: *mut async_task, status: i32) {
        // SAFETY: the async api promises that this function will only be called
        // up to once, so we can reconstitute the `Arc` and let it get dropped.
        let task = unsafe { Arc::from_raw(task as *const UnsafeCell<Self>) };
        // SAFETY: if we can't get a mut ref from the arc, then the task is already
        // being cancelled, so we don't want to call it.
        if let Ok(task) = Arc::try_unwrap(task) {
            (task.into_inner().func)(Status::from_raw(status));
        }
    }
}
