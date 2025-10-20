// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::future::Future;
use core::marker::PhantomData;
use core::ops::Deref;

use fidl_next_codec::{Constrained, Encode};
use fidl_next_protocol::{self as protocol, ProtocolError, ServerHandler, Transport};

use crate::{HasConnectionHandles, Method, Respond, RespondErr, RespondFuture, ServerEnd};

/// A strongly typed server.
#[repr(transparent)]
pub struct Server<
    P,
    #[cfg(feature = "fuchsia")] T: Transport = zx::Channel,
    #[cfg(not(feature = "fuchsia"))] T: Transport,
> {
    server: protocol::Server<T>,
    _protocol: PhantomData<P>,
}

unsafe impl<P, T> Send for Server<P, T>
where
    protocol::Server<T>: Send,
    T: Transport,
{
}

impl<P, T: Transport> Server<P, T> {
    /// Creates a new server handle from an untyped server handle.
    pub fn from_untyped(server: protocol::Server<T>) -> Self {
        Self { server, _protocol: PhantomData }
    }

    /// Closes the channel from the server end.
    pub fn close(&self) {
        self.server.close();
    }

    /// Closes the channel from the server end without sending an epitaph.
    pub fn close_with_epitaph(&self, epitaph: i32) {
        self.server.close_with_epitaph(epitaph);
    }
}

impl<P, T: Transport> Clone for Server<P, T> {
    fn clone(&self) -> Self {
        Self { server: self.server.clone(), _protocol: PhantomData }
    }
}

impl<P: HasConnectionHandles<T>, T: Transport> Deref for Server<P, T> {
    type Target = P::Server;

    fn deref(&self) -> &Self::Target {
        // SAFETY: `P::Server` is a `#[repr(transparent)]` wrapper around
        // `Server<T>`.
        unsafe { &*(self as *const Self).cast::<P::Server>() }
    }
}

/// A protocol which dispatches incoming server messages to a handler.
pub trait DispatchServerMessage<
    H,
    #[cfg(feature = "fuchsia")] T: Transport = zx::Channel,
    #[cfg(not(feature = "fuchsia"))] T: Transport,
>: Sized + 'static
{
    /// Handles a received server one-way message with the given handler.
    fn on_one_way(
        handler: &mut H,
        ordinal: u64,
        buffer: T::RecvBuffer,
    ) -> impl Future<Output = Result<(), ProtocolError<T::Error>>> + Send;

    /// Handles a received server two-way message with the given handler.
    fn on_two_way(
        handler: &mut H,
        ordinal: u64,
        buffer: T::RecvBuffer,
        responder: protocol::Responder<T>,
    ) -> impl Future<Output = Result<(), ProtocolError<T::Error>>> + Send;
}

/// An adapter for a server protocol handler.
pub struct ServerHandlerAdapter<P, H> {
    handler: H,
    _protocol: PhantomData<P>,
}

unsafe impl<P, H> Send for ServerHandlerAdapter<P, H> where H: Send {}

impl<P, H> ServerHandlerAdapter<P, H> {
    /// Creates a new protocol server handler from a supported handler.
    pub fn from_untyped(handler: H) -> Self {
        Self { handler, _protocol: PhantomData }
    }
}

impl<P, H, T> ServerHandler<T> for ServerHandlerAdapter<P, H>
where
    P: DispatchServerMessage<H, T>,
    T: Transport,
{
    fn on_one_way(
        &mut self,
        ordinal: u64,
        buffer: T::RecvBuffer,
    ) -> impl Future<Output = Result<(), ProtocolError<T::Error>>> + Send {
        P::on_one_way(&mut self.handler, ordinal, buffer)
    }

    fn on_two_way(
        &mut self,
        ordinal: u64,
        buffer: <T as Transport>::RecvBuffer,
        responder: protocol::Responder<T>,
    ) -> impl Future<Output = Result<(), ProtocolError<T::Error>>> + Send {
        P::on_two_way(&mut self.handler, ordinal, buffer, responder)
    }
}

/// A strongly typed server.
pub struct ServerDispatcher<
    P,
    #[cfg(feature = "fuchsia")] T: Transport = zx::Channel,
    #[cfg(not(feature = "fuchsia"))] T: Transport,
> {
    dispatcher: protocol::ServerDispatcher<T>,
    _protocol: PhantomData<P>,
}

unsafe impl<P, T> Send for ServerDispatcher<P, T>
where
    protocol::Server<T>: Send,
    T: Transport,
{
}

impl<P, T: Transport> ServerDispatcher<P, T> {
    /// Creates a new server dispatcher from a server end.
    pub fn new(server_end: ServerEnd<P, T>) -> Self {
        Self {
            dispatcher: protocol::ServerDispatcher::new(server_end.into_untyped()),
            _protocol: PhantomData,
        }
    }

    /// Returns the dispatcher's server.
    pub fn server(&self) -> Server<P, T> {
        Server::from_untyped(self.dispatcher.server())
    }

    /// Creates a new server dispatcher from an untyped server dispatcher.
    pub fn from_untyped(server: protocol::ServerDispatcher<T>) -> Self {
        Self { dispatcher: server, _protocol: PhantomData }
    }

    /// Runs the server with the provided handler.
    pub async fn run<H>(self, handler: H) -> Result<H, ProtocolError<T::Error>>
    where
        P: DispatchServerMessage<H, T>,
        H: Send,
    {
        self.dispatcher
            .run(ServerHandlerAdapter { handler, _protocol: PhantomData::<P> })
            .await
            .map(|adapter| adapter.handler)
    }
}

/// A strongly typed `Responder`.
#[must_use]
pub struct Responder<
    M,
    #[cfg(feature = "fuchsia")] T: Transport = zx::Channel,
    #[cfg(not(feature = "fuchsia"))] T: Transport,
> {
    responder: protocol::Responder<T>,
    _method: PhantomData<M>,
}

impl<M, T: Transport> Responder<M, T> {
    /// Creates a new responder.
    pub fn from_untyped(responder: protocol::Responder<T>) -> Self {
        Self { responder, _method: PhantomData }
    }

    /// Responds to the client.
    ///
    /// For methods which return a result, this method implicitly returns `Ok`
    /// of the given response.
    pub fn respond<R>(self, response: R) -> RespondFuture<T>
    where
        M: Method + Respond<R>,
        M::Response: Constrained<Constraint = ()>,
        <M as Respond<R>>::Output: Encode<T::SendBuffer, Encoded = M::Response>,
    {
        self.respond_with(M::respond(response))
    }

    /// Responds `Err` to the client.
    pub fn respond_err<R>(self, response: R) -> RespondFuture<T>
    where
        M: Method + RespondErr<R>,
        M::Response: Constrained<Constraint = ()>,
        <M as RespondErr<R>>::Output: Encode<T::SendBuffer, Encoded = M::Response>,
    {
        self.respond_with(M::respond_err(response))
    }

    /// Responds to the client.
    pub fn respond_with<R>(self, response: R) -> RespondFuture<T>
    where
        M: Method,
        M::Response: Constrained<Constraint = ()>,
        R: Encode<T::SendBuffer, Encoded = M::Response>,
    {
        RespondFuture::from_untyped(self.responder.respond(M::ORDINAL, response))
    }
}
