// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! FIDL protocol servers.

use core::future::Future;
use core::num::NonZeroU32;

use fidl_next_codec::{Encode, EncodeError, EncoderExt as _};

use crate::concurrency::sync::Arc;
use crate::concurrency::sync::atomic::{AtomicI64, Ordering};

use crate::{ProtocolError, Transport, decode_header, encode_header};

use super::connection::{Connection, SendFuture};

/// A responder for a two-way message.
#[must_use]
pub struct Responder {
    txid: NonZeroU32,
}

struct ServerSenderInner<T: Transport> {
    connection: Connection<T>,
    epitaph: AtomicI64,
}

impl<T: Transport> ServerSenderInner<T> {
    const EPITAPH_NONE: i64 = i64::MAX;

    fn new(shared: T::Shared) -> Self {
        Self { connection: Connection::new(shared), epitaph: AtomicI64::new(Self::EPITAPH_NONE) }
    }

    fn close_with_epitaph(&self, epitaph: Option<i32>) {
        if let Some(epitaph) = epitaph {
            self.epitaph.store(epitaph as i64, Ordering::Relaxed);
        }
        self.connection.stop();
    }

    fn epitaph(&self) -> Option<i32> {
        let epitaph = self.epitaph.load(Ordering::Relaxed);
        if epitaph != Self::EPITAPH_NONE { Some(epitaph as i32) } else { None }
    }
}

/// A sender for a server endpoint.
pub struct ServerSender<T: Transport> {
    inner: Arc<ServerSenderInner<T>>,
}

impl<T: Transport> ServerSender<T> {
    /// Closes the channel from the server end.
    pub fn close(&self) {
        self.inner.close_with_epitaph(None);
    }

    /// Closes the channel from the server end after sending an epitaph message.
    pub fn close_with_epitaph(&self, epitaph: i32) {
        self.inner.close_with_epitaph(Some(epitaph));
    }

    /// Send an event.
    pub fn send_event<M>(&self, ordinal: u64, event: M) -> Result<SendFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        self.inner.connection.send_with(|buffer| {
            encode_header::<T>(buffer, 0, ordinal)?;
            buffer.encode_next(event)
        })
    }

    /// Send a response to a two-way message.
    pub fn send_response<M>(
        &self,
        responder: Responder,
        ordinal: u64,
        response: M,
    ) -> Result<SendFuture<'_, T>, EncodeError>
    where
        M: Encode<T::SendBuffer>,
    {
        self.inner.connection.send_with(|buffer| {
            encode_header::<T>(buffer, responder.txid.get(), ordinal)?;
            buffer.encode_next(response)
        })
    }
}

impl<T: Transport> Clone for ServerSender<T> {
    fn clone(&self) -> Self {
        Self { inner: self.inner.clone() }
    }
}

/// A type which handles incoming events for a server.
///
/// The futures returned by `on_one_way` and `on_two_way` are required to be `Send`. See
/// `LocalServerHandler` for a version of this trait which does not require the returned futures to
/// be `Send`.
pub trait ServerHandler<T: Transport> {
    /// Handles a received one-way server message.
    ///
    /// The server cannot handle more messages until `on_one_way` completes. If `on_one_way` may
    /// block, perform asynchronous work, or take a long time to process a message, it should
    /// offload work to an async task.
    fn on_one_way(
        &mut self,
        sender: &ServerSender<T>,
        ordinal: u64,
        buffer: T::RecvBuffer,
    ) -> impl Future<Output = ()> + Send;

    /// Handles a received two-way server message.
    ///
    /// The server cannot handle more messages until `on_two_way` completes. If `on_two_way` may
    /// block, perform asynchronous work, or take a long time to process a message, it should
    /// offload work to an async task.
    fn on_two_way(
        &mut self,
        sender: &ServerSender<T>,
        ordinal: u64,
        buffer: T::RecvBuffer,
        responder: Responder,
    ) -> impl Future<Output = ()> + Send;
}

/// A server for an endpoint.
pub struct Server<T: Transport> {
    sender: ServerSender<T>,
    exclusive: T::Exclusive,
    is_terminated: bool,
}

impl<T: Transport> Drop for Server<T> {
    fn drop(&mut self) {
        if !self.is_terminated {
            // SAFETY: We checked that the connection has not been terminated.
            unsafe {
                self.sender.inner.connection.terminate(ProtocolError::Stopped);
            }
        }
    }
}

impl<T: Transport> Server<T> {
    /// Creates a new server from a transport.
    pub fn new(transport: T) -> Self {
        let (shared, exclusive) = transport.split();
        Self {
            sender: ServerSender { inner: Arc::new(ServerSenderInner::new(shared)) },
            exclusive,
            is_terminated: false,
        }
    }

    /// Returns the sender for the server.
    pub fn sender(&self) -> &ServerSender<T> {
        &self.sender
    }

    /// Runs the server with the provided handler.
    pub async fn run<H>(mut self, mut handler: H) -> Result<H, ProtocolError<T::Error>>
    where
        H: ServerHandler<T>,
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

        // If we closed locally, we may have an epitaph to send before
        // terminating the connection.
        if matches!(error, ProtocolError::Stopped) {
            if let Some(epitaph) = self.sender.inner.epitaph() {
                // Note that we don't care whether sending the epitaph succeeds
                // or fails; it's best-effort.

                // SAFETY: The connection has not been terminated.
                let _ = unsafe { self.sender.inner.connection.send_epitaph(epitaph).await };
            }
        }

        // SAFETY: The connection has not been terminated.
        unsafe {
            self.sender.inner.connection.terminate(error.clone());
        }
        self.is_terminated = true;

        match error {
            // We consider servers to have finished successfully if they stop
            // themselves manually, or if the client disconnects.
            ProtocolError::Stopped | ProtocolError::PeerClosed => Ok(handler),

            // Otherwise, the server finished with an error.
            _ => Err(error),
        }
    }

    /// # Safety
    ///
    /// The connection must not be terminated.
    async unsafe fn run_one<H>(&mut self, handler: &mut H) -> Result<(), ProtocolError<T::Error>>
    where
        H: ServerHandler<T>,
    {
        // SAFETY: The caller guaranteed that the connection is not terminated.
        let mut buffer = unsafe { self.sender.inner.connection.recv(&mut self.exclusive).await? };

        let (txid, ordinal) =
            decode_header::<T>(&mut buffer).map_err(ProtocolError::InvalidMessageHeader)?;
        if let Some(txid) = NonZeroU32::new(txid) {
            handler.on_two_way(&self.sender, ordinal, buffer, Responder { txid }).await;
        } else {
            handler.on_one_way(&self.sender, ordinal, buffer).await;
        }

        Ok(())
    }
}
