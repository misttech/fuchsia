// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use assert_matches::assert_matches;
use fuchsia_async::DurationExt as _;
use futures::channel::mpsc;
use futures::future::join;
use futures::lock::Mutex;
use futures::{FutureExt as _, SinkExt as _, StreamExt as _};
use log::info;
use net_declare::fidl_ip_v6;
use netstack_testing_common::realms::{
    self, KnownServiceProvider, Manager, ManagerConfig, Netstack, NetstackExt, SocketProxyType,
    TestSandboxExt as _,
};
use netstack_testing_common::{ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, wait_for_component_stopped};
use netstack_testing_macros::netstack_test;
use policy_properties::{NetworkTokenExt, NetworksWatchDefaultResponseExt};
use policy_testing_common::{NetcfgOwnedDeviceArgs, with_netcfg_owned_device};
use pretty_assertions::assert_eq;
use std::collections::HashSet;
use std::pin::pin;
use std::sync::Arc;
use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_name as fnet_name,
    fidl_fuchsia_net_policy_properties as fnp_properties,
    fidl_fuchsia_net_policy_socketproxy as fnp_socketproxy,
    fidl_fuchsia_net_policy_testing as fnp_testing, fidl_fuchsia_posix_socket as fposix_socket,
};

trait TakeNetwork {
    fn take_network(self) -> Option<fnp_properties::NetworkToken>;
}

impl TakeNetwork for fnp_properties::NetworksWatchDefaultResponse {
    fn take_network(self) -> Option<fidl_fuchsia_net_policy_properties::NetworkToken> {
        match self {
            fnp_properties::NetworksWatchDefaultResponse::Network(network_token) => {
                Some(network_token)
            }
            fnp_properties::NetworksWatchDefaultResponse::NoDefaultNetwork(_) => None,
            _ => None,
        }
    }
}

fn network(network_id: u32, mark: Option<u32>) -> fnp_socketproxy::Network {
    fnp_socketproxy::Network {
        network_id: Some(network_id),
        info: Some(fnp_socketproxy::NetworkInfo::Starnix(fnp_socketproxy::StarnixNetworkInfo {
            mark: mark,
            handle: None,
            ..Default::default()
        })),
        ..Default::default()
    }
}

fn marks(mark_1: Option<u32>, mark_2: Option<u32>) -> fnet::Marks {
    fnet::Marks { mark_1, mark_2, ..Default::default() }
}

fn expect_sequence(
    actual: &[Option<Vec<fnp_properties::PropertyUpdate>>],
    expected: &[Option<fnp_properties::PropertyUpdate>],
) {
    let mut actual = actual.iter().peekable();
    let expected = expected.iter();

    for expect in expected {
        let next = actual.next();
        match next {
            None => panic!("Missing property. Next expected property is: {expect:?}"),
            Some(value) => match (value, expect) {
                (Some(v), Some(e)) => {
                    if !v.contains(e) {
                        panic!("Found out of sequence entry (expected: {e:?}, found: {v:?}");
                    }
                }
                (None, None) => {}
                _ => panic!("Found out of sequence entry (expected: {expect:?}, found: {value:?}"),
            },
        }

        loop {
            let peek = actual.peek();
            match peek {
                None => break,
                Some(value) => match (value, expect) {
                    (Some(v), Some(e)) => {
                        if !v.contains(e) {
                            break;
                        }
                    }
                    (None, None) => {}
                    _ => break,
                },
            }
            let _ = actual.next();
        }
    }
}

