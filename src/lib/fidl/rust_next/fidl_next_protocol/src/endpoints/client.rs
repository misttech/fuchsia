// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL protocol clients.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll};
use std::sync::{Arc, Mutex};

use fidl_next_codec::{Encode, EncodeError, EncoderExt};

use crate::{ProtocolError, Transport, decode_epitaph, decode_header, encode_header};

use super::connection::{Connection, ORDINAL_EPITAPH, SendFuture};
use super::lockers::{LockerError, Lockers};

struct ClientSenderInner<T: Transport> {
    connection: Connection<T>,
    responses: Mutex<Lockers<T::RecvBuffer>>,
}

impl<T: Transport> ClientSenderInner<T> {
    fn new(shared: T::Shared) -> Self {
        Self { connection: Connection::new(shared), responses: Mutex::new(Lockers::new()) }
    }
}

/// A sender for a client endpoint.
pub struct ClientSender<T: Transport> {
    inner: Arc<ClientSenderInner<T>>,
}

impl<T: Transport> Drop for ClientSender<T> {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 2 {
            // This was the last client sender other than the one in the client
            // itself. Stop the connection.
            self.close();
        }
    }
}

impl<T: Transport> ClientSender<T> {
    /// Closes the channel from the client end.
    pub fn close(&self) {
        self.inner.connection.stop();
    }

    /// Send a request.
    pub fn send_one_way<M>(
        &self,
        ordinal: u64,
        request: M,
    ) -> Result<SendFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        self.send_message(0, ordinal, request)
    }

    /// Send a request and await for a response.
    pub fn send_two_way<M>(
        &self,
        ordinal: u64,
        request: M,
    ) -> Result<ResponseFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        let index = self.inner.responses.lock().unwrap().alloc(ordinal);

        // Send with txid = index + 1 because indices start at 0.
        match self.send_message(index + 1, ordinal, request) {
            Ok(future) => Ok(ResponseFuture {
                inner: &self.inner,
                index,
                state: ResponseFutureState::Sending(future),
            }),
            Err(e) => {
                self.inner.responses.lock().unwrap().free(index);
                Err(e)
            }
        }
    }

    fn send_message<M>(
        &self,
        txid: u32,
        ordinal: u64,
        message: M,
    ) -> Result<SendFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        self.inner.connection.send_with(|buffer| {
            encode_header::<T>(buffer, txid, ordinal)?;
            buffer.encode_next(message)
        })
    }
}

impl<T: Transport> Clone for ClientSender<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

enum ResponseFutureState<'a, T: Transport> {
    Sending(SendFuture<'a, T>),
    Receiving,
    // We store the completion state locally so that we can free the locker
    // during poll, instead of waiting until the future is dropped.
    Completed,
}

/// A future for a request pending a response.
pub struct ResponseFuture<'a, T: Transport> {
    inner: &'a ClientSenderInner<T>,
    index: u32,
    state: ResponseFutureState<'a, T>,
}

impl<T: Transport> Drop for ResponseFuture<'_, T> {
    fn drop(&mut self) {
        let mut responses = self.inner.responses.lock().unwrap();
        match self.state {
            // SAFETY: The future was canceled before it could be sent. The transaction ID was never
            // used, so it's safe to immediately reuse.
            ResponseFutureState::Sending(_) => responses.free(self.index),
            ResponseFutureState::Receiving => {
                if responses.get(self.index).unwrap().cancel() {
                    responses.free(self.index);
                }
            }
            // We already freed the slot when we completed.
            ResponseFutureState::Completed => (),
        }
    }
}

impl<T: Transport> ResponseFuture<'_, T> {
    fn poll_receiving(&mut self, cx: &mut Context<'_>) -> Poll<<Self as Future>::Output> {
        let mut responses = self.inner.responses.lock().unwrap();
        let ready = if let Some(ready) = responses.get(self.index).unwrap().read(cx.waker()) {
            Ok(ready)
        } else if let Some(termination_reason) = self.inner.connection.get_termination_reason() {
            Err(termination_reason)
        } else {
            return Poll::Pending;
        };

        responses.free(self.index);
        self.state = ResponseFutureState::Completed;
        Poll::Ready(ready)
    }
}

impl<T: Transport> Future for ResponseFuture<'_, T> {
    type Output = Result<T::RecvBuffer, ProtocolError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: We treat the state as pinned as long as it is sending.
        let this = unsafe { Pin::into_inner_unchecked(self) };

        match &mut this.state {
            ResponseFutureState::Sending(future) => {
                // SAFETY: Because the state is sending, we always treat its
                // future as pinned.
                let pinned = unsafe { Pin::new_unchecked(future) };
                match pinned.poll(cx) {
                    // The send has not completed yet. Leave the state as
                    // sending.
                    Poll::Pending => Poll::Pending,
                    Poll::Ready(Ok(())) => {
                        // The send succeeded. Change the state to receiving
                        // and poll receiving.
                        this.state = ResponseFutureState::Receiving;
                        this.poll_receiving(cx)
                    }
                    Poll::Ready(Err(e)) => {
                        // The send failed. Set our state to completed and free
                        // the locker.
                        this.state = ResponseFutureState::Completed;
                        this.inner.responses.lock().unwrap().free(this.index);
                        Poll::Ready(Err(e))
                    }
                }
            }
            ResponseFutureState::Receiving => this.poll_receiving(cx),
            // We could reach here if this future is polled after completion, but that's not
            // supposed to happen.
            ResponseFutureState::Completed => unreachable!(),
        }
    }
}

