// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::pin::Pin;
use std::ptr::NonNull;
use std::sync::Arc;
use std::sync::atomic::{AtomicI32, Ordering};
use std::task::{Context, Poll};

use libasync_sys::{async_cancel_task, async_dispatcher, async_post_task, async_task};

use futures::task::AtomicWaker;
use zx::Status;
use zx::sys::{ZX_ERR_CANCELED, ZX_OK};

use crate::callback_state::CallbackSharedState;
use crate::{AsAsyncDispatcherRef, OnDispatcher};

type SharedState = CallbackSharedState<async_task, AfterDeadlineState>;

/// Implements methods used for setting and waiting on timers on a dispatcher.
pub trait DispatcherTimerExt: OnDispatcher {
    /// Returns a future that will fire when after the given deadline time.
    ///
    /// This can be used instead of the fuchsia-async timer primitives in situations where
    /// there isn't a currently active fuchsia-async executor running on that dispatcher for some
    /// reason (ie. the rust code does not own the dispatcher) or for cases where the small overhead
    /// of fuchsia-async compatibility is too much.
    fn after_deadline(&self, deadline: zx::MonotonicInstant) -> AfterDeadline<Self>;
}

impl<T> DispatcherTimerExt for T
where
    T: OnDispatcher,
{
    fn after_deadline(&self, deadline: zx::MonotonicInstant) -> AfterDeadline<Self> {
        AfterDeadline::new(self, deadline)
    }
}

struct AfterDeadlineState {
    async_dispatcher: NonNull<async_dispatcher>,
    waker: AtomicWaker,
    /// The status will initially be [`Status::SHOULD_WAIT`]. Once fired it will be the status
    /// returned by the callback.
    status: AtomicI32,
}

// SAFETY: All fields in AfterDeadlineState are either atomic or immutable.
unsafe impl Send for AfterDeadlineState {}
unsafe impl Sync for AfterDeadlineState {}

impl AfterDeadlineState {
    extern "C" fn call(_dispatcher: *mut async_dispatcher, task: *mut async_task, status: i32) {
        debug_assert!(
            status == ZX_OK || status == ZX_ERR_CANCELED,
            "task callback called with status other than ok or canceled"
        );
        // SAFETY: This callback's copy of the `async_task` object was refcounted for when we
        // started the wait.
        let state = unsafe { SharedState::from_raw_ptr(task) };
        state.status.store(status, Ordering::Relaxed);
        state.waker.wake();
    }
}

/// A future that represents a deferral to a future time.
///
/// See [`OnDispatcher::after_deadline`] for more information.
pub struct AfterDeadline<D: OnDispatcher> {
    dispatcher: D,
    state: Option<Arc<SharedState>>,
    deadline: zx::MonotonicInstant,
}

impl<D: OnDispatcher + Clone> AfterDeadline<D> {
    pub(super) fn new(dispatcher: &D, deadline: zx::MonotonicInstant) -> Self {
        let dispatcher = dispatcher.clone();
        let state = None;
        Self { dispatcher, state, deadline }
    }
}

impl<D: OnDispatcher + Unpin> Future for AfterDeadline<D> {
    type Output = Result<(), Status>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // if we've already spawned a task then return based on the task's state.
        if let Some(state) = &self.state {
            let status = state.status.load(Ordering::Relaxed);
            if status != Status::SHOULD_WAIT.into_raw() {
                return Poll::Ready(Status::ok(status));
            } else {
                state.waker.register(cx.waker());
                return Poll::Pending;
            }
        }

        let deadline = self.deadline;

        let now = self.dispatcher.on_maybe_dispatcher(|dispatcher| Ok(dispatcher.now()));
        match now {
            Ok(now) if deadline < zx::MonotonicInstant::from_nanos(now) => {
                return Poll::Ready(Ok(()));
            }
            Err(err) => {
                return Poll::Ready(Err(err));
            }
            _ => {}
        }

        // otherwise we want to wait for a callback
        let state = self.dispatcher.on_maybe_dispatcher(move |dispatcher| {
            // SAFETY: the fdf dispatcher is valid by construction and can provide an async dispatcher.
            let async_dispatcher = dispatcher.inner();

            let task = async_task {
                handler: Some(AfterDeadlineState::call),
                deadline: deadline.into_nanos(),
                ..Default::default()
            };
            let state = AfterDeadlineState {
                async_dispatcher,
                waker: AtomicWaker::new(),
                status: AtomicI32::new(Status::SHOULD_WAIT.into_raw()),
            };
            let state = SharedState::new(task, state);
            state.waker.register(cx.waker());

            let state_ptr = SharedState::make_raw_ptr(state.clone());

            // SAFETY: We know the `async_dispatcher` is valid because we're running inside
            // `on_dispatcher` and we are giving ownership of the shared state object to the
            // callback.
            let res = Status::ok(unsafe { async_post_task(async_dispatcher.as_ptr(), state_ptr) });
            match res {
                Ok(_) => Ok(state),
                Err(err) => {
                    // SAFETY: Posting the task failed, so we now have an outstanding reference to
                    // the state object that will never have a callback called on it.
                    unsafe { SharedState::release_raw_ptr(state_ptr) };
                    Err(err)
                }
            }
        });