#[netstack_test]
#[variant(N, Netstack)]
#[variant(M, Manager)]
async fn test_track_socket_marks<N: Netstack, M: Manager>(name: &str) {
    use fnp_properties::PropertyUpdate;

    let _if_name = with_netcfg_owned_device::<M, N, _>(
        name,
        ManagerConfig::EnableSocketProxy,
        NetcfgOwnedDeviceArgs {
            use_out_of_stack_dhcp_client: N::USE_OUT_OF_STACK_DHCP_CLIENT,
            socket_proxy_type: SocketProxyType::Fake,
            ..Default::default()
        },
        |_if_id, _network, _interface_state, realm, _sandbox| {
            async move {
                let (mut tx, mut rx) = mpsc::channel::<()>(1);
                let (mut shutdown_tx, mut shutdown_rx) = mpsc::channel::<()>(1);

                let last_updates = Arc::new(Mutex::new(Vec::new()));
                let background = {
                    let networks = realm
                        .connect_to_protocol::<fnp_properties::NetworksMarker>()
                        .expect("couldn't connect to fuchsia.net.policy.properties/Networks");
                    let last_updates = last_updates.clone();
                    async move {
                        let mut network = networks
                            .watch_default()
                            .await
                            .expect("failed to fetch default network")
                            .take_network()
                            .expect("the first return from watch default should never fail");
                        let watch_default = |networks: &fnp_properties::NetworksProxy| {
                            networks.watch_default().fuse()
                        };
                        let watch_update =
                            |networks: &fnp_properties::NetworksProxy,
                             network: &fnp_properties::NetworkToken| {
                                networks
                                    .watch_properties(
                                        fnp_properties::NetworksWatchPropertiesRequest {
                                            network: Some(
                                                network.duplicate().expect("couldn't duplicate"),
                                            ),
                                            properties: Some(vec![
                                                fnp_properties::Property::SocketMarks,
                                                fnp_properties::Property::DnsConfiguration,
                                            ]),
                                            ..Default::default()
                                        },
                                    )
                                    .fuse()
                            };
                        let mut next_network = watch_default(&networks);
                        let mut watch_for_updates = watch_update(&networks, &network);
                        let mut last_marks = marks(None, None);
                        loop {
                            futures::select! {
                                new_network = next_network => {
                                    match new_network
                                            .expect("failed to fetch default network")
                                            .take_network() {
                                        Some(net) => {
                                            info!("Observed new network");
                                            network = net;
                                        },
                                        None => {
                                            info!(
                                                "Default network was lost via WatchDefault."
                                            );
                                            {
                                                let mut updates = last_updates.lock().await;
                                                if updates.last() != Some(&None) {
                                                    updates.push(None);
                                                    tx.send(()).await.expect("Can't send update");
                                                }
                                            }
                                        }
                                    }
                                    next_network = watch_default(&networks);
                                }
                                property_update = watch_for_updates => {
                                    match property_update {
                                        Ok(Ok(update)) => {
                                            for part in &update.clone() {
                                                if let PropertyUpdate::SocketMarks(marks) = part {
                                                    if *marks != last_marks {
                                                        info!(
                                                            "Updating last_updates: {:?}", update
                                                        );
                                                        last_marks = marks.clone();
                                                        last_updates
                                                            .lock()
                                                            .await
                                                            .push(Some(update.clone()));
                                                        tx
                                                            .send(())
                                                            .await
                                                            .expect("Can't send update");
                                                    }
                                                }
                                            }
                                        }
                                        Ok(Err(fnp_properties::WatchError::NetworkGone)) => {
                                            info!("Default network was lost via WatchUpdate");
                                            {
                                                let mut updates = last_updates.lock().await;
                                                if updates.last() != Some(&None) {
                                                    updates.push(None);
                                                    tx.send(()).await.expect("Can't send update");
                                                }
                                            }
                                        }
                                        _ => {}
                                    }
                                    watch_for_updates = watch_update(&networks, &network);
                                }
                                _ = shutdown_rx.next() => {
                                    return;
                                }
                            }
                        }
                    }
                };

                let test = async move {
                    let socket_proxy = realm
                        .connect_to_protocol_from_child::<fnp_socketproxy::NetworkRegistryMarker>(
                            realms::constants::fake_socket_proxy::COMPONENT_NAME,
                        )
                        .expect("failed to connect to FakeSocketProxy");

                    socket_proxy
                        .add(&network(1, Some(1)))
                        .await
                        .expect("fidl error")
                        .expect("protocol error");
                    socket_proxy
                        .set_default(&fposix_socket::OptionalUint32::Value(1))
                        .await
                        .expect("fidl error")
                        .expect("protocol error");
                    let _ = rx.next().await;

                    socket_proxy
                        .update(&network(1, Some(2)))
                        .await
                        .expect("fidl error")
                        .expect("protocol error");
                    let _ = rx.next().await;

                    socket_proxy
                        .update(&network(1, Some(4)))
                        .await
                        .expect("fidl error")
                        .expect("protocol error");
                    let _ = rx.next().await;

                    socket_proxy
                        .set_default(&fposix_socket::OptionalUint32::Unset(fposix_socket::Empty))
                        .await
                        .expect("fidl error")
                        .expect("protocol error");
                    let _ = rx.next().await;

                    socket_proxy
                        .update(&network(1, Some(8)))
                        .await
                        .expect("fidl error")
                        .expect("protocol error");
                    socket_proxy
                        .set_default(&fposix_socket::OptionalUint32::Value(1))
                        .await
                        .expect("fidl error")
                        .expect("protocol error");
                    let _ = rx.next().await;

                    let updates = last_updates.lock().await.clone();
                    expect_sequence(
                        &updates,
                        &vec![
                            Some(PropertyUpdate::SocketMarks(marks(Some(1), None))),
                            Some(PropertyUpdate::SocketMarks(marks(Some(2), None))),
                            Some(PropertyUpdate::SocketMarks(marks(Some(4), None))),
                            // None update represents the empty default_network.update call
                            None,
                            Some(PropertyUpdate::SocketMarks(marks(Some(8), None))),
                        ],
                    );

                    shutdown_tx.send(()).await.expect("couldn't trigger clean shutdown");
                };

                // N.B. Waiting for both futures to complete ensures that both
                // the test and the background task clean themselves up, closing
                // all open FIDL channels before shutting down the realm.
                futures::future::join(background, test).await;
            }
            .boxed_local()
        },
    )
    .await;
}

