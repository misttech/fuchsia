// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(feature = "loom")]

use fidl_next_codec::{DecoderExt as _, WireString};
use fidl_next_protocol_loom::mpsc::Mpsc;
use fidl_next_protocol_loom::{
    Client, ClientDispatcher, ClientHandler, ProtocolError, Responder, ServerDispatcher,
    ServerHandler, Transport,
};
use loom::future::block_on;
use loom::thread::spawn;

fn loom() -> loom::model::Builder {
    let mut builder = loom::model::Builder::new();
    builder.preemption_bound = Some(1);
    builder
}

#[test]
fn close_on_drop() {
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
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(ordinal, 42);
            assert_eq!(&**message, "Ping");

            responder
                .respond(42, "Pong")
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client().clone();
        let client_task = spawn(|| block_on(client_dispatcher.run_client()));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let recv_future =
            block_on(client.send_two_way(42, "Ping").expect("client failed to encode request"))
                .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message, "Pong");

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
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(ordinal, 42);
            assert_eq!(&**message, "Ping");

            responder
                .respond(42, "Pong")
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client().clone();
        let client_task = spawn(|| block_on(client_dispatcher.run_client()));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let recv_future =
            block_on(client.send_two_way(42, "Ping").expect("client failed to encode request"))
                .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message, "Pong");

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
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(&**message, "Ping");

            responder
                .respond(42, "Pong")
                .expect("failed to encode response")
                .await
                .expect("failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client().clone();
        let client_task = spawn(|| block_on(client_dispatcher.run_client()));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let recv_future =
            block_on(client.send_two_way(42, "Ping").expect("client failed to encode request"))
                .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message, "Pong");

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
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");

            let response = match ordinal {
                1 => "One",
                2 => "Two",
                3 => "Three",
                x => panic!("unexpected request ordinal {x} from client"),
            };

            assert_eq!(&**message, response);

            responder
                .respond(ordinal, response)
                .expect("server failed to encode response")
                .await
                .expect("server failed to send response");

            Ok(())
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client().clone();
        let client_task = spawn(|| block_on(client_dispatcher.run_client()));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        let send_one =
            block_on(client.send_two_way(1, "One").expect("client failed to encode request"))
                .expect("client failed to send request");
        let send_two =
            block_on(client.send_two_way(2, "Two").expect("client failed to encode request"))
                .expect("client failed to send request");
        let send_three =
            block_on(client.send_two_way(3, "Three").expect("client failed to encode request"))
                .expect("client failed to send request");

        let (response_one, response_two, response_three) =
            block_on(futures::future::join3(send_one, send_two, send_three));

        let message_one = response_one
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message_one, "One");

        let message_two = response_two
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message_two, "Two");

        let message_three = response_three
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message_three, "Three");

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
            buffer: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            assert_eq!(ordinal, 10);
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(&**message, "Surprise!");

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

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client().clone();
        let client_task = spawn(|| block_on(client_dispatcher.run(TestClient { client })));
        let server_dispatcher = ServerDispatcher::new(server_end);
        let server = server_dispatcher.server().clone();
        let server_task = spawn(|| block_on(server_dispatcher.run(TestServer)));

        block_on(server.send_event(10, "Surprise!").expect("server failed to encode response"))
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
            buffer: T::RecvBuffer,
        ) -> Result<(), ProtocolError<T::Error>> {
            assert_eq!(ordinal, 42);
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(&**message, "Hello world");

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

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client_dispatcher = ClientDispatcher::new(client_end);
        let client = client_dispatcher.client().clone();
        let client_task = spawn(|| block_on(client_dispatcher.run_client()));
        let server_task = spawn(|| block_on(ServerDispatcher::new(server_end).run(TestServer)));

        client
            .send_one_way(42, "Hello world")
            .expect("client failed to encode request")
            .send_immediately()
            .expect("client failed to send request");

        client.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}
