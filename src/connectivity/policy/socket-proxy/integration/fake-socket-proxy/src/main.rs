// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use anyhow::Context as _;
use fuchsia_component::server::ServiceFs;
use futures::channel::mpsc;
use futures::lock::Mutex;
use futures::stream::{StreamExt as _, TryStreamExt as _};
use futures::SinkExt as _;
use log::{debug, error};
use {
    fidl_fuchsia_net_policy_properties as fnp_properties,
    fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy,
    fidl_fuchsia_net_policy_testing as fnp_testing,
};

async fn handle_provider(
    tx: mpsc::Sender<fnp_properties::DefaultNetworkUpdate>,
    rs: fnp_testing::FakeSocketProxy_RequestStream,
) -> Result<(), anyhow::Error> {
    rs.map(|r| r.context("fidl error"))
        .try_for_each(|req| {
            let mut tx = tx.clone();
            async move {
                match req {
                    fnp_testing::FakeSocketProxy_Request::UpdateDefaultNetwork {
                        update,
                        responder,
                    } => {
                        tx.send(update).await?;
                        responder.send()?;
                    }
                }
                Ok(())
            }
        })
        .await
}

async fn handle_default_network_watcher(
    rx: Arc<Mutex<mpsc::Receiver<fnp_properties::DefaultNetworkUpdate>>>,
    rs: fnp_properties::DefaultNetworkWatcherRequestStream,
) -> Result<(), anyhow::Error> {
    rs.map(|r| r.context("fidl error"))
        .try_for_each(|req| {
            let rx = rx.clone();
            async move {
                let mut rx = rx.try_lock().expect("only one default network watcher at once");
                match req {
                    fnp_properties::DefaultNetworkWatcherRequest::Watch { responder } => {
                        if let Some(upd) = rx.next().await {
                            responder.send(&upd)?;
                        }
                    }
                    _ => {}
                }
                Ok(())
            }
        })
        .await
}

async fn handle_dns_server_watcher(
    rs: fnp_socketproxy::DnsServerWatcherRequestStream,
) -> Result<(), anyhow::Error> {
    let responders = Arc::new(Mutex::new(Vec::new()));
    rs.map(|r| r.context("fidl error"))
        .try_for_each(|req| {
            let responders = responders.clone();
            async move {
                match req {
                    fnp_socketproxy::DnsServerWatcherRequest::WatchServers { responder } => {
                        responders.lock().await.push(responder);
                    }
                    fnp_socketproxy::DnsServerWatcherRequest::CheckPresence { responder } => {
                        responder.send()?;
                    }
                }

                Ok(())
            }
        })
        .await
}

async fn handle_fuchsia_networks(
    rs: fnp_socketproxy::FuchsiaNetworksRequestStream,
) -> Result<(), anyhow::Error> {
    rs.map(|r| r.context("fidl error"))
        .try_for_each(|req| async move {
            match req {
                fnp_socketproxy::FuchsiaNetworksRequest::SetDefault {
                    network_id: _,
                    responder,
                } => responder.send(Ok(()))?,
                fnp_socketproxy::FuchsiaNetworksRequest::Add { network: _, responder } => {
                    responder.send(Ok(()))?;
                }
                fnp_socketproxy::FuchsiaNetworksRequest::Update { network: _, responder } => {
                    responder.send(Ok(()))?;
                }
                fnp_socketproxy::FuchsiaNetworksRequest::Remove { network_id: _, responder } => {
                    responder.send(Ok(()))?;
                }
                fnp_socketproxy::FuchsiaNetworksRequest::CheckPresence { responder } => {
                    responder.send()?;
                }
            }

            Ok(())
        })
        .await
}

enum IncomingServices {
    FakeSocketProxy(fnp_testing::FakeSocketProxy_RequestStream),
    DefaultNetworkWatcher(fnp_properties::DefaultNetworkWatcherRequestStream),
    DnsServerWatcher(fnp_socketproxy::DnsServerWatcherRequestStream),
    FuchsiaNetworks(fnp_socketproxy::FuchsiaNetworksRequestStream),
}

#[fuchsia::main]
async fn main() -> Result<(), anyhow::Error> {
    debug!("Starting fake-socket-proxy");

    let mut fs = ServiceFs::new_local();
    let (tx, rx) = mpsc::channel(100);
    let rx = Arc::new(Mutex::new(rx));
    let _ = fs
        .dir("svc")
        .add_fidl_service(IncomingServices::FakeSocketProxy)
        .add_fidl_service(IncomingServices::DefaultNetworkWatcher)
        .add_fidl_service(IncomingServices::DnsServerWatcher)
        .add_fidl_service(IncomingServices::FuchsiaNetworks);

    let _ = fs.take_and_serve_directory_handle()?;

    fs.for_each_concurrent(10, |request| {
        let tx = tx.clone();
        let rx = rx.clone();
        async {
            if let Err(e) = match request {
                IncomingServices::FakeSocketProxy(rs) => handle_provider(tx, rs).await,
                IncomingServices::DefaultNetworkWatcher(rs) => {
                    handle_default_network_watcher(rx, rs).await
                }
                IncomingServices::DnsServerWatcher(rs) => handle_dns_server_watcher(rs).await,
                IncomingServices::FuchsiaNetworks(rs) => handle_fuchsia_networks(rs).await,
            } {
                error!("{e:?}")
            }
        }
    })
    .await;

    Ok(())
}
