// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::{concat, stringify};

use fidl_next_codec::{
    Constrained, Decode, DecodeError, Encode, EncodeError, EncodeOption, FromWire, FromWireOption,
    FromWireOptionRef, FromWireRef, IntoNatural, Slot, Unconstrained, Wire, munge,
};
use fidl_next_protocol::{ProtocolError, Transport};

use crate::{
    Client, ClientDispatcher, DispatchClientMessage, DispatchServerMessage, Executor, HasExecutor,
    HasTransport, IgnoreEvents, Server, ServerDispatcher,
};

macro_rules! endpoint {
    (
        #[doc = $doc:literal]
        $name:ident
    ) => {
        #[doc = $doc]
        #[derive(Debug, PartialEq)]
        #[repr(transparent)]
        pub struct $name<
            P,
            T = <P as HasTransport>::Transport,
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
            T: Constrained<Constraint=()>,
        {
            fn decode(slot: Slot<'_, Self>, decoder: &mut D, constraint:  <Self as Constrained>::Constraint) -> Result<(), DecodeError> {
                munge!(let Self { transport, _protocol: _ } = slot);
                T::decode(transport, decoder, constraint)
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode` calls
        // `T::encode` on `transport`. `_protocol` is a ZST which does not have any data to encode.
        unsafe impl<W, E, P, T> Encode<$name<P, W>, E> for $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            T: Encode<W, E>,
            W: Constrained<Constraint = ()> + Wire,
        {
            fn encode(
                self,
                encoder: &mut E,
                out: &mut MaybeUninit<$name<P, W>>,
                constraint:  (),
            ) -> Result<(), EncodeError> {
                munge!(let $name { transport, _protocol: _ } = out);
                self.transport.encode(encoder, transport, constraint)
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode_ref` calls
        // `T::encode_ref` on `transport`. `_protocol` is a ZST which does not have any data to
        // encode.
        unsafe impl<'a, W, E, P, T> Encode<$name<P, W>, E> for &'a $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            &'a T: Encode<W, E>,
            W: Constrained<Constraint = ()> + Wire,
        {
            fn encode(
                self,
                encoder: &mut E,
                out: &mut MaybeUninit<$name<P, W>>,
                constraint:  (),
            ) -> Result<(), EncodeError> {
                self.as_ref().encode(encoder, out, constraint)
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode_option`
        // calls `T::encode_option` on `transport`. `_protocol` is a ZST which does not have any
        // data to encode.
        unsafe impl<W, E, P, T> EncodeOption<$name<P, W>, E> for $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            T: EncodeOption<W, E>,
            W: Constrained<Constraint = ()>
        {
            fn encode_option(
                this: Option<Self>,
                encoder: &mut E,
                out: &mut MaybeUninit<$name<P, W>>,
                constraint: (),
            ) -> Result<(), EncodeError> {
                munge!(let $name { transport, _protocol: _ } = out);
                T::encode_option(this.map(|this| this.transport), encoder, transport, constraint)
            }
        }

        // SAFETY: `$name` is `#[repr(transparent)]` over the transport `T`, and `encode_option_ref`
        // calls `T::encode_option_ref` on `transport`. `_protocol` is a ZST which does not have any
        // data to encode.
        unsafe impl<'a, W, E, P, T> EncodeOption<$name<P, W>, E> for &'a $name<P, T>
        where
            E: ?Sized,
            P: 'static,
            &'a T: EncodeOption<W, E>,
            W: Constrained<Constraint = ()>
        {
            fn encode_option(
                this: Option<Self>,
                encoder: &mut E,
                out: &mut MaybeUninit<$name<P, W>>,
                constraint:  (),
            ) -> Result<(), EncodeError> {
                munge!(let $name { transport, _protocol: _ } = out);
                <&T>::encode_option(this.map(|this| &this.transport), encoder, transport, constraint)
            }
        }

        impl<P, T: Constrained<Constraint = ()>> Unconstrained for $name<P, T> {}

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

        impl<P, T: IntoNatural> IntoNatural for $name<P, T> {
            type Natural = $name<P, T::Natural>;
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
    /// Spawns a dispatcher for the given client end with a handler computed
    /// from a closure on an executor.
    ///
    /// Returns the client and a join handle for the spawned task.
    pub fn spawn_handler_full_on_with<H, E>(
        self,
        create_handler: impl FnOnce(Client<P, T>) -> H,
        executor: &E,
    ) -> (Client<P, T>, HandlerTask<T, H, E>)
    where
        P: DispatchClientMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        let dispatcher = ClientDispatcher::new(self);
        let client = dispatcher.client();
        let handler = create_handler(client.clone());
        (client, executor.spawn(dispatcher.run(handler)))
    }

    /// Spawns a dispatcher for the given client end with a handler on an
    /// executor.
    ///
    /// Returns the client and a join handle for the spawned task.
    pub fn spawn_handler_full_on<H, E>(
        self,
        handler: H,
        executor: &E,
    ) -> (Client<P, T>, HandlerTask<T, H, E>)
    where
        P: DispatchClientMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        self.spawn_handler_full_on_with(|_| handler, executor)
    }

    /// Spawns a dispatcher for the given client end with a handler computed
    /// from a closure on an executor.
    ///
    /// Returns the client.
    pub fn spawn_handler_on_with<H, E>(
        self,
        create_handler: impl FnOnce(Client<P, T>) -> H,
        executor: &E,
    ) -> Client<P, T>
    where
        P: DispatchClientMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        let (client, task) = Self::spawn_handler_full_on_with(self, create_handler, executor);
        executor.detach(task);
        client
    }

    /// Spawns a dispatcher for the given client end with a handler on an
    /// executor.
    ///
    /// Returns the client.
    pub fn spawn_handler_on<H, E>(self, handler: H, executor: &E) -> Client<P, T>
    where
        P: DispatchClientMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        self.spawn_handler_on_with(|_| handler, executor)
    }

    /// Spawns a dispatcher for the given client end with a handler computed
    /// from a closure on the default executor for the transport.
    ///
    /// Returns the client and a join handle for the spawned task.
    pub fn spawn_handler_full_with<H>(
        self,
        create_handler: impl FnOnce(Client<P, T>) -> H,
    ) -> (Client<P, T>, HandlerTask<T, H>)
    where
        P: DispatchClientMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_handler_full_on_with(self, create_handler, &executor)
    }

    /// Spawns a dispatcher for the given client end with a handler on the
    /// default executor for the transport.
    ///
    /// Returns the client and a join handle for the spawned task.
    pub fn spawn_handler_full<H>(self, handler: H) -> (Client<P, T>, HandlerTask<T, H>)
    where
        P: DispatchClientMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        self.spawn_handler_full_with(|_| handler)
    }

    /// Spawns a dispatcher for the given client end with a handler computed
    /// from a closure on the default executor for the transport.
    ///
    /// Returns the client.
    pub fn spawn_handler_with<H>(
        self,
        create_handler: impl FnOnce(Client<P, T>) -> H,
    ) -> Client<P, T>
    where
        P: DispatchClientMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_handler_on_with(self, create_handler, &executor)
    }

    /// Spawns a dispatcher for the given client end with a handler on the
    /// default executor for the transport.
    ///
    /// Returns the client.
    pub fn spawn_handler<H>(self, handler: H) -> Client<P, T>
    where
        P: DispatchClientMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        self.spawn_handler_with(|_| handler)
    }

    /// Spawns a dispatcher for the given client end on an executor.
    ///
    /// The spawned dispatcher will ignore all incoming events. Returns the
    /// client and a join handle for the spawned task.
    pub fn spawn_full_on<E>(self, executor: &E) -> (Client<P, T>, HandlerTask<T, (), E>)
    where
        P: DispatchClientMessage<IgnoreEvents, T>,
        T: 'static,
        E: Executor,
    {
        let dispatcher = ClientDispatcher::new(self);
        let client = dispatcher.client();
        (client, executor.spawn(dispatcher.run_client()))
    }

    /// Spawns a dispatcher for the given client end on an executor.
    ///
    /// The spawned dispatcher will ignore all incoming events. Returns the
    /// client.
    pub fn spawn_on<E>(self, executor: &E) -> Client<P, T>
    where
        P: DispatchClientMessage<IgnoreEvents, T>,
        T: 'static,
        E: Executor,
    {
        let (client, task) = Self::spawn_full_on(self, executor);
        executor.detach(task);
        client
    }

    /// Spawns a dispatcher for the given client end on the default executor for
    /// the transport.
    ///
    /// The spawned dispatcher will ignore all incoming events. Returns the
    /// client and a join handle for the spawned task.
    pub fn spawn_full(self) -> (Client<P, T>, HandlerTask<T, ()>)
    where
        P: DispatchClientMessage<IgnoreEvents, T>,
        T: HasExecutor + 'static,
    {
        let executor = self.executor();
        Self::spawn_full_on(self, &executor)
    }

    /// Spawns a dispatcher for the given client end on the default executor for
    /// the transport.
    ///
    /// The spawned dispatcher will ignore all incoming events. Returns the
    /// client.
    pub fn spawn(self) -> Client<P, T>
    where
        P: DispatchClientMessage<IgnoreEvents, T>,
        T: HasExecutor + 'static,
    {
        let executor = self.executor();
        Self::spawn_on(self, &executor)
    }
}

