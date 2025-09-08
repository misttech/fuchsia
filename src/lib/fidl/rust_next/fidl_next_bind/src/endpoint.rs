// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::{concat, stringify};

use fidl_next_codec::{
    Decode, DecodeError, Encodable, EncodableOption, Encode, EncodeError, EncodeOption,
    EncodeOptionRef, EncodeRef, FromWire, FromWireOption, FromWireOptionRef, FromWireRef, Slot,
    Wire, munge,
};
use fidl_next_protocol::{ProtocolError, Transport};

use crate::{
    Client, ClientSender, DispatchClientMessage, DispatchServerMessage, Executor, HasExecutor,
    Server, ServerSender,
};

macro_rules! endpoint {
    (
        #[doc = $doc:literal]
        $name:ident
    ) => {
        #[doc = $doc]
        #[derive(Debug)]
        #[repr(transparent)]
        pub struct $name<
            P,
            #[cfg(feature = "fuchsia")]
            T = zx::Channel,
            #[cfg(not(feature = "fuchsia"))]
            T,
        > {
            transport: T,
            _protocol: PhantomData<P>,
        }

        unsafe impl<P, T: Send> Send for $name<P, T> {}

        unsafe impl<P, T: Sync> Sync for $name<P, T> {}

        // SAFETY:
        // - `$name::Decoded<'de>` wraps a `T::Decoded<'de>`. Because `T: Wire`, `T::Decoded<'de>`
        //   does not yield any references to decoded data that outlive `'de`. Therefore,
        //   `$name::Decoded<'de>` also does not yield any references to decoded data that outlive
        //   `'de`.
        // - `$name` is `#[repr(transparent)]` over the transport `T`, and `zero_padding` calls
        //   `T::zero_padding` on `transport`. `_protocol` is a ZST which does not have any padding
        //   bytes to zero-initialize.
        unsafe impl<P: 'static, T: Wire> Wire for $name<P, T> {
            type Decoded<'de> = $name<P, T::Decoded<'de>>;

            #[inline]
            fn zero_padding(out: &mut MaybeUninit<Self>) {
                munge!(let Self { transport, _protocol: _ } = out);
                T::zero_padding(transport);
            }
        }

        impl<P, T> $name<P, T> {
            #[doc = concat!(
                "Converts from `&",
                stringify!($name),
                "<P, T>` to `",
                stringify!($name),
                "<P, &T>`.",
            )]
            pub fn as_ref(&self) -> $name<P, &T> {
                $name { transport: &self.transport, _protocol: PhantomData }
            }

            /// Returns a new endpoint over the given transport.
            pub fn from_untyped(transport: T) -> Self {
                Self { transport, _protocol: PhantomData }
            }

            /// Returns the underlying transport.
            pub fn into_untyped(self) -> T {
                self.transport
            }

            /// Returns the executor for the underlying transport.
            pub fn executor(&self) -> T::Executor
            where
                T: HasExecutor,
            {
                self.transport.executor()
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `decode` calls
        // `T::decode` on `transport`. `_protocol` is a ZST which does not have any data to decode.
        unsafe impl<D, P, T> Decode<D> for $name<P, T>
        where
            D: ?Sized,
            P: 'static,
            T: Decode<D>,
        {
            fn decode(slot: Slot<'_, Self>, decoder: &mut D) -> Result<(), DecodeError> {
                munge!(let Self { transport, _protocol: _ } = slot);
                T::decode(transport, decoder)
            }
        }

        impl<P, T> Encodable for $name<P, T>
        where
            T: Encodable,
            P: 'static,
        {
            type Encoded = $name<P, T::Encoded>;
        }

        impl<P, T> EncodableOption for $name<P, T>
        where
            T: EncodableOption,
            P: 'static,
        {
            type EncodedOption = $name<P, T::EncodedOption>;
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode` calls
        // `T::encode` on `transport`. `_protocol` is a ZST which does not have any data to encode.
        unsafe impl<E, P, T> Encode<E> for $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            T: Encode<E>,
        {
            fn encode(
                self,
                encoder: &mut E,
                out: &mut MaybeUninit<Self::Encoded>,
            ) -> Result<(), EncodeError> {
                munge!(let Self::Encoded { transport, _protocol: _ } = out);
                self.transport.encode(encoder, transport)
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode_ref` calls
        // `T::encode_ref` on `transport`. `_protocol` is a ZST which does not have any data to
        // encode.
        unsafe impl<E, P, T> EncodeRef<E> for $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            T: EncodeRef<E>,
        {
            fn encode_ref(
                &self,
                encoder: &mut E,
                out: &mut MaybeUninit<Self::Encoded>,
            ) -> Result<(), EncodeError> {
                self.as_ref().encode(encoder, out)
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode_option`
        // calls `T::encode_option` on `transport`. `_protocol` is a ZST which does not have any
        // data to encode.
        unsafe impl<E, P, T> EncodeOption<E> for $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            T: EncodeOption<E>,
        {
            fn encode_option(
                this: Option<Self>,
                encoder: &mut E,
                out: &mut MaybeUninit<Self::EncodedOption>,
            ) -> Result<(), EncodeError> {
                munge!(let Self::EncodedOption { transport, _protocol: _ } = out);
                T::encode_option(this.map(|this| this.transport), encoder, transport)
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode_option_ref`
        // calls `T::encode_option_ref` on `transport`. `_protocol` is a ZST which does not have any
        // data to encode.
        unsafe impl<E, P, T> EncodeOptionRef<E> for $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            T: EncodeOptionRef<E>,
        {
            fn encode_option_ref(
                this: Option<&Self>,
                encoder: &mut E,
                out: &mut MaybeUninit<Self::EncodedOption>,
            ) -> Result<(), EncodeError> {
                munge!(let Self::EncodedOption { transport, _protocol: _ } = out);
                T::encode_option_ref(this.map(|this| &this.transport), encoder, transport)
            }
        }

        impl<P, T, U> FromWire<$name<P, U>> for $name<P, T>
        where
            T: FromWire<U>,
        {
            #[inline]
            fn from_wire(wire: $name<P, U>) -> Self {
                $name {
                    transport: T::from_wire(wire.transport),
                    _protocol: PhantomData,
                }
            }
        }

        impl<P, T, U> FromWireRef<$name<P, U>> for $name<P, T>
        where
            T: FromWireRef<U>,
        {
            #[inline]
            fn from_wire_ref(wire: &$name<P, U>) -> Self {
                $name {
                    transport: T::from_wire_ref(&wire.transport),
                    _protocol: PhantomData,
                }
            }
        }

        impl<P, T, U> FromWireOption<$name<P, U>> for $name<P, T>
        where
            P: 'static,
            T: FromWireOption<U>,
            U: Wire,
        {
            #[inline]
            fn from_wire_option(wire: $name<P, U>) -> Option<Self> {
                T::from_wire_option(wire.transport).map(|transport| $name {
                    transport,
                    _protocol: PhantomData,
                })
            }
        }

        impl<P, T, U> FromWireOptionRef<$name<P, U>> for $name<P, T>
        where
            P: 'static,
            T: FromWireOptionRef<U>,
            U: Wire,
        {
            #[inline]
            fn from_wire_option_ref(wire: &$name<P, U>) -> Option<Self> {
                T::from_wire_option_ref(&wire.transport).map(|transport| $name {
                    transport,
                    _protocol: PhantomData,
                })
            }
        }
    };
}

endpoint! {
    /// The client end of a protocol.
    ClientEnd
}

endpoint! {
    /// The server end of a protocol.
    ServerEnd
}

/// A client or server handler task.
pub type HandlerTask<T, H, E = <T as HasExecutor>::Executor> =
    <E as Executor>::Task<Result<H, ProtocolError<<T as Transport>::Error>>>;

impl<P, T: Transport> ClientEnd<P, T> {
    /// Spawns a client for the given client end with a handler on an executor.
    ///
    /// Returns the client sender and a join handle for the spawned task.
    pub fn spawn_full_with_handler_on<H, E>(
        self,
        handler: H,
        executor: &E,
    ) -> (ClientSender<P, T>, HandlerTask<T, H, E>)
    where
        P: DispatchClientMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        let client = Client::new(self);
        let sender = client.sender().clone();
        (sender, executor.spawn(client.run(handler)))
    }

    /// Spawns a client for the given client end with a handler on an executor.
    ///
    /// Returns the client sender.
    pub fn spawn_with_handler_on<H, E>(self, handler: H, executor: &E) -> ClientSender<P, T>
    where
        P: DispatchClientMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        let (sender, task) = Self::spawn_full_with_handler_on(self, handler, executor);
        executor.detach(task);
        sender
    }

    /// Spawns a client for the given client end with a handler on the default
    /// executor for the transport.
    ///
    /// Returns the client sender and a join handle for the spawned task.
    pub fn spawn_full_with_handler<H>(self, handler: H) -> (ClientSender<P, T>, HandlerTask<T, H>)
    where
        P: DispatchClientMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_full_with_handler_on(self, handler, &executor)
    }

    /// Spawns a client for the given client end with a handler on the default
    /// executor for the transport.
    ///
    /// Returns the client sender.
    pub fn spawn_with_handler<H>(self, handler: H) -> ClientSender<P, T>
    where
        P: DispatchClientMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_with_handler_on(self, handler, &executor)
    }

    /// Spawns a client for the given client end on an executor.
    ///
    /// The spawned client will ignore all incoming events. Returns the client
    /// sender and a join handle for the spawned task.
    pub fn spawn_full_on<E>(self, executor: &E) -> (ClientSender<P, T>, HandlerTask<T, (), E>)
    where
        P: 'static,
        T: 'static,
        E: Executor,
    {
        let client = Client::new(self);
        let sender = client.sender().clone();
        (sender, executor.spawn(client.run_sender()))
    }

    /// Spawns a client for the given client end on an executor.
    ///
    /// The spawned client will ignore all incoming events. Returns the client
    /// sender.
    pub fn spawn_on<E>(self, executor: &E) -> ClientSender<P, T>
    where
        P: 'static,
        T: 'static,
        E: Executor,
    {
        let (sender, task) = Self::spawn_full_on(self, executor);
        executor.detach(task);
        sender
    }

    /// Spawns a client for the given client end on the default executor for the
    /// transport.
    ///
    /// The spawned client will ignore all incoming events. Returns the client
    /// sender and a join handle for the spawned task.
    pub fn spawn_full(self) -> (ClientSender<P, T>, HandlerTask<T, ()>)
    where
        P: 'static,
        T: HasExecutor + 'static,
    {
        let executor = self.executor();
        Self::spawn_full_on(self, &executor)
    }

    /// Spawns a client for the given client end on the default executor for the
    /// transport.
    ///
    /// The spawned client will ignore all incoming events. Returns the client
    /// sender.
    pub fn spawn(self) -> ClientSender<P, T>
    where
        P: 'static,
        T: HasExecutor + 'static,
    {
        let executor = self.executor();
        Self::spawn_on(self, &executor)
    }
}

