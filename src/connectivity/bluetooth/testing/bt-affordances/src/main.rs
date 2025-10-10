// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error};
use fidl_fuchsia_bluetooth_affordances::{
    HostControllerRequest, HostControllerRequestStream, PeerControllerRequest,
    PeerControllerRequestStream,
};
use fuchsia_bt_test_affordances::WorkThread;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt, TryStreamExt};
use log::error;
use std::sync::Arc;

pub enum Services {
    Peer(PeerControllerRequestStream),
    Host(HostControllerRequestStream),
}

async fn handle_peer_request(
    stream: PeerControllerRequestStream,
    worker: Arc<WorkThread>,
) -> Result<(), Error> {
    stream
        .map(|result| result.context("failed request"))
        .try_for_each(|request| {
            let worker = worker.clone();
            async move {
                match request {
                    PeerControllerRequest::GetKnownPeers { responder } => {
                        match worker.get_known_peers().await {
                            Ok(peers) => {
                                responder.send(Ok(peers.as_slice()))?;
                            }
                            Err(err) => {
                                error!("GetKnownPeers encountered error: {err}");
                                responder.send(Err(
                                    fidl_fuchsia_bluetooth_affordances::Error::Internal,
                                ))?;
                            }
                        }
                    }
                    PeerControllerRequest::_UnknownMethod { ordinal, .. } => {
                        error!(
                            "PeerControllerRequest: unknown method received with ordinal {ordinal}"
                        );
                    }
                }
                Ok(())
            }
        })
        .await
}

async fn handle_host_request(
    stream: HostControllerRequestStream,
    worker: Arc<WorkThread>,
) -> Result<(), Error> {
    stream
        .map(|result| result.context("failed request"))
        .try_for_each(|request| {
            let worker = worker.clone();
            async move {
                match request {
                    HostControllerRequest::GetActiveHost { responder } => {
                        match worker.get_active_host().await {
                            Ok(active_host) => {
                                responder.send(Ok(&active_host))?;
                            }
                            Err(err) => {
                                error!("GetActiveHost encountered error: {err}");
                                responder.send(Err(
                                    fidl_fuchsia_bluetooth_affordances::Error::Internal,
                                ))?;
                            }
                        }
                    }
                    HostControllerRequest::_UnknownMethod { ordinal, .. } => {
                        error!(
                            "HostControllerRequest: unknown method received with ordinal {ordinal}"
                        );
                    }
                }
                Ok(())
            }
        })
        .await
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let mut fs = ServiceFs::new_local();
    let _ = fs.dir("svc").add_fidl_service(Services::Peer);
    let _ = fs.dir("svc").add_fidl_service(Services::Host);
    let _ = fs.take_and_serve_directory_handle()?;

    let worker = Arc::new(WorkThread::spawn());

    fs.for_each_concurrent(None, |request| {
        let worker_clone = worker.clone();
        async move {
            match request {
                Services::Peer(stream) => handle_peer_request(stream, worker_clone)
                    .await
                    .unwrap_or_else(|e| error!("{e:?}")),
                Services::Host(stream) => handle_host_request(stream, worker_clone)
                    .await
                    .unwrap_or_else(|e| error!("{e:?}")),
            }
        }
    })
    .await;

    Ok(())
}
