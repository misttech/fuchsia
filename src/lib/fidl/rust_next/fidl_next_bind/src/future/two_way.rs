// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::future::Future;
use core::marker::PhantomData;
use core::pin::Pin;
use core::task::{Context, Poll, ready};

use fidl_next_codec::{Decode, DecoderExt, EncodeError};
use fidl_next_protocol::Transport;
use pin_project::pin_project;

use crate::{Error, Method, Response};

#[pin_project(project = TwoWayFutureStateProj, project_replace = TwoWayFutureStateOwn)]
enum TwoWayFutureState<'a, T: Transport> {
    EncodeError(EncodeError),
    SendRequest(fidl_next_protocol::TwoWayRequestFuture<'a, T>),
    SendingRequest(#[pin] fidl_next_protocol::TwoWayRequestFuture<'a, T>),
    ReceiveResponse(fidl_next_protocol::TwoWayResponseFuture<'a, T>),
    ReceivingResponse(#[pin] fidl_next_protocol::TwoWayResponseFuture<'a, T>),
    DecodeBuffer(T::RecvBuffer),
    Finished,
}

macro_rules! impl_two_way_future_state {
    ($(
        $variant:ident($ty:ty) => $check:ident $unwrap:ident
    ),* $(,)?) => {
        impl<T: Transport> TwoWayFutureState<'_, T> {
            $(
                #[allow(dead_code)]
                fn $check(&self) -> bool {
                    matches!(self, Self::$variant(_))
                }
            )*
        }

        impl<'a, T: Transport> TwoWayFutureStateOwn<'a, T> {
            $(
                #[allow(dead_code)]
                fn $unwrap(self) -> $ty {
                    let Self::$variant(value) = self else {
                        unreachable!()
                    };
                    value
                }
            )*
        }
    };
}

impl_two_way_future_state! {
    EncodeError(EncodeError) => is_encode_error unwrap_encode_error,
    SendRequest(fidl_next_protocol::TwoWayRequestFuture<'a, T>)
        => is_send_request unwrap_send_request,
    ReceiveResponse(fidl_next_protocol::TwoWayResponseFuture<'a, T>)
        => is_receive_response unwrap_receive_response,
    DecodeBuffer(T::RecvBuffer) => is_decode_buffer unwrap_decode_buffer,
}

impl<'a, T: Transport> TwoWayFutureState<'a, T> {
    fn finish(self: Pin<&mut Self>) -> TwoWayFutureStateOwn<'a, T> {
        self.project_replace(Self::Finished)
    }

    fn poll_advance(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Result<(), Error<T::Error>>> {
        Poll::Ready(match self.as_mut().project() {
            TwoWayFutureStateProj::EncodeError(_) => {
                Err(Error::Encode(self.finish().unwrap_encode_error()))
            }
            TwoWayFutureStateProj::SendRequest(_) => {
                let future = self.as_mut().finish().unwrap_send_request();
                self.project_replace(Self::SendingRequest(future));
                Ok(())
            }
            TwoWayFutureStateProj::SendingRequest(future) => {
                match ready!(future.poll(cx)) {
                    Ok(future) => {
                        self.project_replace(Self::ReceiveResponse(future));
                        Ok(())
                    }
                    Err(error) => {
                        self.finish();
                        Err(Error::Protocol(error))
                    }
                }
            }
            TwoWayFutureStateProj::ReceiveResponse(_) => {
                let future = self.as_mut().finish().unwrap_receive_response();
                self.project_replace(Self::ReceivingResponse(future));
                Ok(())
            }
            TwoWayFutureStateProj::ReceivingResponse(future) => {
                match ready!(future.poll(cx)) {
                    Ok(buffer) => {
                        self.project_replace(Self::DecodeBuffer(buffer));
                        Ok(())
                    }
                    Err(error) => {
                        self.finish();
                        Err(Error::Protocol(error))
                    }
                }
            }
            TwoWayFutureStateProj::DecodeBuffer(_) | TwoWayFutureStateProj::Finished => {
                panic!("TwoWayFutureState polled after completing");
            }
        })
    }

    fn poll_until(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        is_done: impl Fn(&Self) -> bool,
    ) -> Poll<Result<TwoWayFutureStateOwn<'a, T>, Error<T::Error>>> {
        while !is_done(&self) {
            if let Err(error) = ready!(self.as_mut().poll_advance(cx)) {
                return Poll::Ready(Err(error));
            }
        }
        Poll::Ready(Ok(self.finish()))
    }
}

macro_rules! two_way_futures {
    ($(
        $(#[$metas:meta])* $future:ident -> $output:ty {
            $check:ident => |$state:ident| $expr:expr
        }
    ),* $(,)?) => {
        $(
            $(#[$metas])*
            #[must_use = "futures do nothing unless polled"]
            #[pin_project]
            pub struct $future<
                'a,
                M: Method,
                #[cfg(feature = "fuchsia")] T: Transport = zx::Channel,
                #[cfg(not(feature = "fuchsia"))] T: Transport,
            > {
                #[pin]
                state: TwoWayFutureState<'a, T>,
                _method: PhantomData<M>,
            }

            impl<'a, M, T> Future for $future<'a, M, T>
            where
                M: Method,
                M::Response: Decode<T::RecvBuffer>,
                T: Transport,
            {
                type Output = Result<$output, Error<T::Error>>;

                fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
                    let $state = ready!(self.project().state.poll_until(
                        cx,
                        TwoWayFutureState::$check,
                    ))?;
                    Poll::Ready(Ok($expr))
                }
            }
        )*
    }
}

two_way_futures! {
    /// A future which performs a two-way FIDL method call.
    TwoWayFuture -> Response<M, T> {
        is_decode_buffer => |state| state.unwrap_decode_buffer().decode()?
    },

    /// A future which performs a two-way FIDL method call.
    ///
    /// This future has already been successfully encoded. It still needs to be
    /// sent and a response needs to be received.
    EncodedTwoWayFuture -> Response<M, T> {
        is_decode_buffer => |state| state.unwrap_decode_buffer().decode()?
    },

    /// A future which sends a two-way FIDL method call.
    ///
    /// This future returns another future which completes the FIDL call.
    SendTwoWayFuture -> SentTwoWayFuture<'a, M, T> {
        is_receive_response => |state| SentTwoWayFuture {
            state: TwoWayFutureState::ReceiveResponse(state.unwrap_receive_response()),
            _method: PhantomData,
        }
    },

    /// A future which performs a two-way FIDL method call.
    ///
    /// This future has already been successfully encoded and sent. A response
    /// still needs to be received.
    SentTwoWayFuture -> Response<M, T> {
        is_decode_buffer => |state| state.unwrap_decode_buffer().decode()?
    },

    /// A future which receives a two-way FIDL method call.
    ///
    /// This future returns the response buffer without decoding it first.
    ReceiveTwoWayFuture -> T::RecvBuffer {
        is_decode_buffer => |state| state.unwrap_decode_buffer()
    },
}

macro_rules! impl_for_futures {
    (
        $($futures:ident)*,
        $encode:item
    ) => {
        $(
            impl<'a, M: Method, T: Transport> $futures<'a, M, T> {
                $encode
            }
        )*
    }
}

impl_for_futures! {
    TwoWayFuture,

    /// Encodes the two-way message.
    ///
    /// Returns a future which completes the request, or an error if it failed.
    pub fn encode(self) -> Result<EncodedTwoWayFuture<'a, M, T>, Error<T::Error>> {
        Ok(EncodedTwoWayFuture {
            state: match self.state {
                TwoWayFutureState::EncodeError(error) => return Err(Error::Encode(error)),
                state => state,
            },
            _method: PhantomData,
        })
    }
}

impl_for_futures! {
    TwoWayFuture EncodedTwoWayFuture,

    /// Sends the two-way message.
    ///
    /// Returns a future which completes the request, or an error if it failed.
    pub fn send(self) -> SendTwoWayFuture<'a, M, T> {
        SendTwoWayFuture {
            state: self.state,
            _method: PhantomData,
        }
    }
}

impl_for_futures! {
    TwoWayFuture EncodedTwoWayFuture SentTwoWayFuture,

    /// Receives the response to the two-way message.
    ///
    /// Returns the response buffer, or an error if it failed.
    pub fn receive(self) -> ReceiveTwoWayFuture<'a, M, T> {
        ReceiveTwoWayFuture {
            state: self.state,
            _method: PhantomData,
        }
    }
}

impl<'a, M: Method, T: Transport> TwoWayFuture<'a, M, T> {
    /// Returns a `TwoWayFuture` wrapping the given result.
    pub fn from_untyped(
        result: Result<fidl_next_protocol::TwoWayRequestFuture<'a, T>, EncodeError>,
    ) -> Self {
        Self {
            state: match result {
                Ok(future) => TwoWayFutureState::SendRequest(future),
                Err(error) => TwoWayFutureState::EncodeError(error),
            },
            _method: PhantomData,
        }
    }
}
