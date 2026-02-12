// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use dns_server_watcher::{DnsServers, DnsServersUpdateSource};
use fuchsia_component::server::ServiceFs;
use futures::stream::StreamExt as _;
use log::debug;
use {
    fidl_fuchsia_net_policy_properties as fnp_properties,
    fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy,
    fidl_fuchsia_net_policy_testing as fnp_testing,
};

enum IncomingServices {
    FakeNetcfg(fnp_testing::FakeNetcfgRequestStream),
    NetworkRegistry(fnp_socketproxy::NetworkRegistryRequestStream),
    Networks(fnp_properties::NetworksRequestStream),
}

impl std::fmt::Debug for IncomingServices {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::FakeNetcfg(_) => f.debug_tuple("FakeNetcfg").finish(),
            Self::NetworkRegistry(_) => f.debug_tuple("NetworkRegistry").finish(),
            Self::Networks(_) => f.debug_tuple("Networks").finish(),
        }
    }
}

#[derive(Debug)]
enum Event {
    FakeNetcfg(Result<fnp_testing::FakeNetcfgRequest, fidl::Error>),
    NetworkRegistryRequest(Result<fnp_socketproxy::NetworkRegistryRequest, fidl::Error>),
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
        .add_fidl_service(IncomingServices::NetworkRegistry)
        .add_fidl_service(IncomingServices::Networks);
    let _ = fs.take_and_serve_directory_handle().expect("must serve ServiceFs");
    let mut fs = fs.fuse();

    let mut fake_network_streams =
        futures::stream::SelectAll::<fnp_testing::FakeNetcfgRequestStream>::default();
    let mut network_registry_streams =
        futures::stream::SelectAll::<fnp_socketproxy::NetworkRegistryRequestStream>::default();
    let mut networks_streams = netcfg::network::ConnectionTagged::default();
    let mut dns_servers = DnsServers::default();

    let mut networks_service = netcfg::network::NetpolNetworksService::default();

    loop {
        let event = futures::select! {
            req_stream = fs.select_next_some() => {
                match req_stream {
                    IncomingServices::FakeNetcfg(rs) => fake_network_streams.push(rs),
                    IncomingServices::NetworkRegistry(rs) => network_registry_streams.push(rs),
                    IncomingServices::Networks(rs) => networks_streams.push(rs),
                }
                continue;
            }
            fake_netcfg_req = fake_network_streams.select_next_some() => {
                Event::FakeNetcfg(fake_netcfg_req)
            }
            network_registry_req = network_registry_streams.select_next_some() => {
                Event::NetworkRegistryRequest(network_registry_req)
            }
            net_attr_req = networks_streams.select_next_some() => {
                Event::NetworksAttributesRequest(net_attr_req)
            }
        };

        match event {
            Event::FakeNetcfg(req) => match req.expect("fake netcfg fidl error") {
                fnp_testing::FakeNetcfgRequest::SetDns { servers, responder } => {
                    dns_servers
                        .set_servers_from_source(DnsServersUpdateSource::SocketProxy, servers);
                    networks_service
                        .update(netcfg::network::PropertyUpdate::dns(&dns_servers))
                        .await;
                    responder.send().expect("Could not report response");
                }
            },
            Event::NetworkRegistryRequest(req) => networks_service
                .handle_delegated_networks_update(req)
                .await
                .expect("Could not update delegated networks"),
            Event::NetworksAttributesRequest((id, req)) => networks_service
                .handle_network_attributes_request(id, req)
                .await
                .expect("could not handle attribute request"),
        }
    }
}