trait PropertyUpdateExt {
    fn dns_configuration(&self) -> Option<&fnp_properties::DnsConfiguration>;
}

impl PropertyUpdateExt for fnp_properties::PropertyUpdate {
    fn dns_configuration(&self) -> Option<&fidl_fuchsia_net_policy_properties::DnsConfiguration> {
        match self {
            fidl_fuchsia_net_policy_properties::PropertyUpdate::DnsConfiguration(
                dns_configuration,
            ) => Some(dns_configuration),
            fidl_fuchsia_net_policy_properties::PropertyUpdate::SocketMarks(_) | _ => None,
        }
    }
}

#[netstack_test]
#[variant(N, Netstack)]
#[variant(M, Manager)]
async fn test_track_dns_changes<N: Netstack, M: Manager>(name: &str) -> Result<(), anyhow::Error> {
    const NDP_DNS_SERVER1: fnet::Ipv6Address = fidl_ip_v6!("20a::1234:5678");
    const NDP_DNS_SERVER2: fnet::Ipv6Address = fidl_ip_v6!("20a::2345:6789");
    const NDP_DNS_SERVER3: fnet::Ipv6Address = fidl_ip_v6!("20a::3456:7890");

    const DEFAULT_DNS_PORT: u16 = 53;
    const TEST_NETWORK_ID: u32 = 2;

    let _if_name = with_netcfg_owned_device::<M, N, _>(
        name,
        ManagerConfig::EnableSocketProxy,
        NetcfgOwnedDeviceArgs {
            use_out_of_stack_dhcp_client: N::USE_OUT_OF_STACK_DHCP_CLIENT,
            socket_proxy_type: SocketProxyType::Fake,
            ..Default::default()
        },
        |_if_id, _test_network, _interface_state, realm, _sandbox| {
            async move {
                async fn update_dns(
                    fake_socket_proxy: &fnp_testing::FakeSocketProxy_Proxy,
                    addresses: &[fnet::Ipv6Address],
                ) {
                    fake_socket_proxy
                        .set_dns(&[fnp_socketproxy::DnsServerList {
                            source_network_id: Some(TEST_NETWORK_ID),
                            addresses: Some(
                                addresses
                                    .iter()
                                    .map(|address| {
                                        fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                                            address: address.clone(),
                                            port: DEFAULT_DNS_PORT,
                                            zone_index: 0,
                                        })
                                    })
                                    .collect(),
                            ),
                            ..Default::default()
                        }])
                        .await
                        .expect("fidl error");
                }

                let wait_for_netmgr = wait_for_component_stopped(
                    &realm,
                    realms::constants::netcfg::COMPONENT_NAME,
                    None,
                )
                .fuse();
                let mut wait_for_netmgr = pin!(wait_for_netmgr);
                let socket_proxy = realm
                    .connect_to_protocol_from_child::<fnp_socketproxy::NetworkRegistryMarker>(
                        realms::constants::fake_socket_proxy::COMPONENT_NAME,
                    )
                    .expect("failed to connect to FakeSocketProxy");
                socket_proxy
                    .add(&network(TEST_NETWORK_ID, None))
                    .await
                    .expect("fidl error")
                    .expect("protocol error");
                socket_proxy
                    .set_default(&fposix_socket::OptionalUint32::Value(2))
                    .await
                    .expect("fidl error")
                    .expect("protocol error");
                let networks = realm
                    .connect_to_protocol_from_child::<fnp_properties::NetworksMarker>(
                        realms::constants::netcfg::COMPONENT_NAME,
                    )
                    .expect("failed to connect to Networks");
                let network = networks
                    .watch_default()
                    .await
                    .expect("failed to fetch default network")
                    .take_network()
                    .expect("the first return from watch default should never fail");
                let watch_update =
                    |networks: &fnp_properties::NetworksProxy,
                     network: &fnp_properties::NetworkToken| {
                        networks
                            .watch_properties(fnp_properties::NetworksWatchPropertiesRequest {
                                network: Some(network.duplicate().expect("couldn't duplicate")),
                                properties: Some(vec![fnp_properties::Property::DnsConfiguration]),
                                ..Default::default()
                            })
                            .fuse()
                    };
                let mut watch = watch_update(&networks, &network);
                let mut dns_sequence = std::collections::VecDeque::from([
                    vec![NDP_DNS_SERVER1],
                    vec![NDP_DNS_SERVER1, NDP_DNS_SERVER2],
                    vec![NDP_DNS_SERVER1, NDP_DNS_SERVER2, NDP_DNS_SERVER3],
                ]);
                let mut seen_updates = Vec::new();

                let fake_socket_proxy = realm
                    .connect_to_protocol_from_child::<fnp_testing::FakeSocketProxy_Marker>(
                        realms::constants::fake_socket_proxy::COMPONENT_NAME,
                    )
                    .expect("Failed to connect to FakeSocketProxy");

                'main: loop {
                    let () = futures::select! {
                        update = watch => {
                            watch = watch_update(&networks, &network);
                            let update = update.expect("fidl error").expect("protocol error");
                            let server_count = update[0]
                                .dns_configuration()
                                .unwrap()
                                .servers
                                .as_ref()
                                .map(|s|s.len())
                                .unwrap_or(0);
                            seen_updates.push(update);
                            if let Some(list) = dns_sequence.pop_front() {
                                // Each update is 1 more server than the
                                // last. Wait until we see the previous
                                // update.
                                if list.len() - 1 == server_count {
                                    update_dns(&fake_socket_proxy, &list).await;
                                } else {
                                    dns_sequence.push_front(list);
                                }
                            }
                            // The final update should have 3 DNS servers.
                            if server_count >= 3 {
                                break 'main;
                            }
                        },
                        () = fuchsia_async::Timer::new(
                            ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.after_now()
                        ).fuse() => {
                            panic!("timed out waiting for DNS server list");
                        },
                        stopped_event = wait_for_netmgr => {
                            panic!("the network manager: {stopped_event:?}");
                        }
                    };
                }

                let dns_servers = seen_updates
                    .into_iter()
                    .map(|upd| {
                        upd[0]
                            .dns_configuration()
                            .unwrap()
                            .servers
                            .as_ref()
                            .unwrap()
                            .into_iter()
                            .map(|server| server.address)
                            .collect::<HashSet<_>>()
                    })
                    .collect::<Vec<_>>();

                // We expect there to be 4 total DNS server updates:
                assert_eq!(dns_servers.len(), 4);

                // 1st update: Initial empty list.
                assert_eq!(dns_servers[0], HashSet::new());

                // 2nd update: Just NDP_DNS_SERVER1.
                assert_eq!(
                    dns_servers[1],
                    vec![Some(fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                        address: NDP_DNS_SERVER1,
                        port: DEFAULT_DNS_PORT,
                        zone_index: 0
                    })),]
                    .into_iter()
                    .collect::<HashSet<_>>()
                );

                // 3rd update: Just NDP_DNS_SERVER1 and NDP_DNS_SERVER2.
                assert_eq!(
                    dns_servers[2],
                    vec![
                        Some(fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                            address: NDP_DNS_SERVER1,
                            port: DEFAULT_DNS_PORT,
                            zone_index: 0
                        })),
                        Some(fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                            address: NDP_DNS_SERVER2,
                            port: DEFAULT_DNS_PORT,
                            zone_index: 0
                        })),
                    ]
                    .into_iter()
                    .collect::<HashSet<_>>()
                );

                // 4th update: Just NDP_DNS_SERVER1, NDP_DNS_SERVER2, and NDP_DNS_SERVER3.
                assert_eq!(
                    dns_servers[3],
                    vec![
                        Some(fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                            address: NDP_DNS_SERVER1,
                            port: DEFAULT_DNS_PORT,
                            zone_index: 0
                        })),
                        Some(fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                            address: NDP_DNS_SERVER2,
                            port: DEFAULT_DNS_PORT,
                            zone_index: 0
                        })),
                        Some(fnet::SocketAddress::Ipv6(fnet::Ipv6SocketAddress {
                            address: NDP_DNS_SERVER3,
                            port: DEFAULT_DNS_PORT,
                            zone_index: 0
                        })),
                    ]
                    .into_iter()
                    .collect::<HashSet<_>>()
                );
            }
            .boxed_local()
        },
    )
    .await;

    Ok(())
}

