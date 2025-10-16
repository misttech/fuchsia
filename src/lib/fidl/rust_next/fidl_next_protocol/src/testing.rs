// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use core::mem::MaybeUninit;
use fidl_next_codec::{
    Chunk, Constrained, Decode, Decoded, Decoder, DecoderExt as _, Encodable, Encode, EncodeError,
    Encoder, EncoderExt as _, Unconstrained, Wire, WireString, munge,
};
use fuchsia_async::Task;

use crate::{
    Client, ClientDispatcher, ClientHandler, NonBlockingTransport, ProtocolError, Responder,
    ServerDispatcher, ServerHandler, Transport,
};

pub fn assert_encoded<T: Encode<Vec<Chunk>>>(value: T, chunks: &[Chunk])
where
    T::Encoded: Constrained<Constraint = ()>,
{
    let mut encoded_chunks = Vec::new();
    encoded_chunks.encode_next(value, ()).unwrap();
    assert_eq!(encoded_chunks, chunks, "encoded chunks did not match");
}

pub fn assert_decoded<T: for<'a> Decode<&'a mut [Chunk]> + Constrained<Constraint = ()>>(
    mut chunks: &mut [Chunk],
    f: impl FnOnce(Decoded<T, &mut [Chunk]>),
) {
    let value = (&mut chunks).decode::<T>().expect("failed to decode");
    f(value)
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

pub async fn test_close_on_drop<T>(make_ends: impl FnOnce() -> (T, T))
where
    T: Transport + 'static,
{
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            ordinal: u64,
            buffer: T::RecvBuffer,
            responder: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");
            assert_eq!(ordinal, 42);
            assert_eq!(message.as_str(), "Ping");

            responder
                .respond(42, TestMessage::new("Pong"))
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    let (client_end, server_end) = make_ends();
    let client_dispatcher = ClientDispatcher::new(client_end);
    let client = client_dispatcher.client();
    let client_task = Task::spawn(client_dispatcher.run_client());
    let server_task = Task::spawn(ServerDispatcher::new(server_end).run(TestServer));

    let message = client
        .send_two_way(42, TestMessage::new("Ping"))
        .expect("client failed to encode request")
        .await
        .expect("client failed to send request")
        .await
        .expect("client failed to receive response")
        .decode::<WireTestMessage<'_>>()
        .expect("failed to decode response");
    assert_eq!(message.as_str(), "Pong");

    // Dropping the last client should close the connection.
    drop(client);

    client_task.await.expect("client encountered an error");
    server_task.await.expect("server encountered an error");
}

pub async fn test_one_way<T>(make_ends: impl FnOnce() -> (T, T))
where
    T: Transport + 'static,
{
    struct TestServer;

    impl<T: Transport> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            ordinal: u64,
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
            _: T::RecvBuffer,
            _: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected two-way message");
        }
    }

    let (client_end, server_end) = make_ends();
    let client_dispatcher = ClientDispatcher::new(client_end);
    let client = client_dispatcher.client();
    let client_task = Task::spawn(client_dispatcher.run_client());
    let server_task = Task::spawn(ServerDispatcher::new(server_end).run(TestServer));

    client
        .send_one_way(42, TestMessage::new("Hello world"))
        .expect("client failed to encode request")
        .await
        .expect("client failed to send request");

    client.close();

    client_task.await.expect("client encountered an error");
    server_task.await.expect("server encountered an error");
}

pub async fn test_two_way<T>(make_ends: impl FnOnce() -> (T, T))
where
    T: Transport + 'static,
{
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            ordinal: u64,
            buffer: T::RecvBuffer,
            responder: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            assert_eq!(ordinal, 42);
            let message = buffer.decode::<WireTestMessage<'_>>().expect("failed to decode request");
            assert_eq!(message.as_str(), "Ping");

            responder
                .respond(42, TestMessage::new("Pong"))
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    let (client_end, server_end) = make_ends();
    let client_dispatcher = ClientDispatcher::new(client_end);
    let client = client_dispatcher.client();
    let client_task = Task::spawn(client_dispatcher.run_client());
    let server_task = Task::spawn(ServerDispatcher::new(server_end).run(TestServer));

    let message = client
        .send_two_way(42, TestMessage::new("Ping"))
        .expect("client failed to encode request")
        .await
        .expect("client failed to send request")
        .await
        .expect("client failed to receive response")
        .decode::<WireTestMessage<'_>>()
        .expect("failed to decode response");
    assert_eq!(message.as_str(), "Pong");

    client.close();

    client_task.await.expect("client encountered an error");
    server_task.await.expect("server encountered an error");
}

