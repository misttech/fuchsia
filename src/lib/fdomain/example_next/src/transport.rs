// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdomain_client::FDomainTransport;
use fdomain_container::FDomain;
use fdomain_container::wire::FDomainCodec;
use fidl::endpoints::Proxy;
use fidl_next::{ClientEnd, Server, ServerEnd};
use fidl_next_fuchsia_examples::echo::prelude::*;
use fidl_next_fuchsia_examples::echo_launcher::prelude::*;
use futures::stream::Stream;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};
use vfs::directory::helper::DirectlyMutable;

pub struct LocalFDomainTransport(FDomainCodec);

impl FDomainTransport for LocalFDomainTransport {
    fn poll_send_message(
        mut self: Pin<&mut Self>,
        msg: &[u8],
        _ctx: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Poll::Ready(self.0.message(msg).map_err(std::io::Error::other))
    }
}

impl Stream for LocalFDomainTransport {
    type Item = std::io::Result<Box<[u8]>>;

    fn poll_next(mut self: Pin<&mut Self>, ctx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        Pin::new(&mut self.as_mut().0).poll_next(ctx).map_err(std::io::Error::other)
    }
}

struct EchoServer {
    prefix: String,
    quiet: bool,
}

impl EchoServerHandler<fidl::Channel> for EchoServer {
    async fn echo_string(
        &mut self,
        request: fidl_next::Request<echo::EchoString, fidl::Channel>,
        responder: fidl_next::Responder<echo::EchoString>,
    ) {
        let value = request.value.to_owned();
        if !self.quiet {
            log::info!("Received echo request for string {:?}", value);
        }
        let sender = responder.sender().clone();
        let response = EchoEchoStringResponse { response: format!("{}{value}", self.prefix) };

        if responder.respond(response).await.is_err() {
            sender.close()
        } else if !self.quiet {
            log::info!("echo response sent successfully");
        }
    }

    async fn send_string(
        &mut self,
        _sender: &fidl_next::ServerSender<Echo, fidl::Channel>,
        _request: fidl_next::Request<echo::SendString, fidl::Channel>,
    ) {
    }
}

struct EchoLauncherServer {
    quiet: bool,
    scope: fuchsia_async::Scope,
}

impl EchoLauncherServerHandler<fidl::Channel> for EchoLauncherServer {
    async fn get_echo(
        &mut self,
        request: fidl_next::Request<echo_launcher::GetEcho, fidl::Channel>,
        responder: fidl_next::Responder<echo_launcher::GetEcho>,
    ) {
        let prefix = request.echo_prefix.to_owned();

        if !self.quiet {
            log::info!("Received echo launcher request with prefix string {:?}", prefix);
        }

        let (client, server) = fidl::Channel::create();
        let server = ServerEnd::<Echo, _>::from_untyped(server);
        let client = ClientEnd::<Echo, _>::from_untyped(client);
        let server = Server::new(server);
        let quiet = self.quiet;
        self.scope.spawn(async move {
            server.run(EchoServer { prefix, quiet }).await.unwrap();
        });
        let response = EchoLauncherGetEchoResponse { response: client };

        let sender = responder.sender().clone();
        if responder.respond(response).await.is_err() {
            sender.close();
        } else if !quiet {
            log::info!("echo launcher response sent successfully");
        }
    }

    async fn get_echo_pipelined(
        &mut self,
        _sender: &fidl_next::ServerSender<EchoLauncher, fidl::Channel>,
        request: fidl_next::Request<echo_launcher::GetEchoPipelined, fidl::Channel>,
    ) {
        let EchoLauncherGetEchoPipelinedRequest { echo_prefix, request } = request.take();

        if !self.quiet {
            log::info!(
                "Received pipelined echo launcher request with prefix string {:?}",
                echo_prefix
            );
        }

        let server = Server::new(request);
        let quiet = self.quiet;
        self.scope.spawn(async move {
            server.run(EchoServer { prefix: echo_prefix, quiet }).await.unwrap();
        });
    }
}

pub fn exec_server(quiet: bool) -> LocalFDomainTransport {
    let service = vfs::service::endpoint(move |scope, channel| {
        log::info!("Spawned endpoint");
        let endpoint = ServerEnd::<EchoLauncher, _>::from_untyped(channel.into_zx_channel());
        let server = Server::new(endpoint);
        scope.spawn(async move {
            let ret =
                server.run(EchoLauncherServer { quiet, scope: fuchsia_async::Scope::new() }).await;

            if let Err(e) = ret {
                log::warn!(error:? = e; "Echo server terminated");
            }
        });
    });
    let namespace = vfs::directory::immutable::simple();
    namespace.add_entry("echo", service).expect("Could not build namespace!");
    LocalFDomainTransport(FDomainCodec::new(FDomain::new(move || {
        log::info!("Spawning vfs client");
        Ok(fidl::endpoints::ClientEnd::new(
            vfs::directory::serve_read_only(Arc::clone(&namespace))
                .into_channel()
                .unwrap()
                .into_zx_channel(),
        ))
    })))
}
