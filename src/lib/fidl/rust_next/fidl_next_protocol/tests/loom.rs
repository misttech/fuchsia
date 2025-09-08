// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next_codec::{DecoderExt as _, WireString};
use fidl_next_protocol_loom::mpsc::Mpsc;
use fidl_next_protocol_loom::{
    Client, ClientHandler, ClientSender, Responder, Server, ServerHandler, ServerSender, Transport,
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
        async fn on_one_way(&mut self, _: &ServerSender<T>, _: u64, _: T::RecvBuffer) {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            sender: &ServerSender<T>,
            ordinal: u64,
            buffer: T::RecvBuffer,
            responder: Responder,
        ) {
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(ordinal, 42);
            assert_eq!(&**message, "Ping");

            sender
                .send_response(responder, 42, "Pong")
                .expect("failed to encode response")
                .await
                .expect("failed to send response");
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client = Client::new(client_end);
        let client_sender = client.sender().clone();
        let client_task = spawn(|| block_on(client.run_sender()));
        let server_task = spawn(|| block_on(Server::new(server_end).run(TestServer)));

        let recv_future = block_on(
            client_sender.send_two_way(42, "Ping").expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message, "Pong");

        // Dropping the last client sender should close the connection.
        drop(client_sender);

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn send_one_way() {
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(&mut self, _: &ServerSender<T>, _: u64, _: T::RecvBuffer) {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            sender: &ServerSender<T>,
            ordinal: u64,
            buffer: T::RecvBuffer,
            responder: Responder,
        ) {
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(ordinal, 42);
            assert_eq!(&**message, "Ping");

            sender
                .send_response(responder, 42, "Pong")
                .expect("failed to encode response")
                .await
                .expect("failed to send response");
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client = Client::new(client_end);
        let client_sender = client.sender().clone();
        let client_task = spawn(|| block_on(client.run_sender()));
        let server_task = spawn(|| block_on(Server::new(server_end).run(TestServer)));

        let recv_future = block_on(
            client_sender.send_two_way(42, "Ping").expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message, "Pong");

        client_sender.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn two_way() {
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(&mut self, _: &ServerSender<T>, _: u64, _: T::RecvBuffer) {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            sender: &ServerSender<T>,
            ordinal: u64,
            buffer: T::RecvBuffer,
            responder: Responder,
        ) {
            assert_eq!(ordinal, 42);
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(&**message, "Ping");

            sender
                .send_response(responder, 42, "Pong")
                .expect("failed to encode response")
                .await
                .expect("failed to send response");
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client = Client::new(client_end);
        let client_sender = client.sender().clone();
        let client_task = spawn(|| block_on(client.run_sender()));
        let server_task = spawn(|| block_on(Server::new(server_end).run(TestServer)));

        let recv_future = block_on(
            client_sender.send_two_way(42, "Ping").expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let message = block_on(recv_future)
            .expect("client failed to receive response")
            .decode::<WireString<'_>>()
            .expect("failed to decode response");
        assert_eq!(&**message, "Pong");

        client_sender.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn multiple_two_way() {
    struct TestServer;

    impl<T: Transport + 'static> ServerHandler<T> for TestServer {
        async fn on_one_way(&mut self, _: &ServerSender<T>, _: u64, _: T::RecvBuffer) {
            panic!("unexpected event");
        }

        async fn on_two_way(
            &mut self,
            sender: &ServerSender<T>,
            ordinal: u64,
            buffer: T::RecvBuffer,
            responder: Responder,
        ) {
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");

            let response = match ordinal {
                1 => "One",
                2 => "Two",
                3 => "Three",
                x => panic!("unexpected request ordinal {x} from client"),
            };

            assert_eq!(&**message, response);

            sender
                .send_response(responder, ordinal, response)
                .expect("server failed to encode response")
                .await
                .expect("server failed to send response");
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client = Client::new(client_end);
        let client_sender = client.sender().clone();
        let client_task = spawn(|| block_on(client.run_sender()));
        let server_task = spawn(|| block_on(Server::new(server_end).run(TestServer)));

        let send_one = block_on(
            client_sender.send_two_way(1, "One").expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let send_two = block_on(
            client_sender.send_two_way(2, "Two").expect("client failed to encode request"),
        )
        .expect("client failed to send request");
        let send_three = block_on(
            client_sender.send_two_way(3, "Three").expect("client failed to encode request"),
        )
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

        client_sender.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}

#[test]
fn event() {
    struct TestClient;

    impl<T: Transport> ClientHandler<T> for TestClient {
        async fn on_event(
            &mut self,
            sender: &ClientSender<T>,
            ordinal: u64,
            buffer: T::RecvBuffer,
        ) {
            assert_eq!(ordinal, 10);
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(&**message, "Surprise!");

            sender.close();
        }
    }

    pub struct TestServer;

    impl<T: Transport> ServerHandler<T> for TestServer {
        async fn on_one_way(&mut self, _: &ServerSender<T>, _: u64, _: T::RecvBuffer) {}
        async fn on_two_way(
            &mut self,
            _: &ServerSender<T>,
            _: u64,
            _: T::RecvBuffer,
            _: Responder,
        ) {
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client = Client::new(client_end);
        let client_task = spawn(|| block_on(client.run(TestClient)));
        let server = Server::new(server_end);
        let server_sender = server.sender().clone();
        let server_task = spawn(|| block_on(server.run(TestServer)));

        block_on(
            server_sender.send_event(10, "Surprise!").expect("server failed to encode response"),
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
        async fn on_one_way(&mut self, _: &ServerSender<T>, ordinal: u64, buffer: T::RecvBuffer) {
            assert_eq!(ordinal, 42);
            let message = buffer.decode::<WireString<'_>>().expect("failed to decode request");
            assert_eq!(&**message, "Hello world");
        }

        async fn on_two_way(
            &mut self,
            _: &ServerSender<T>,
            _: u64,
            _: T::RecvBuffer,
            _: Responder,
        ) {
            panic!("unexpected two-way message");
        }
    }

    loom().check(|| {
        let (client_end, server_end) = Mpsc::new();
        let client = Client::new(client_end);
        let client_sender = client.sender().clone();
        let client_task = spawn(|| block_on(client.run_sender()));
        let server_task = spawn(|| block_on(Server::new(server_end).run(TestServer)));

        client_sender
            .send_one_way(42, "Hello world")
            .expect("client failed to encode request")
            .send_immediately()
            .expect("client failed to send request");

        client_sender.close();

        client_task.join().unwrap().expect("client encountered an error");
        server_task.join().unwrap().expect("server encountered an error");
    });
}
