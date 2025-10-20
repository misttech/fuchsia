// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(feature = "loom")]

use core::mem::MaybeUninit;

use fidl_next_codec::{
    Decode, Decoder, DecoderExt as _, Encodable, Encode, EncodeError, Encoder, Unconstrained, Wire,
    WireString, munge,
};
use fidl_next_protocol_loom::mpsc::Mpsc;
use fidl_next_protocol_loom::{
    Client, ClientDispatcher, ClientHandler, Flexibility, ProtocolError, Responder,
    ServerDispatcher, ServerHandler, Transport,
};
use loom::future::block_on;
use loom::thread::spawn;

struct IgnoreEvents;

impl<T: Transport> ClientHandler<T> for IgnoreEvents {
    async fn on_event(
        &mut self,
        _: u64,
        _: Flexibility,
        _: T::RecvBuffer,
    ) -> Result<(), ProtocolError<T::Error>> {
        Ok(())
    }
}

fn loom() -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(1);
    builder
}

#[repr(transparent)]
struct WireTestMessage<'a>(WireString<'a>);

impl<'a> WireTestMessage<'a> {
    fn as_str(&'a self) -> &'a str {
        self.0.as_str()
    }
}

impl Unconstrained for WireTestMessage<'_> {}

unsafe impl Wire for WireTestMessage<'static> {
    type Decoded<'de> = WireTestMessage<'de>;

    fn zero_padding(out: &mut MaybeUninit<Self>) {
        munge!(let Self (s) = out);
        WireString::zero_padding(s);
    }
}

unsafe impl<D: Decoder> Decode<D> for WireTestMessage<'static> {
    fn decode(
        slot: fidl_next_codec::Slot<'_, Self>,
        decoder: &mut D,
        _: (),
    ) -> Result<(), fidl_next_codec::DecodeError> {
        munge!(let Self (s) = slot);
        WireString::decode(s, decoder, 1000)
    }
}

struct TestMessage(String);
impl TestMessage {
    fn new(s: &str) -> Self {
        Self(s.to_string())
    }
}

impl Encodable for TestMessage {
    type Encoded = WireTestMessage<'static>;
}

unsafe impl<E: Encoder + ?Sized> Encode<E> for TestMessage {
    fn encode(
        self,
        encoder: &mut E,
        out: &mut MaybeUninit<Self::Encoded>,
        _: (),
    ) -> Result<(), EncodeError> {
        munge!(let WireTestMessage (s) = out);
        self.0.as_str().encode(encoder, s, 1000)
    }
}