#[netstack_test]
#[variant(N, Netstack)]
#[variant(M, Manager)]
async fn test_network_token_correlation<N: Netstack, M: Manager>(
    name: &str,
) -> Result<(), anyhow::Error> {
    let _if_name = with_netcfg_owned_device::<M, N, _>(
        name,
        ManagerConfig::EnableSocketProxy,
        NetcfgOwnedDeviceArgs {
            use_out_of_stack_dhcp_client: N::USE_OUT_OF_STACK_DHCP_CLIENT,
            socket_proxy_type: SocketProxyType::Fake,
            ..Default::default()
        },
        |if_id, _network, _interface_state, realm, _sandbox| {
            async move {
                let socket_proxy = realm
                    .connect_to_protocol_from_child::<fnp_socketproxy::NetworkRegistryMarker>(
                        realms::constants::fake_socket_proxy::COMPONENT_NAME,
                    )
                    .expect("failed to connect to FakeSocketProxy");
                socket_proxy
                    .add(&network(if_id.try_into().unwrap(), None))
                    .await
                    .expect("fidl error")
                    .expect("protocol error");
                socket_proxy
                    .set_default(&fposix_socket::OptionalUint32::Value(if_id.try_into().unwrap()))
                    .await
                    .expect("fidl error")
                    .expect("protocol error");

                let networks = realm
                    .connect_to_protocol_from_child::<fnp_properties::NetworksMarker>(
                        realms::constants::netcfg::COMPONENT_NAME,
                    )
                    .expect("fidl error");
                let networks2 = realm
                    .connect_to_protocol_from_child::<fnp_properties::NetworksMarker>(
                        realms::constants::netcfg::COMPONENT_NAME,
                    )
                    .expect("fidl error");

                let (net1, net2) =
                    futures::future::join(networks.watch_default(), networks2.watch_default())
                        .await;
                let (net1, net2) = (
                    net1.ok().and_then(NetworksWatchDefaultResponseExt::into_network).unwrap(),
                    net2.ok().and_then(NetworksWatchDefaultResponseExt::into_network).unwrap(),
                );

                assert!(net1.koid().is_ok());
                assert_eq!(net1.koid(), net2.koid());

                let resolver = realm
                    .connect_to_protocol_from_child::<fnp_properties::NetworkTokenResolverMarker>(
                        realms::constants::netcfg::COMPONENT_NAME,
                    )
                    .expect("fidl error");
                let resolver2 = realm
                    .connect_to_protocol_from_child::<fnp_properties::NetworkTokenResolverMarker>(
                        realms::constants::netcfg::COMPONENT_NAME,
                    )
                    .expect("fidl error");
                let (resolved1, resolved2) = futures::future::join(
                    resolver.resolve_token(net1.duplicate().expect("can't duplicate")),
                    resolver2.resolve_token(net1.duplicate().expect("can't duplicate")),
                )
                .await;
                let (resolved1, resolved2) = (
                    resolved1.expect("fidl error").expect("bad token"),
                    resolved2.expect("fidl error").expect("bad token"),
                );

                assert_ne!(resolved1.koid(), net1.koid());
                assert!(resolved1.koid().is_ok());
                assert_eq!(resolved1.koid(), resolved2.koid());

                // Unset the default.
                socket_proxy
                    .set_default(&fposix_socket::OptionalUint32::Unset(fposix_socket::Empty))
                    .await
                    .expect("fidl error")
                    .expect("thingy");

                // Resolving using a default network token for a network that is
                // no longer the default should fail, since the default network
                // token is no longer valid.
                let no_default_resolved = resolver
                    .resolve_token(net1.duplicate().expect("can't duplicate"))
                    .await
                    .expect("fidl error");
                assert_matches!(
                    no_default_resolved,
                    Err(fnp_properties::NetworkTokenResolverResolveTokenError::InvalidNetworkToken)
                );
            }
            .boxed_local()
        },
    )
    .await;

    Ok(())
}