/// A type which handles incoming events for a client.
pub trait ClientHandler<T: Transport> {
    /// Handles a received client event.
    ///
    /// The client cannot handle more messages until `on_event` completes. If `on_event` should
    /// handle requests in parallel, it should spawn a new async task and return.
    fn on_event(
        &mut self,
        sender: &ClientSender<T>,
        ordinal: u64,
        buffer: T::RecvBuffer,
    ) -> impl Future<Output = ()> + Send;
}

/// A client for an endpoint.
///
/// It must be actively polled to receive events and two-way message responses.
pub struct Client<T: Transport> {
    sender: ClientSender<T>,
    exclusive: T::Exclusive,
    is_terminated: bool,
}

impl<T: Transport> Drop for Client<T> {
    fn drop(&mut self) {
        if !self.is_terminated {
            // SAFETY: We checked that the connection has not been terminated.
            unsafe {
                self.terminate(ProtocolError::Stopped);
            }
        }
    }
}

impl<T: Transport> Client<T> {
    /// Creates a new client from a transport.
    pub fn new(transport: T) -> Self {
        let (shared, exclusive) = transport.split();
        let inner = Arc::new(ClientSenderInner::new(shared));
        Self { sender: ClientSender { inner }, exclusive, is_terminated: false }
    }

    /// # Safety
    ///
    /// The connection must not yet be terminated.
    unsafe fn terminate(&mut self, error: ProtocolError<T::Error>) {
        // SAFETY: We checked that the connection has not been terminated.
        unsafe {
            self.sender.inner.connection.terminate(error);
        }
        self.sender.inner.responses.lock().unwrap().wake_all();
    }

    /// Returns the sender for the client.
    pub fn sender(&self) -> &ClientSender<T> {
        &self.sender
    }

    /// Runs the client with the provided handler.
    pub async fn run<H>(mut self, mut handler: H) -> Result<H, ProtocolError<T::Error>>
    where
        H: ClientHandler<T>,
    {
        // We may assume that the connection has not been terminated because
        // connections are only terminated by `run` and `drop`. Neither of those
        // could have been called before this method because `run` consumes
        // `self` and `drop` is only ever called once.

        let error = loop {
            // SAFETY: The connection has not been terminated.
            let result = unsafe { self.run_one(&mut handler).await };
            if let Err(error) = result {
                break error;
            }
        };

        // SAFETY: The connection has not been terminated.
        unsafe {
            self.terminate(error.clone());
        }
        self.is_terminated = true;

        match error {
            // We consider clients to have finished successfully only if they
            // stop themselves manually.
            ProtocolError::Stopped => Ok(handler),

            // Otherwise, the client finished with an error.
            _ => Err(error),
        }
    }

    /// # Safety
    ///
    /// The connection must not be terminated.
    async unsafe fn run_one<H>(&mut self, handler: &mut H) -> Result<(), ProtocolError<T::Error>>
    where
        H: ClientHandler<T>,
    {
        // SAFETY: The caller guaranteed that the connection is not terminated.
        let mut buffer = unsafe { self.sender.inner.connection.recv(&mut self.exclusive).await? };

        let (txid, ordinal) =
            decode_header::<T>(&mut buffer).map_err(ProtocolError::InvalidMessageHeader)?;

        if ordinal == ORDINAL_EPITAPH {
            let epitaph =
                decode_epitaph::<T>(&mut buffer).map_err(ProtocolError::InvalidEpitaphBody)?;
            return Err(ProtocolError::PeerClosedWithEpitaph(epitaph));
        } else if txid == 0 {
            handler.on_event(&self.sender, ordinal, buffer).await;
        } else {
            let mut responses = self.sender.inner.responses.lock().unwrap();
            let locker = responses
                .get(txid - 1)
                .ok_or_else(|| ProtocolError::UnrequestedResponse { txid })?;

            match locker.write(ordinal, buffer) {
                // Reader didn't cancel
                Ok(false) => (),
                // Reader canceled, we can drop the entry
                Ok(true) => responses.free(txid - 1),
                Err(LockerError::NotWriteable) => {
                    return Err(ProtocolError::UnrequestedResponse { txid });
                }
                Err(LockerError::MismatchedOrdinal { expected, actual }) => {
                    return Err(ProtocolError::InvalidResponseOrdinal { expected, actual });
                }
            }
        }

        Ok(())
    }

    /// Runs the client with the [`IgnoreEvents`] handler.
    pub async fn run_sender(self) -> Result<(), ProtocolError<T::Error>> {
        self.run(IgnoreEvents).await.map(|_| ())
    }
}

/// A client handler which ignores any incoming events.
pub struct IgnoreEvents;

impl<T: Transport> ClientHandler<T> for IgnoreEvents {
    async fn on_event(&mut self, _: &ClientSender<T>, _: u64, _: T::RecvBuffer) {}
}
