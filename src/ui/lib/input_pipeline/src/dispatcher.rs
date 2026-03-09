// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::task::Context;
use futures::prelude::*;
use futures::task::Poll;
use pin_project_lite::pin_project;
use std::pin::Pin;

pin_project! {
    #[derive(Debug)]
    #[must_use = "futures do nothing unless polled"]
    pub struct OnTimeout<F, T, OT> {
        #[pin]
        timer: T,
        #[pin]
        future: F,
        on_timeout: Option<OT>,
    }
}

impl<F: Future, T, OT> Future for OnTimeout<F, T, OT>
where
    T: Future<Output = ()> + 'static,
    OT: FnOnce() -> F::Output,
{
    type Output = F::Output;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        if let Poll::Ready(item) = this.future.poll(cx) {
            return Poll::Ready(item);
        }
        if let Poll::Ready(()) = this.timer.poll(cx) {
            let ot = this.on_timeout.take().expect("polled with timeout after completion");
            let item = (ot)();
            return Poll::Ready(item);
        }
        Poll::Pending
    }
}

/// A wrapper for a future which will complete with a provided closure when a timeout occurs. This
/// is forked from [`fuchsia_async::OnTimeout`] because that has a fixed dependency on
/// [`fuchsia_async::Timer`] which driver dispatcher does not support.
pub trait TimeoutExt: Future + Sized {
    fn on_timeout<T, OT>(self, timer: T, on_timeout: OT) -> OnTimeout<Self, T, OT>
    where
        T: Future<Output = ()> + 'static,
        OT: FnOnce() -> Self::Output,
    {
        OnTimeout { timer, future: self, on_timeout: Some(on_timeout) }
    }
}

impl<F: Future + Sized> TimeoutExt for F {}

pub type MonotonicInstant = fuchsia_async::MonotonicInstant;

#[derive(Clone, Default)]
pub struct Dispatcher {}

pub type Transport = zx::Channel;

#[derive(Debug)]
pub struct TaskHandle<T>(fuchsia_async::Task<T>);

impl TaskHandle<()> {
    pub fn detach(self) {
        self.0.detach();
    }
}

impl<T: 'static> TaskHandle<T> {
    pub fn abort(self) -> impl Future<Output = Option<T>> {
        self.0.abort()
    }
}

#[cfg(test)]
impl<T: 'static> From<fuchsia_async::Task<T>> for TaskHandle<T> {
    fn from(task: fuchsia_async::Task<T>) -> Self {
        Self(task)
    }
}

impl<T: 'static> Future for TaskHandle<T> {
    type Output = T;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        match self.0.poll_unpin(cx) {
            Poll::Ready(t) => Poll::Ready(t),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl Dispatcher {
    #[must_use]
    pub fn spawn_local(future: impl Future<Output = ()> + 'static) -> TaskHandle<()>
    where
        Self: 'static,
    {
        TaskHandle(fuchsia_async::Task::local(future))
    }

    pub fn after_deadline(deadline: MonotonicInstant) -> impl Future<Output = ()> + 'static {
        fuchsia_async::Timer::new(deadline)
    }
}