#[netstack_test]
#[variant(N, Netstack)]
async fn test_fake_netcfg<N: Netstack>(name: &str) -> Result<(), anyhow::Error> {
    const TEST_INTERFACE: u64 = 1;

    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox
        .create_netstack_realm_with::<N, _, _>(name, [KnownServiceProvider::FakeNetcfg])
        .expect("create netstack realm");

    let fake_netcfg = realm
        .connect_to_protocol::<fnp_testing::FakeNetcfgMarker>()
        .expect("could not connect to FakeNetcfg");
    let networks = realm
        .connect_to_protocol::<fnp_properties::NetworksMarker>()
        .expect("could not connect to Networks");
    let network_registry = realm
        .connect_to_protocol::<fnp_socketproxy::NetworkRegistryMarker>()
        .expect("could not connect to FakeNetcfg");

    let expected_servers = vec![fnet_name::DnsServer_ {
        address: Some(net_declare::fidl_socket_addr!("[20a::1234:5678]:53")),
        source: Some(fnet_name::DnsServerSource::SocketProxy(
            fnet_name::SocketProxyDnsServerSource {
                source_interface: Some(TEST_INTERFACE),
                ..Default::default()
            },
        )),
        ..Default::default()
    }];
    // TODO(https://fxbug.dev/477980011): This is unnecessary once
    // DNS servers are derived from NetworkRegistry updates.
    fake_netcfg.set_dns(&expected_servers).await.expect("Failed to set expected dns servers");

    let expected =
        vec![fnp_properties::PropertyUpdate::DnsConfiguration(fnp_properties::DnsConfiguration {
            servers: Some(expected_servers),
            ..Default::default()
        })];

    let update_netcfg_fut = async move {
        network_registry
            .add(&fnp_socketproxy::Network {
                network_id: Some(TEST_INTERFACE as u32),
                info: Some(fnp_socketproxy::NetworkInfo::Starnix(
                    fnp_socketproxy::StarnixNetworkInfo { mark: Some(1), ..Default::default() },
                )),
                dns_servers: Some(fnp_socketproxy::NetworkDnsServers {
                    v6: Some(vec![net_declare::fidl_ip_v6!("20a::1234:5678")]),
                    v4: Some(vec![]),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await?
            .expect("add failed");

        let res = network_registry.set_default(&fposix_socket::OptionalUint32::Value(1)).await?;

        Ok::<_, anyhow::Error>(res)
    };
    let properties_watch = async move {
        let network = networks.watch_default().await?.take_network().expect("no network returned");

        let update = networks
            .watch_properties(fnp_properties::NetworksWatchPropertiesRequest {
                network: Some(network.duplicate().unwrap()),
                properties: Some(vec![fnp_properties::Property::DnsConfiguration]),
                ..Default::default()
            })
            .await?
            .map_err(|e| anyhow::anyhow!("Protocol error {e:?}"))?;

        Ok::<_, anyhow::Error>(update)
    };

    let (res1, res2) = join(update_netcfg_fut, properties_watch).await;

    res1.expect("add network fidl error").expect("add network protocol error");
    let update = res2.expect("error while watching properties");
    assert_eq!(update, expected);

    Ok(())
}