        match state {
            Ok(state) => {
                self.state = Some(state);
                Poll::Pending
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }
}

impl<D: OnDispatcher> Drop for AfterDeadline<D> {
    fn drop(&mut self) {
        let Some(state) = self.state.take() else {
            // if we never spawned a task we can just return.
            return;
        };
        self.dispatcher.on_dispatcher(|dispatcher| {
            let Some(dispatcher) = dispatcher else {
                // if the dispatcher is no longer alive then the callback will have been
                // called with ZX_ERR_CANCELED and we can assume that freed the callback's
                // Arc.
                return;
            };
            if state.status.load(Ordering::Relaxed) != Status::SHOULD_WAIT.into_raw() {
                // the callback has been called so we don't even need to try to cancel it.
                return;
            }
            let async_dispatcher = dispatcher.inner();
            if async_dispatcher != state.async_dispatcher {
                panic!("Dropping a pending `AfterDeadline` future from a different dispatcher than the one it was awaited on.");
            }
            let state_ptr = SharedState::as_raw_ptr(&state);
            // SAFETY: We know that the current async dispatcher is valid because we are running
            // inside `on_dispatcher`, and we know the `state_ptr` is valid because the `Arc`
            // holding it is still held.
            let status = unsafe { async_cancel_task(async_dispatcher.as_ptr(), state_ptr) };
            if Status::from_raw(status) == Status::OK {
                // SAFETY: If the cancellation was successful, we know the callback won't be called
                // so we need to deallocate the copy of the arc that was given to it.
                unsafe { SharedState::release_raw_ptr(state_ptr) };
            }
        });
    }
}

#[cfg(all(not_yet, test))]
mod tests {
    use std::sync::mpsc;
    use std::thread::sleep;
    use std::time::Duration;

    use super::*;

    use futures::{FutureExt, poll};
    use std::task::Waker;

    use crate::dispatcher::tests::with_raw_dispatcher;
    use crate::dispatcher::{CurrentDispatcher, OnDispatcher};

    #[test]
    fn after_the_past() {
        with_raw_dispatcher("testing task", |dispatcher| {
            let (tx, rx) = mpsc::channel();
            dispatcher
                .spawn_task(async move {
                    let fut = CurrentDispatcher.after_deadline(zx::MonotonicInstant::INFINITE_PAST);
                    assert_eq!(poll!(fut), Poll::Ready(Ok(())));
                    tx.send(()).unwrap();
                })
                .unwrap();
            rx.recv().unwrap();
        });
    }

    #[test]
    fn after_now() {
        with_raw_dispatcher("testing task", |dispatcher| {
            let (tx, rx) = mpsc::channel();
            dispatcher
                .spawn_task(async move {
                    let fut = CurrentDispatcher.after_deadline(CurrentDispatcher.now().unwrap());
                    assert_eq!(poll!(fut), Poll::Ready(Ok(())));
                    tx.send(()).unwrap();
                })
                .unwrap();
            rx.recv().unwrap();
        });
    }

    #[test]
    fn after_future() {
        with_raw_dispatcher("testing task", |dispatcher| {
            let (tx, rx) = mpsc::channel();
            dispatcher
                .spawn_task(async move {
                    let deadline =
                        CurrentDispatcher.now().unwrap() + zx::MonotonicDuration::from_seconds(3);
                    let mut fut = CurrentDispatcher.after_deadline(deadline);
                    assert_eq!(poll!(&mut fut), Poll::Pending);
                    assert!(fut.await.is_ok());
                    assert!(CurrentDispatcher.now().unwrap() >= deadline);
                    tx.send(()).unwrap();
                })
                .unwrap();
            rx.recv().unwrap();
        });
    }

    #[test]
    fn drop_after_poll() {
        with_raw_dispatcher("testing task", |dispatcher| {
            let (tx, rx) = mpsc::channel();
            dispatcher
                .spawn_task(async move {
                    let deadline =
                        CurrentDispatcher.now().unwrap() + zx::MonotonicDuration::from_minutes(3);
                    let mut fut = CurrentDispatcher.after_deadline(deadline);
                    assert_eq!(poll!(&mut fut), Poll::Pending);
                    tx.send(()).unwrap();
                })
                .unwrap();
            rx.recv().unwrap();
        });
    }

    #[test]
    fn dispatcher_shutdown_cancel() {
        let (fut_tx, fut_rx) = mpsc::channel();
        with_raw_dispatcher("testing task", |dispatcher| {
            let (tx, rx) = mpsc::channel();
            dispatcher
                .spawn_task(async move {
                    let deadline =
                        CurrentDispatcher.now().unwrap() + zx::MonotonicDuration::from_minutes(3);
                    let mut fut = CurrentDispatcher.after_deadline(deadline);
                    assert_eq!(poll!(&mut fut), Poll::Pending);
                    fut_tx.send(fut).unwrap();
                    tx.send(()).unwrap();
                })
                .unwrap();
            rx.recv().unwrap();
        });
        let mut fut = fut_rx.recv().unwrap();
        loop {
            let Poll::Ready(res) = fut.poll_unpin(&mut Context::from_waker(Waker::noop())) else {
                sleep(Duration::from_millis(10));
                continue;
            };
            assert_eq!(res, Err(Status::CANCELED));
            break;
        }
    }
}