impl<P, T: Transport> ServerEnd<P, T> {
    /// Spawns a dispatcher for the given server end with a handler computed
    /// from a closure on an executor.
    ///
    /// Returns the join handle for the spawned task and the server.
    pub fn spawn_full_on_with<H, E>(
        self,
        create_handler: impl FnOnce(Server<P, T>) -> H,
        executor: &E,
    ) -> (HandlerTask<T, H, E>, Server<P, T>)
    where
        P: DispatchServerMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        let dispatcher = ServerDispatcher::new(self);
        let server = dispatcher.server();
        let handler = create_handler(server.clone());
        (executor.spawn(dispatcher.run(handler)), server)
    }

    /// Spawns a dispatcher for the given server end with a handler on an
    /// executor.
    ///
    /// Returns the join handle for the spawned task and the server.
    pub fn spawn_full_on<H, E>(
        self,
        handler: H,
        executor: &E,
    ) -> (HandlerTask<T, H, E>, Server<P, T>)
    where
        P: DispatchServerMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        self.spawn_full_on_with(|_| handler, executor)
    }

    /// Spawns a dispatcher for the given server end with a handler computed
    /// from a closure on an executor.
    ///
    /// Returns the join handle for the spawned task.
    pub fn spawn_on_with<H, E>(
        self,
        create_handler: impl FnOnce(Server<P, T>) -> H,
        executor: &E,
    ) -> HandlerTask<T, H, E>
    where
        P: DispatchServerMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        let dispatcher = ServerDispatcher::new(self);
        let handler = create_handler(dispatcher.server());
        executor.spawn(dispatcher.run(handler))
    }

    /// Spawns a dispatcher for the given server end with a handler on an
    /// executor.
    ///
    /// Returns the join handle for the spawned task.
    pub fn spawn_on<H, E>(self, handler: H, executor: &E) -> HandlerTask<T, H, E>
    where
        P: DispatchServerMessage<H, T>,
        T: 'static,
        H: Send + 'static,
        E: Executor,
    {
        self.spawn_on_with(|_| handler, executor)
    }

    /// Spawns a dispatcher for the given server end with a handler computed
    /// from a closure on the default executor for the transport.
    ///
    /// Returns the join handle for the spawned task and the server.
    pub fn spawn_full_with<H>(
        self,
        create_handler: impl FnOnce(Server<P, T>) -> H,
    ) -> (HandlerTask<T, H>, Server<P, T>)
    where
        P: DispatchServerMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_full_on_with(self, create_handler, &executor)
    }

    /// Spawns a dispatcher for the given server end with a handler on the
    /// default executor for the transport.
    ///
    /// Returns the join handle for the spawned task and the server.
    pub fn spawn_full<H>(self, handler: H) -> (HandlerTask<T, H>, Server<P, T>)
    where
        P: DispatchServerMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        self.spawn_full_with(|_| handler)
    }

    /// Spawns a dispatcher for the given server end with a handler computed
    /// from a closure on the default executor for the transport.
    ///
    /// Returns the join handle for the spawned task.
    pub fn spawn_with<H>(self, create_handler: impl FnOnce(Server<P, T>) -> H) -> HandlerTask<T, H>
    where
        P: DispatchServerMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        let executor = self.executor();
        Self::spawn_on_with(self, create_handler, &executor)
    }

    /// Spawns a dispatcher for the given server end with a handler on the
    /// default executor for the transport.
    ///
    /// Returns the join handle for the spawned task.
    pub fn spawn<H>(self, handler: H) -> HandlerTask<T, H>
    where
        P: DispatchServerMessage<H, T>,
        T: HasExecutor + 'static,
        H: Send + 'static,
    {
        self.spawn_with(|_| handler)
    }
}
