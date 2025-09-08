// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_next::{
    ClientEnd, ClientSender, Flexible, FlexibleResult, Request, Responder, Response, ServerEnd,
    ServerSender, Transport,
};
use fidl_next_examples_calculator::calculator::prelude::*;

struct MyCalculatorClient {
    error: Option<u32>,
}

impl<T: Transport> CalculatorClientHandler<T> for MyCalculatorClient {
    async fn on_error(
        &mut self,
        sender: &ClientSender<Calculator, T>,
        response: Response<calculator::OnError, T>,
    ) {
        self.error = Some(*response.status_code);
        sender.close();
    }
}

impl MyCalculatorClient {
    pub fn new() -> Self {
        Self { error: None }
    }
}

struct MyCalculatorServer {
    last_result: Option<i32>,
}

impl MyCalculatorServer {
    pub fn new() -> Self {
        Self { last_result: None }
    }
}

impl<T: Transport + 'static> CalculatorServerHandler<T> for MyCalculatorServer {
    async fn add(
        &mut self,
        sender: &ServerSender<Calculator, T>,
        request: Request<calculator::Add, T>,
        responder: Responder<calculator::Add>,
    ) {
        let sum = request.a + request.b;
        self.last_result = Some(sum);

        let response = Flexible::Ok(CalculatorAddResponse { sum });
        let _ = responder.respond(&sender, response).await;
    }

    async fn divide(
        &mut self,
        sender: &ServerSender<Calculator, T>,
        request: Request<calculator::Divide, T>,
        responder: Responder<calculator::Divide>,
    ) {
        let response = if request.divisor != 0 {
            let quotient = request.dividend / request.divisor;
            self.last_result = Some(quotient);

            FlexibleResult::Ok(CalculatorDivideResponse {
                quotient: request.dividend / request.divisor,
                remainder: request.dividend % request.divisor,
            })
        } else {
            FlexibleResult::Err(DivisionError::DivideByZero)
        };

        let _ = responder.respond(&sender, response).await;
    }

    async fn clear(&mut self, _: &ServerSender<Calculator, T>) {
        self.last_result = None;
    }
}

#[cfg(not(target_os = "fuchsia"))]
type Endpoint = fidl_next::fuchsia_async::Mpsc;

#[cfg(target_os = "fuchsia")]
type Endpoint = zx::Channel;

fn create_endpoints() -> (ClientEnd<Calculator, Endpoint>, ServerEnd<Calculator, Endpoint>) {
    #[cfg(not(target_os = "fuchsia"))]
    {
        fidl_next::fuchsia_async::Mpsc::new()
    }
    #[cfg(target_os = "fuchsia")]
    {
        fidl_next::fuchsia::create_channel()
    }
}

async fn add(client_sender: &ClientSender<Calculator, Endpoint>) {
    let result = client_sender.add(16, 26).await.expect("failed to send or receive request");
    let response = result.ok().expect("add request failed with an error");

    assert_eq!(response.sum, 42);
}

async fn divide(client_sender: &ClientSender<Calculator, Endpoint>) {
    // Normal division
    let result = client_sender.divide(100, 3).await.expect("failed to send or receive request");
    let response = result.ok().expect("divide request failed with an error");

    assert_eq!(response.quotient, 33);
    assert_eq!(response.remainder, 1);

    // Cause an error
    let result = client_sender.divide(42, 0).await.expect("failed to send or receive request");

    let error = result.err().expect("divide request succeeded unexpectedly");
    assert_eq!(DivisionError::DivideByZero, (*error).into());
}

async fn clear(client_sender: &ClientSender<Calculator, Endpoint>) {
    client_sender.clear().await.expect("failed to send request");
}

async fn on_error(server_sender: &ServerSender<Calculator, Endpoint>) {
    server_sender.on_error(100u32).await.expect("failed to send event");
}

#[fuchsia_async::run_singlethreaded]
async fn main() {
    let (client_end, server_end) = create_endpoints();
    let (client_sender, client_task) =
        client_end.spawn_full_with_handler(MyCalculatorClient::new());
    let (server_task, server_sender) = server_end.spawn_full(MyCalculatorServer::new());

    add(&client_sender).await;
    divide(&client_sender).await;
    clear(&client_sender).await;
    on_error(&server_sender).await;

    client_task.await.unwrap();
    server_task.await.unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_add() {
        let (client_end, server_end) = create_endpoints();
        let (client_sender, client_task) =
            client_end.spawn_full_with_handler(MyCalculatorClient::new());
        let server_task = server_end.spawn(MyCalculatorServer::new());

        add(&client_sender).await;

        client_sender.close();

        assert_eq!(client_task.await.unwrap().error, None);
        assert_eq!(server_task.await.unwrap().last_result, Some(42));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_divide() {
        let (client_end, server_end) = create_endpoints();
        let (client_sender, client_task) =
            client_end.spawn_full_with_handler(MyCalculatorClient::new());
        let server_task = server_end.spawn(MyCalculatorServer::new());

        divide(&client_sender).await;

        client_sender.close();

        assert_eq!(client_task.await.unwrap().error, None);
        assert_eq!(server_task.await.unwrap().last_result, Some(33));
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_clear() {
        let (client_end, server_end) = create_endpoints();
        let (client_sender, client_task) =
            client_end.spawn_full_with_handler(MyCalculatorClient::new());
        let server_task = server_end.spawn(MyCalculatorServer::new());

        add(&client_sender).await;
        clear(&client_sender).await;

        client_sender.close();

        assert_eq!(client_task.await.unwrap().error, None);
        assert_eq!(server_task.await.unwrap().last_result, None);
    }

    #[fuchsia_async::run_singlethreaded(test)]
    async fn test_on_error() {
        let (client_end, server_end) = create_endpoints();
        let (client_sender, client_task) =
            client_end.spawn_full_with_handler(MyCalculatorClient::new());
        let (server_task, server_sender) = server_end.spawn_full(MyCalculatorServer::new());

        on_error(&server_sender).await;

        client_sender.close();

        assert_eq!(client_task.await.unwrap().error, Some(100));
        assert_eq!(server_task.await.unwrap().last_result, None);
    }
}