pub async fn test_multiple_two_way<T>(make_ends: impl FnOnce() -> (T, T))
where
    T: Transport + 'static,
{
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            _: u64,
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            ordinal: u64,
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
                .respond(ordinal, TestMessage::new(response))
                .expect("server failed to encode response")
                .await
                .expect("server failed to send response");

            Ok(())
        }
    }

    let (client_end, server_end) = make_ends();
    let client_dispatcher = ClientDispatcher::new(client_end);
    let client = client_dispatcher.client();
    let client_task = Task::spawn(client_dispatcher.run_client());
    let server_task = Task::spawn(ServerDispatcher::new(server_end).run(TestServer));

    let send_one = client
        .send_two_way(1, TestMessage::new("One"))
        .expect("client failed to encode request")
        .await
        .expect("client failed to send request");
    let send_two = client
        .send_two_way(2, TestMessage::new("Two"))
        .expect("client failed to encode request")
        .await
        .expect("client failed to send request");
    let send_three = client
        .send_two_way(3, TestMessage::new("Three"))
        .expect("client failed to encode request")
        .await
        .expect("client failed to send request");
    let (response_one, response_two, response_three) =
        futures::join!(send_one, send_two, send_three);

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

    client_task.await.expect("client encountered an error");
    server_task.await.expect("server encountered an error");
}

pub async fn test_event<T>(make_ends: impl FnOnce() -> (T, T))
where
    T: Transport + 'static,
{
    struct TestClient<T: Transport> {
        client: Client<T>,
    }

    impl<T: Transport> ClientHandler<T> for TestClient<T> {
        async fn on_event(
            &mut self,
            ordinal: u64,
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
            _: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            Ok(())
        }

        async fn on_two_way(
            &mut self,
            _: u64,
            _: T::RecvBuffer,
            _: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            Ok(())
        }
    }

    let (client_end, server_end) = make_ends();
    let client_dispatcher = ClientDispatcher::new(client_end);
    let client = client_dispatcher.client();
    let client_task = Task::spawn(client_dispatcher.run(TestClient { client }));
    let server_dispatcher = ServerDispatcher::new(server_end);
    let server = server_dispatcher.server();
    let server_task = Task::spawn(server_dispatcher.run(TestServer));

    server
        .send_event(10, TestMessage::new("Surprise!"))
        .expect("server failed to encode response")
        .await
        .expect("server failed to send response");

    client_task.await.expect("client encountered an error");
    server_task.await.expect("server encountered an error");
}

pub async fn test_one_way_nonblocking<T>(make_ends: impl FnOnce() -> (T, T))
where
    T: NonBlockingTransport + 'static,
{
    struct TestServer;

    impl<T: Transport> ServerHandler<T> for TestServer {
        async fn on_one_way(
            &mut self,
            ordinal: u64,
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
            _: T::RecvBuffer,
            _: Responder<T>,
        ) -> Result<(), ProtocolError<T::Error>> {
            panic!("unexpected two-way message");
        }
    }

    let (client_end, server_end) = make_ends();
    let client_dispatcher = ClientDispatcher::new(client_end);
    let client = client_dispatcher.client();
    let client_task = Task::spawn(client_dispatcher.run_client());
    let server_task = Task::spawn(ServerDispatcher::new(server_end).run(TestServer));

    client
        .send_one_way(42, TestMessage::new("Hello world"))
        .expect("client failed to encode request")
        .send_immediately()
        .expect("client failed to send request");

    client.close();

    client_task.await.expect("client encountered an error");
    server_task.await.expect("server encountered an error");
}
