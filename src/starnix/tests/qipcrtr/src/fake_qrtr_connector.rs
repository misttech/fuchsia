// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fidl_fuchsia_hardware_qualcomm_router as fqrtr;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::LocalComponentHandles;
use futures::prelude::*;
use zx::{self, Peered};

pub async fn mock_qrtr_client_service(handles: LocalComponentHandles) -> Result<(), Error> {
    let mut fs = ServiceFs::new();
    fs.dir("svc").add_fidl_service(|stream: fqrtr::QrtrConnectorRequestStream| stream);
    fs.serve_connection(handles.outgoing_dir)?;

    fs.for_each_concurrent(0, |stream| async move {
        run_qrtr_connector_server(stream)
            .await
            .unwrap_or_else(|e| eprintln!("Error while serving QrtrConnector: {:?}", e))
    })
    .await;

    Ok(())
}

async fn run_qrtr_connector_server(stream: fqrtr::QrtrConnectorRequestStream) -> Result<(), Error> {
    stream
        .map(|result| result.context("failed request"))
        .try_for_each(|request| async move {
            match request {
                fqrtr::QrtrConnectorRequest::GetConnection { options, proxy, responder } => {
                    qrtr_connector_get_connection(options, proxy, responder)
                }
                _ => unreachable!("Unexpected QrtrConnectorRequest"),
            }
        })
        .await
}

fn qrtr_connector_get_connection(
    _options: fqrtr::ConnectionOptions,
    proxy: fidl::endpoints::ServerEnd<fqrtr::QrtrClientConnectionMarker>,
    responder: fqrtr::QrtrConnectorGetConnectionResponder,
) -> Result<(), Error> {
    responder.send(Ok(())).context("sending GetConnection response")?;
    fuchsia_async::Task::spawn(async move {
        if let Err(e) = run_qrtr_client_connection_server(proxy.into_stream()).await {
            eprintln!("Error while serving QrtrClientConnection: {:?}", e);
        }
    })
    .detach();
    Ok(())
}

async fn run_qrtr_client_connection_server(
    mut stream: fqrtr::QrtrClientConnectionRequestStream,
) -> Result<(), Error> {
    while let Some(Ok(request)) = stream.next().await {
        match request {
            fqrtr::QrtrClientConnectionRequest::GetNodeId { responder } => {
                let _ = responder.send(1);
            }
            fqrtr::QrtrClientConnectionRequest::GetPortId { responder } => {
                let _ = responder.send(10);
            }
            fqrtr::QrtrClientConnectionRequest::GetSignals { responder } => {
                let (client, server) = fidl::EventPair::create();
                let _ = server.signal_peer(
                    zx::Signals::NONE,
                    zx::Signals::from_bits_truncate(
                        fqrtr::SIGNAL_READABLE | fqrtr::SIGNAL_WRITABLE,
                    ),
                );
                let _ = responder.send(client);
            }
            fqrtr::QrtrClientConnectionRequest::Read { responder } => {
                let _ = responder.send(Ok((2, 20, b"recv_data")));
            }
            fqrtr::QrtrClientConnectionRequest::Write { responder, .. } => {
                let _ = responder.send(Ok(()));
            }
            fqrtr::QrtrClientConnectionRequest::CloseConnection { responder } => {
                let _ = responder.send();
                return Ok(());
            }
            _ => {
                // Ignore unknown methods
                return Err(anyhow::anyhow!("Unknown QrtrClientConnectionRequest"));
            }
        }
    }
    Ok(())
}
