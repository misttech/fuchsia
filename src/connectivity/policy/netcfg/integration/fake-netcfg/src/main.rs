// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;

use fidl_fuchsia_net_policy_properties::PropertyUpdate;
use fuchsia_component::server::ServiceFs;
use futures::lock::Mutex;
use futures::stream::StreamExt as _;
use log::debug;
use {
    fidl_fuchsia_net_name as fnet_name, fidl_fuchsia_net_policy_properties as fnp_properties,
    fidl_fuchsia_net_policy_testing as fnp_testing,
};

async fn handle_fake_netcfg_request(
    req: Result<fnp_testing::FakeNetcfgRequest, fidl::Error>,
    networks: Arc<Mutex<netcfg::network::NetpolNetworksService>>,
) -> Result<(), anyhow::Error> {
    let req = req.expect("fake netcfg request");
    match req {
        fnp_testing::FakeNetcfgRequest::UpdateProperties {
            network_id,
            is_default,
            updates,

            responder,
        } => {
            let mut update = netcfg::network::PropertyUpdate::default();
            if is_default {
                update = update.default_network(network_id)?;
            }
            for upd in updates {
                match upd {
                    PropertyUpdate::SocketMarks(marks) => {
                        update = update.socket_marks(network_id, marks)?;
                    }
                    PropertyUpdate::DnsConfiguration(dns_configuration) => {
                        let mut dns_servers = dns_server_watcher::DnsServers::default();
                        dns_servers.set_servers_from_source(
                            dns_server_watcher::DnsServersUpdateSource::SocketProxy,
                            dns_configuration
                                .servers
                                .unwrap_or_default()
                                .into_iter()
                                .map(|mut d| {
                                    if d.source.is_none() {
                                        d.source = Some(fnet_name::DnsServerSource::SocketProxy(
                                            fnet_name::SocketProxyDnsServerSource {
                                                source_interface: Some(network_id),
                                                ..Default::default()
                                            },
                                        ));
                                    }
                                    d
                                })
                                .collect(),
                        );
                        update = update.dns(&dns_servers);
                    }
                    // New methods to this service must be handled.
                    PropertyUpdate::__SourceBreaking { .. } => {}
                }
            }

            log::info!("Updating NetpolNetworksService: {update:?}");
            networks.lock().await.update(update).await;
            responder.send()?;

            Ok(())
        }
    }
}

enum IncomingServices {
    FakeNetcfg(fnp_testing::FakeNetcfgRequestStream),
    Networks(fnp_properties::NetworksRequestStream),
}

impl std::fmt::Debug for IncomingServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FakeNetcfg(_) => f.debug_tuple("FakeNetcfg").finish(),
            Self::Networks(_) => f.debug_tuple("Networks").finish(),
        }
    }
}

#[derive(Debug)]
enum Event {
    FakeNetcfgRequest(Result<fnp_testing::FakeNetcfgRequest, fidl::Error>),
    NetworksAttributesRequest(
        (netcfg::network::ConnectionId, Result<fnp_properties::NetworksRequest, fidl::Error>),
    ),
}

#[fuchsia::main]
async fn main() {
    debug!("Starting fake-netcfg");

    let mut fs = ServiceFs::new_local();
    let _ = fs
        .dir("svc")
        .add_fidl_service(IncomingServices::FakeNetcfg)
        .add_fidl_service(IncomingServices::Networks);
    let _ = fs.take_and_serve_directory_handle().expect("must serve ServiceFs");
    let mut fs = fs.fuse();

    let mut fake_netcfg_streams =
        futures::stream::SelectAll::<fnp_testing::FakeNetcfgRequestStream>::default();
    let networks_service = Arc::new(Mutex::new(netcfg::network::NetpolNetworksService::default()));
    let mut networks_streams = netcfg::network::NetworksRequestStreams::default();

    loop {
        let event = futures::select! {
            req_stream = fs.next() => {
                match req_stream {
                    Some(IncomingServices::FakeNetcfg(rs)) => fake_netcfg_streams.push(rs),
                    Some(IncomingServices::Networks(rs)) => networks_streams.push(rs),
                    None => {}
                }
                continue;
            }
            fake_netcfg_req = fake_netcfg_streams.select_next_some() => {
                Event::FakeNetcfgRequest(fake_netcfg_req)
            }
            net_attr_req = networks_streams.select_next_some() => {
                Event::NetworksAttributesRequest(net_attr_req)
            }
        };

        match event {
            Event::FakeNetcfgRequest(req) => {
                handle_fake_netcfg_request(req, networks_service.clone())
                    .await
                    .expect("could not handle fake_netcfg request")
            }
            Event::NetworksAttributesRequest((id, req)) => networks_service
                .lock()
                .await
                .handle_network_attributes_request(id, req)
                .await
                .expect("could not handle attribute request"),
        }
    }
}