#[test]
fn close_on_drop() {
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: Flexibility,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            ordinal: u64,
            _: Flexibility,
            buffer: T::RecvBuffer,
            responder: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");
            assert_eq!(ordinal, 42);
            assert_eq!(message.as_str(), "Ping");

            responder
                .respond(42, Flexibility::Strict, TestMessage::new("Pong"))
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client();
        let client_task = spawn(|| block_on(client_dispatcher.run(IgnoreEvents)));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let recv_future = block_on(
            client
                .send_two_way(42, Flexibility::Strict, TestMessage::new("Ping"))
                .expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireTestMessage<'_>>()
            .expect("failed to decode response");
        assert_eq!(message.as_str(), "Pong");

        // Dropping the last client should close the connection.
        drop(client);

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn send_one_way() {
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: Flexibility,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            ordinal: u64,
            _: Flexibility,
            buffer: T::RecvBuffer,
            responder: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");
            assert_eq!(ordinal, 42);
            assert_eq!(message.as_str(), "Ping");

            responder
                .respond(42, Flexibility::Strict, TestMessage::new("Pong"))
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client();
        let client_task = spawn(|| block_on(client_dispatcher.run(IgnoreEvents)));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let recv_future = block_on(
            client
                .send_two_way(42, Flexibility::Strict, TestMessage::new("Ping"))
                .expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireTestMessage<'_>>()
            .expect("failed to decode response");
        assert_eq!(message.as_str(), "Pong");

        client.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn two_way() {
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: Flexibility,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            ordinal: u64,
            _: Flexibility,
            buffer: T::RecvBuffer,
            responder: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            assert_eq!(ordinal, 42);
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");
            assert_eq!(message.as_str(), "Ping");

            responder
                .respond(42, Flexibility::Strict, TestMessage::new("Pong"))
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client();
        let client_task = spawn(|| block_on(client_dispatcher.run(IgnoreEvents)));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let recv_future = block_on(
            client
                .send_two_way(42, Flexibility::Strict, TestMessage::new("Ping"))
                .expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireTestMessage<'_>>()
            .expect("failed to decode response");
        assert_eq!(message.as_str(), "Pong");

        client.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn multiple_two_way() {
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: Flexibility,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            ordinal: u64,
            _: Flexibility,
            buffer: T::RecvBuffer,
            responder: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");

            let response = match ordinal {
                1 => "One",
                2 => "Two",
                3 => "Three",
                x => panic!("unexpected request ordinal {x} from client"),
            };

            assert_eq!(message.as_str(), response);

            responder
                .respond(ordinal, Flexibility::Strict, TestMessage::new(response))
                .expect("server failed to encode response")
                .await
                .expect("server failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client();
        let client_task = spawn(|| block_on(client_dispatcher.run(IgnoreEvents)));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let send_one = block_on(
            client
                .send_two_way(1, Flexibility::Strict, TestMessage::new("One"))
                .expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let send_two = block_on(
            client
                .send_two_way(2, Flexibility::Strict, TestMessage::new("Two"))
                .expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let send_three = block_on(
            client
                .send_two_way(3, Flexibility::Strict, TestMessage::new("Three"))
                .expect("client failed to encode request"),
        )
        .expect("client failed to send request");

        let (response_one, response_two, response_three) =
            block_on(futures::future::join3(send_one, send_two, send_three));

        let message_one = response_one
            .expect("client failed to receive response")
            .decode::<WireTestMessage<'_>>()
            .expect("failed to decode response");
        assert_eq!(message_one.as_str(), "One");

        let message_two = response_two
            .expect("client failed to receive response")
            .decode::<WireTestMessage<'_>>()
            .expect("failed to decode response");
        assert_eq!(message_two.as_str(), "Two");

        let message_three = response_three
            .expect("client failed to receive response")
            .decode::<WireTestMessage<'_>>()
            .expect("failed to decode response");
        assert_eq!(message_three.as_str(), "Three");

        client.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn event() {
    struct TestClient<T: Transport> {
        client: Client<T>,
    }

    impl<T: Transport> ClientHandler<T> for TestClient<T> {
        async fn on_event(
            &mut self,
            ordinal: u64,
            _: Flexibility,
            buffer: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            assert_eq!(ordinal, 10);
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");
            assert_eq!(message.as_str(), "Surprise!");

            self.client.close();

            Ok(())
        }
    }

    pub struct TestServer;

    impl<T: Transport> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: Flexibility,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            Ok(())
        }

        async fn on_two_way(
            &mut self,
            _: u64,
            _: Flexibility,
            _: T::RecvBuffer,
            _: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client();
        let client_task = spawn(|| block_on(client_dispatcher.run(TestClient { client })));
        let server_dispatcher = ServerDispatcher::new(server_end);
        let server = server_dispatcher.server();
        let server_task = spawn(|| block_on(server_dispatcher.run(TestServer)));

        block_on(
            server
                .send_event(10, Flexibility::Strict, TestMessage::new("Surprise!"))
                .expect("server failed to encode response"),
        )
        .expect("server failed to send response");

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn one_way_nonblocking() {
    struct TestServer;

    impl<T: Transport> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            ordinal: u64,
            _: Flexibility,
            buffer: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            assert_eq!(ordinal, 42);
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");
            assert_eq!(message.as_str(), "Hello world");

            Ok(())
        }

        async fn on_two_way(
            &mut self,
            _: u64,
            _: Flexibility,
            _: T::RecvBuffer,
            _: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected two-way message");
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client();
        let client_task = spawn(|| block_on(client_dispatcher.run(IgnoreEvents)));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        client
            .send_one_way(42, Flexibility::Strict, TestMessage::new("Hello world"))
            .expect("client failed to encode request")
            .send_immediately()
            .expect("client failed to send request");

        client.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}
