// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A server to handle calculator requests.
//!
//! This component (and the accompying parent realm) is a realistic example of
//! how to create & route client/server components in Fuchsia. It aims to be
//! fully fleshed out and showcase best practices such as:
//!
//! 1. Testing
//! 2. Exposing capabilities
//! 3. Well commented code
//! 4. FIDL interaction
//! 5. Error handling

use anyhow::Context;
use fidl_next::{Request, Responder, ServerSender};
use fidl_next_fuchsia_examples_calculator::{
    Calculator, CalculatorAddResponse, CalculatorDivideResponse, CalculatorMultiplyResponse,
    CalculatorPowResponse, CalculatorServerHandler, CalculatorSubtractResponse, calculator,
};
use fuchsia_component::server::ServiceFs;
use fuchsia_inspect::component;
use fuchsia_inspect::health::Reporter;
use futures::prelude::*;

#[derive(Clone)]
pub struct CalculatorServer;

impl CalculatorServerHandler<fidl_next::fuchsia::zx::Channel> for CalculatorServer {
    async fn add(
        &mut self,
        sender: &ServerSender<Calculator>,
        request: Request<calculator::Add>,
        responder: Responder<calculator::Add>,
    ) {
        responder
            .respond(sender, CalculatorAddResponse { sum: *request.a + *request.b })
            .unwrap()
            .await
            .unwrap();
    }

    async fn subtract(
        &mut self,
        sender: &ServerSender<Calculator>,
        request: Request<calculator::Subtract>,
        responder: Responder<calculator::Subtract>,
    ) {
        responder
            .respond(sender, CalculatorSubtractResponse { difference: *request.a - *request.b })
            .unwrap()
            .await
            .unwrap();
    }

    async fn multiply(
        &mut self,
        sender: &ServerSender<Calculator>,
        request: Request<calculator::Multiply>,
        responder: Responder<calculator::Multiply>,
    ) {
        responder
            .respond(sender, CalculatorMultiplyResponse { product: *request.a * *request.b })
            .unwrap()
            .await
            .unwrap();
    }

    async fn divide(
        &mut self,
        sender: &ServerSender<Calculator>,
        request: Request<calculator::Divide>,
        responder: Responder<calculator::Divide>,
    ) {
        responder
            .respond(
                sender,
                CalculatorDivideResponse { quotient: *request.dividend / *request.divisor },
            )
            .unwrap()
            .await
            .unwrap();
    }

    async fn pow(
        &mut self,
        sender: &ServerSender<Calculator>,
        request: Request<calculator::Pow>,
        responder: Responder<calculator::Pow>,
    ) {
        responder
            .respond(sender, CalculatorPowResponse { power: request.base.powf(*request.exponent) })
            .unwrap()
            .await
            .unwrap();
    }
}

/// Calculator server entry point.
#[fuchsia::main]
async fn main() -> Result<(), anyhow::Error> {
    let mut service_fs = ServiceFs::new_local();

    // Initialize inspect.
    let _inspect_server_task = inspect_runtime::publish(
        component::inspector(),
        inspect_runtime::PublishOptions::default(),
    );
    component::health().set_starting_up();

    service_fs.dir("svc").add_fidl_next_protocol::<Calculator, _>(CalculatorServer);

    service_fs.take_and_serve_directory_handle().context("failed to serve outgoing namespace")?;

    component::health().set_ok();
    log::debug!("Initialized.");

    service_fs.for_each_concurrent(None, |()| async {}).await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::CalculatorServer;

    use fidl_next::Server;
    use fidl_next::fuchsia::{create_channel, spawn_client_sender_detached};
    use fidl_next_fuchsia_examples_calculator::Calculator;
    use futures::FutureExt;
    use std::pin::pin;

    #[fuchsia::test]
    async fn test_add() {
        let (client_end, server_end) = create_channel::<Calculator>();
        let sender = spawn_client_sender_detached(client_end);
        let server_task = pin!(async move { Server::new(server_end).run(CalculatorServer).await });

        let future = sender.add(4.5, 3.2).unwrap();
        futures::select! {
            actual = future.fuse() => {
                let actual = actual.expect("Add proxy didn't return value.");
                assert_eq!(actual.sum, 7.7);
            },
            _ = server_task.fuse() => {
                panic!("server should never complete.")
            }
        }
    }

    #[fuchsia::test]
    async fn test_subtract() {
        let (client_end, server_end) = create_channel::<Calculator>();
        let sender = spawn_client_sender_detached(client_end);
        let server_task = pin!(async move { Server::new(server_end).run(CalculatorServer).await });

        let future = sender.subtract(7.7, 3.2).unwrap();
        futures::select! {
            actual = future.fuse() => {
                let actual = actual.expect("Subtract proxy didn't return value.");
                assert_eq!(actual.difference, 4.5);
            },
            _ = server_task.fuse() => {
                panic!("server should never complete.")
            }
        }
    }

    #[fuchsia::test]
    async fn test_multiply() {
        let (client_end, server_end) = create_channel::<Calculator>();
        let sender = spawn_client_sender_detached(client_end);
        let server_task = pin!(async move { Server::new(server_end).run(CalculatorServer).await });

        let future = sender.multiply(1.5, 2.0).unwrap();
        futures::select! {
            actual = future.fuse() => {
                let actual = actual.expect("Multiply proxy didn't return value.");
                assert_eq!(actual.product, 3.0);
            },
            _ = server_task.fuse() => {
                panic!("server should never complete.")
            }
        }
    }

    #[fuchsia::test]
    async fn test_divide() {
        let (client_end, server_end) = create_channel::<Calculator>();
        let sender = spawn_client_sender_detached(client_end);
        let server_task = pin!(async move { Server::new(server_end).run(CalculatorServer).await });

        let future = sender.divide(2.0, 4.0).unwrap();
        futures::select! {
            actual = future.fuse() => {
                let actual = actual.expect("Divide proxy didn't return value.");
                assert_eq!(actual.quotient, 0.5);
            },
            _ = server_task.fuse() => {
                panic!("server should never complete.")
            }
        }
    }

    #[fuchsia::test]
    async fn test_pow() {
        let (client_end, server_end) = create_channel::<Calculator>();
        let sender = spawn_client_sender_detached(client_end);
        let server_task = pin!(async move { Server::new(server_end).run(CalculatorServer).await });

        let future = sender.pow(3.0, 4.0).unwrap();
        futures::select! {
            actual = future.fuse() => {
                let actual = actual.expect("Pow proxy didn't return value.");
                assert_eq!(actual.power, 81.0);
            },
            _ = server_task.fuse() => {
                panic!("server should never complete.")
            }
        }
    }
}
