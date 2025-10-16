// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL protocol clients.

use core::future::Future;
use core::pin::Pin;
use core::task::{Context, Poll, ready};

use fidl_next_codec::{Constrained, Encode, EncodeError, EncoderExt};
use pin_project::{pin_project, pinned_drop};

use crate::concurrency::sync::{Arc, Mutex};
use crate::endpoints::connection::{Connection, ORDINAL_EPITAPH};
use crate::endpoints::lockers::{LockerError, Lockers};
use crate::{ProtocolError, SendFuture, Transport, decode_epitaph, decode_header, encode_header};

struct ClientInner<T: Transport> {
    connection: Connection<T>,
    responses: Mutex<Lockers<T::RecvBuffer>>,
}

impl<T: Transport> ClientInner<T> {
    fn new(shared: T::Shared) -> Self {
        Self { connection: Connection::new(shared), responses: Mutex::new(Lockers::new()) }
    }
}

/// A client endpoint.
pub struct Client<T: Transport> {
    inner: Arc<ClientInner<T>>,
}

impl<T: Transport> Drop for Client<T> {
    fn drop(&mut self) {
        if Arc::strong_count(&self.inner) == 2 {
            // This was the last reference to the connection other than the one
            // in the dispatcher itself. Stop the connection.
            self.close();
        }
    }
}

impl<T: Transport> Client<T> {
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
        M::Encoded: Constrained<Constraint = ()>,
    {
        self.send_message(0, ordinal, request)
    }

    /// Send a request and await for a response.
    pub fn send_two_way<M>(
        &self,
        ordinal: u64,
        request: M,
    ) -> Result<TwoWayRequestFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
        M::Encoded: Constrained<Constraint = ()>,
    {
        let index = self.inner.responses.lock().unwrap().alloc(ordinal);

        // Send with txid = index + 1 because indices start at 0.
        match self.send_message(index + 1, ordinal, request) {
            Ok(send_future) => {
                Ok(TwoWayRequestFuture { inner: &self.inner, index: Some(index), send_future })
            }
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
        M::Encoded: Constrained<Constraint = ()>,
    {
        self.inner.connection.send_message(|buffer| {
            encode_header::<T>(buffer, txid, ordinal)?;
            buffer.encode_next(message, ())
        })
    }
}

impl<T: Transport> Clone for Client<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

/// A future for a pending response to a two-way message.
pub struct TwoWayResponseFuture<'a, T: Transport> {
    inner: &'a ClientInner<T>,
    index: Option<u32>,
}

impl<T: Transport> Drop for TwoWayResponseFuture<'_, T> {
    fn drop(&mut self) {
        // If `index` is `Some`, then we still need to free our locker.
        if let Some(index) = self.index {
            let mut responses = self.inner.responses.lock().unwrap();
            if responses.get(index).unwrap().cancel() {
                responses.free(index);
            }
        }
    }
}

impl<T: Transport> Future for TwoWayResponseFuture<'_, T> {
    type Output = Result<T::RecvBuffer, ProtocolError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = Pin::into_inner(self);
        let Some(index) = this.index else {
            panic!("TwoWayResponseFuture polled after returning `Poll::Ready`");
        };

        let mut responses = this.inner.responses.lock().unwrap();
        let ready = if let Some(ready) = responses.get(index).unwrap().read(cx.waker()) {
            Ok(ready)
        } else if let Some(termination_reason) = this.inner.connection.get_termination_reason() {
            Err(termination_reason)
        } else {
            return Poll::Pending;
        };

        responses.free(index);
        this.index = None;
        Poll::Ready(ready)
    }
}

/// A future for a sending a two-way FIDL message.
#[pin_project(PinnedDrop)]
pub struct TwoWayRequestFuture<'a, T: Transport> {
    inner: &'a ClientInner<T>,
    index: Option<u32>,
    #[pin]
    send_future: SendFuture<'a, T>,
}

#[pinned_drop]
impl<T: Transport> PinnedDrop for TwoWayRequestFuture<'_, T> {
    fn drop(self: Pin<&mut Self>) {
        if let Some(index) = self.index {
            let mut responses = self.inner.responses.lock().unwrap();

            // The future was canceled before it could be sent. The transaction
            // ID was never used, so it's safe to immediately reuse.
            responses.free(index);
        }
    }
}