impl<P, T: Transport> ServerEnd<P, T> {
    /// Spawns a server for the given server end with a handler on an executor.
    ///
    /// Returns the join handle for the spawned task and the server sender.
    pub fn spawn_full_on<H, E>(
        self,
        handler: H,
        executor: &E,
    ) -> (HandlerTask<T, H, E>, ServerSender<P, T>)
    where
        P: DispatchServerMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        let server = Server::new(self);
        let sender = server.sender().clone();
        (executor.spawn(server.run(handler)), sender)
    }

    /// Spawns a server for the given server end with a handler on an executor.
    ///
    /// Returns the join handle for the spawned task.
    pub fn spawn_on<H, E>(self, handler: H, executor: &E) -> HandlerTask<T, H, E>
    where
        P: DispatchServerMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        executor.spawn(Server::new(self).run(handler))
    }

    /// Spawns a server for the given server end with a handler on the default
    /// executor for the transport.
    ///
    /// Returns the join handle for the spawned task and the server sender.
    pub fn spawn_full<H>(self, handler: H) -> (HandlerTask<T, H>, ServerSender<P, T>)
    where
        P: DispatchServerMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_full_on(self, handler, &executor)
    }

    /// Spawns a server for the given server end with a handler on the default
    /// executor for the transport.
    ///
    /// Returns the join handle for the spawned task.
    pub fn spawn<H>(self, handler: H) -> HandlerTask<T, H>
    where
        P: DispatchServerMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_on(self, handler, &executor)
    }
}
