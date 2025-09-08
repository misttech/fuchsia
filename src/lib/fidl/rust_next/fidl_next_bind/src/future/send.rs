// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, ready};

use fidl_next_codec::EncodeError;
use fidl_next_protocol::Transport;
use pin_project::pin_project;

use crate::Error;

#[pin_project(project = SendFutureStateProj, project_replace = SendFutureStateOwn)]
enum SendFutureState<'a, T: Transport> {
    EncodeError(EncodeError),
    SendRequest(#[pin] fidl_next_protocol::SendFuture<'a, T>),
    Finished,
}

impl<'a, T: Transport> SendFutureState<'a, T> {
    fn poll_state(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Error<T::Error>>> {
        match self.as_mut().project() {
            SendFutureStateProj::EncodeError(_) => {
                let state = self.project_replace(SendFutureState::Finished);
                let SendFutureStateOwn::EncodeError(error) = state else {
                    unreachable!();
                };
                Poll::Ready(Err(Error::Encode(error)))
            }
            SendFutureStateProj::SendRequest(future) => match ready!(future.poll(cx)) {
                Ok(()) => Poll::Ready(Ok(())),
                Err(error) => Poll::Ready(Err(Error::Protocol(error))),
            },
            SendFutureStateProj::Finished => {
                panic!("SendFutureState polled after completing");
            }
        }
    }
}

/// A future which sends an encoded message to a connection.
#[must_use = "futures do nothing unless polled"]
#[pin_project]
pub struct SendFuture<
    'a,
    #[cfg(feature = "fuchsia")] T: Transport = zx::Channel,
    #[cfg(not(feature = "fuchsia"))] T: Transport,
> {
    #[pin]
    state: SendFutureState<'a, T>,
}

impl<'a, T: Transport> SendFuture<'a, T> {
    /// Returns a `SendFuture` wrapping the given result.
    pub fn from_untyped(
        result: Result<fidl_next_protocol::SendFuture<'a, T>, EncodeError>,
    ) -> Self {
        Self {
            state: match result {
                Err(error) => SendFutureState::EncodeError(error),
                Ok(future) => SendFutureState::SendRequest(future),
            },
        }
    }

    /// Encodes the message.
    ///
    /// Returns a future which sends the message, or an error if it failed.
    pub fn encode(self) -> Result<EncodedSendFuture<'a, T>, Error<T::Error>> {
        Ok(EncodedSendFuture {
            state: match self.state {
                SendFutureState::EncodeError(error) => return Err(Error::Encode(error)),
                state => state,
            },
        })
    }
}

impl<'a, T: Transport> Future for SendFuture<'a, T> {
    type Output = Result<(), Error<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().state.poll_state(cx)
    }
}

/// A future which sends an encoded message to a connection.
///
/// This future has already been successfully encoded. It still needs to be
/// sent.
#[must_use = "futures do nothing unless polled"]
#[pin_project]
pub struct EncodedSendFuture<
    'a,
    #[cfg(feature = "fuchsia")] T: Transport = zx::Channel,
    #[cfg(not(feature = "fuchsia"))] T: Transport,
> {
    #[pin]
    state: SendFutureState<'a, T>,
}

impl<'a, T: Transport> Future for EncodedSendFuture<'a, T> {
    type Output = Result<(), Error<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.project().state.poll_state(cx)
    }
}