impl<'a, T: Transport> Future for TwoWayRequestFuture<'a, T> {
    type Output = Result<TwoWayResponseFuture<'a, T>, ProtocolError<T::Error>>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        let Some(index) = *this.index else {
            panic!("TwoWayRequestFuture polled after returning `Poll::Ready`");
        };

        let result = ready!(this.send_future.poll(cx));
        *this.index = None;
        if let Err(error) = result {
            // The send failed. Free the locker and return an error.
            this.inner.responses.lock().unwrap().free(index);
            Poll::Ready(Err(error))
        } else {
            Poll::Ready(Ok(TwoWayResponseFuture { inner: this.inner, index: Some(index) }))
        }
    }
}

/// A type which handles incoming events for a client.
pub trait ClientHandler<T: Transport> {
    /// Handles a received client event, returning the appropriate flow control
    /// to perform.
    ///
    /// The client cannot handle more messages until `on_event` completes. If
    /// `on_event` should handle requests in parallel, it should spawn a new
    /// async task and return.
    fn on_event(
        &mut self,
        ordinal: u64,
        buffer: T::RecvBuffer,
    ) -> impl Future<Output = Result<(), ProtocolError<T::Error>>> + Send;
}

/// A dispatcher for a client endpoint.
///
/// A client dispatcher receives all of the incoming messages and dispatches them to the client
/// handler and two-way futures. It acts as the message pump for the client.
///
/// The dispatcher must be actively polled to receive events and two-way message responses. If the
/// dispatcher is not [`run`](ClientDispatcher::run) concurrently, then events will not be received
/// and two-way message futures will not receive their responses.
pub struct ClientDispatcher<T: Transport> {
    inner: Arc<ClientInner<T>>,
    exclusive: T::Exclusive,
    is_terminated: bool,
}

impl<T: Transport> Drop for ClientDispatcher<T> {
    fn drop(&mut self) {
        if !self.is_terminated {
            // SAFETY: We checked that the connection has not been terminated.
            unsafe {
                self.terminate(ProtocolError::Stopped);
            }
        }
    }
}

impl<T: Transport> ClientDispatcher<T> {
    /// Creates a new client from a transport.
    pub fn new(transport: T) -> Self {
        let (shared, exclusive) = transport.split();
        Self { inner: Arc::new(ClientInner::new(shared)), exclusive, is_terminated: false }
    }

    /// # Safety
    ///
    /// The connection must not yet be terminated.
    unsafe fn terminate(&mut self, error: ProtocolError<T::Error>) {
        // SAFETY: We checked that the connection has not been terminated.
        unsafe {
            self.inner.connection.terminate(error);
        }
        self.inner.responses.lock().unwrap().wake_all();
    }

    /// Returns a client for the dispatcher.
    ///
    /// When the last `Client` is dropped, the dispatcher will be stopped.
    pub fn client(&self) -> Client<T> {
        Client { inner: self.inner.clone() }
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
        let mut buffer = unsafe { self.inner.connection.recv(&mut self.exclusive).await? };

        let (txid, ordinal) =
            decode_header::<T>(&mut buffer).map_err(ProtocolError::InvalidMessageHeader)?;

        if ordinal == ORDINAL_EPITAPH {
            let epitaph =
                decode_epitaph::<T>(&mut buffer).map_err(ProtocolError::InvalidEpitaphBody)?;
            return Err(ProtocolError::PeerClosedWithEpitaph(epitaph));
        } else if txid == 0 {
            handler.on_event(ordinal, buffer).await?;
        } else {
            let mut responses = self.inner.responses.lock().unwrap();
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
    pub async fn run_client(self) -> Result<(), ProtocolError<T::Error>> {
        self.run(IgnoreEvents).await.map(|_| ())
    }
}

/// A client handler which ignores any incoming events.
pub struct IgnoreEvents;

impl<T: Transport> ClientHandler<T> for IgnoreEvents {
    async fn on_event(&mut self, _: u64, _: T::RecvBuffer) -> Result<(), ProtocolError<T::Error>> {
        Ok(())
    }
}
