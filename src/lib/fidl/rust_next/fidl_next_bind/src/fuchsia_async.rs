// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL bindings integration with fuchsia-async.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};

use fidl_next_protocol::mpsc::Mpsc as BaseMpsc;
use fidl_next_protocol::{NonBlockingTransport, Transport};
use fuchsia_async::{Scope, ScopeHandle, Task};

use crate::{ClientEnd, Executor, HasExecutor, RunsTransport, ServerEnd};

/// A type representing the current fuchsia-async executor.
pub struct FuchsiaAsync;

impl Executor for FuchsiaAsync {
    type Task<T>
        = Task<T>
    where
        T: 'static;

    fn spawn<F>(&self, future: F) -> Self::Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        Task::spawn(future)
    }

    fn detach<T: 'static>(&self, task: Self::Task<T>) {
        drop(task.detach_on_drop());
    }
}

impl Executor for Scope {
    type Task<T>
        = Task<T>
    where
        T: 'static;

    fn spawn<F>(&self, future: F) -> Self::Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.compute(future)
    }

    fn detach<T: 'static>(&self, task: Self::Task<T>) {
        drop(task.detach_on_drop());
    }
}

impl Executor for ScopeHandle {
    type Task<T>
        = Task<T>
    where
        T: 'static;

    fn spawn<F>(&self, future: F) -> Self::Task<F::Output>
    where
        F: Future + Send + 'static,
        F::Output: Send + 'static,
    {
        self.compute(future)
    }

    fn detach<T: 'static>(&self, task: Self::Task<T>) {
        drop(task.detach_on_drop());
    }
}

/// A paired mpsc transport which runs on fuchsia-async by default.
pub struct Mpsc {
    base: BaseMpsc,
}

impl Mpsc {
    /// Creates client and server end mpscs which can communicate with each
    /// other.
    pub fn new<P>() -> (ClientEnd<P, Self>, ServerEnd<P, Self>) {
        let (a, b) = BaseMpsc::new();
        (ClientEnd::from_untyped(Self { base: a }), ServerEnd::from_untyped(Self { base: b }))
    }
}

impl Transport for Mpsc {
    type Error = <BaseMpsc as Transport>::Error;

    fn split(self) -> (Self::Shared, Self::Exclusive) {
        self.base.split()
    }

    type Shared = <BaseMpsc as Transport>::Shared;
    type Exclusive = <BaseMpsc as Transport>::Exclusive;

    type SendBuffer = <BaseMpsc as Transport>::SendBuffer;
    type SendFutureState = <BaseMpsc as Transport>::SendFutureState;

    fn acquire(shared: &Self::Shared) -> Self::SendBuffer {
        BaseMpsc::acquire(shared)
    }

    fn begin_send(shared: &Self::Shared, buffer: Self::SendBuffer) -> Self::SendFutureState {
        BaseMpsc::begin_send(shared, buffer)
    }

    fn poll_send(
        future: Pin<&mut Self::SendFutureState>,
        cx: &mut Context<'_>,
        shared: &Self::Shared,
    ) -> Poll<Result<(), Option<Self::Error>>> {
        BaseMpsc::poll_send(future, cx, shared)
    }

    type RecvFutureState = <BaseMpsc as Transport>::RecvFutureState;
    type RecvBuffer = <BaseMpsc as Transport>::RecvBuffer;

    fn begin_recv(shared: &Self::Shared, exclusive: &mut Self::Exclusive) -> Self::RecvFutureState {
        BaseMpsc::begin_recv(shared, exclusive)
    }

    fn poll_recv(
        future: Pin<&mut Self::RecvFutureState>,
        cx: &mut Context<'_>,
        shared: &Self::Shared,
        exclusive: &mut Self::Exclusive,
    ) -> Poll<Result<Self::RecvBuffer, Option<Self::Error>>> {
        BaseMpsc::poll_recv(future, cx, shared, exclusive)
    }
}

impl NonBlockingTransport for Mpsc {
    fn send_immediately(
        future_state: &mut Self::SendFutureState,
        shared: &Self::Shared,
    ) -> Result<(), Option<Self::Error>> {
        BaseMpsc::send_immediately(future_state, shared)
    }
}

impl<E: RunsTransport<BaseMpsc>> RunsTransport<Mpsc> for E {}

impl HasExecutor for Mpsc {
    type Executor = FuchsiaAsync;

    /// Returns a reference to the executor for this transport.
    fn executor(&self) -> Self::Executor {
        FuchsiaAsync
    }
}
