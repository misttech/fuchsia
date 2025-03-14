// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use assert_matches::assert_matches;
use derivative::Derivative;
use fidl::endpoints::Proxy as _;
use fuchsia_async::{self as fasync, DurationExt as _, TimeoutExt as _};

use futures::{FutureExt as _, StreamExt as _, TryFutureExt as _, TryStreamExt as _};
use maplit::hashmap;
use net_declare::{fidl_ip, fidl_subnet};
use net_types::ip::{GenericOverIp, Ip, IpVersion, IpVersionMarker, Ipv4Addr, Ipv6Addr};
use netemul::RealmUdpSocket as _;
use netstack_testing_common::realms::{Netstack, TestSandboxExt as _};
use netstack_testing_common::{interfaces, ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT};
use netstack_testing_macros::netstack_test;
use std::collections::HashMap;
use test_case::test_case;
use test_util::assert_gt;
use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin,
    fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext,
    fidl_fuchsia_net_multicast_admin as fnet_multicast_admin,
};

use fidl_fuchsia_net_multicast_ext::{
    self as fnet_multicast_ext, AddRouteError, DelRouteError, FidlMulticastAdminIpExt,
    GetRouteStatsError, TableControllerProxy as _, UnicastSourceAndMulticastDestination,
    WatchRoutingEventsResponse,
};

#[derive(Clone, Copy)]
enum IpAddrType {
    Any,
    LinkLocalMulticast,
    LinkLocalUnicast,
    Multicast,
    OtherMulticast,
    Unicast,
}

impl IpAddrType {
    /// Returns the IP address associated with the variant.
    ///
    /// The IP version of the returned address matches the specified `version`.
    fn address(&self, version: IpVersion) -> fnet::IpAddress {
        match version {
            IpVersion::V4 => match *self {
                IpAddrType::Any => fidl_ip!("0.0.0.0"),
                IpAddrType::LinkLocalMulticast => fidl_ip!("224.0.0.1"),
                IpAddrType::LinkLocalUnicast => fidl_ip!("169.254.0.10"),
                IpAddrType::Multicast => fidl_ip!("225.0.0.0"),
                IpAddrType::OtherMulticast => fidl_ip!("225.0.0.1"),
                IpAddrType::Unicast => fidl_ip!("192.168.0.2"),
            },
            IpVersion::V6 => match *self {
                IpAddrType::Any => fidl_ip!("::"),
                IpAddrType::LinkLocalMulticast => fidl_ip!("ff02::a"),
                IpAddrType::LinkLocalUnicast => fidl_ip!("fe80::a"),
                IpAddrType::Multicast => fidl_ip!("ff0e::a"),
                IpAddrType::OtherMulticast => fidl_ip!("ff0e::b"),
                IpAddrType::Unicast => fidl_ip!("200b::1"),
            },
        }
    }
}

/// Identifier for a device that listens for multicast packets.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Client {
    A,
    B,
}

impl Client {
    fn name(&self) -> String {
        format!("client-{:?}", self)
    }

    fn config(&self, version: IpVersion) -> RouterConnectedDeviceConfig {
        match version {
            IpVersion::V4 => match *self {
                Client::A => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("192.168.1.2/24"),
                    router_ep_addr: fidl_subnet!("192.168.1.1/24"),
                },
                Client::B => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("192.168.2.2/24"),
                    router_ep_addr: fidl_subnet!("192.168.2.1/24"),
                },
            },
            IpVersion::V6 => match *self {
                Client::A => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("a::1/24"),
                    router_ep_addr: fidl_subnet!("b::1/24"),
                },
                Client::B => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("a::2/24"),
                    router_ep_addr: fidl_subnet!("b::2/24"),
                },
            },
        }
    }
}

/// Identifier for a device that sends multicast packets.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum Server {
    A,
    B,
}

fn create_router_realm<'a, N: Netstack>(
    name: &'a str,
    sandbox: &'a netemul::TestSandbox,
) -> netemul::TestRealm<'a> {
    sandbox.create_netstack_realm::<N, _>(format!("{}_router_realm", name)).expect("create realm")
}

impl Server {
    fn name(&self) -> String {
        format!("server-{:?}", self)
    }

    fn config(&self, version: IpVersion) -> RouterConnectedDeviceConfig {
        match version {
            IpVersion::V4 => match *self {
                Server::A => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("192.168.0.2/24"),
                    router_ep_addr: fidl_subnet!("192.168.0.1/24"),
                },
                Server::B => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("192.168.3.1/24"),
                    router_ep_addr: fidl_subnet!("192.168.3.2/24"),
                },
            },
            IpVersion::V6 => match *self {
                Server::A => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("a::3/24"),
                    router_ep_addr: fidl_subnet!("b::3/24"),
                },
                Server::B => RouterConnectedDeviceConfig {
                    ep_addr: fidl_subnet!("a::4/24"),
                    router_ep_addr: fidl_subnet!("b::4/24"),
                },
            },
        }
    }
}

/// A network that is configured to send, forward, and receive multicast
/// packets.
///
/// Generates a network with the following structure:
///
/// Server::A ─┐        ┌─ Client::A
///            │        │
///            ├─router─┼
///            │        │
/// Server::B ─┘        └─ Client::B
///
/// The device types and their responsibilities are as follows:
///
///  - A server sends multicast packets to the router.
///  - The router optionally forwards and/or delivers multicast packets locally.
///    Additionally, if configured, the router sends multicast packets.
///  - A client listens for forwarded multicast packets.
struct MulticastForwardingNetwork<'a, I: Ip> {
    router_listener_socket: Option<fasync::net::UdpSocket>,
    router_realm: &'a netemul::TestRealm<'a>,
    clients: std::collections::HashMap<Client, ClientDevice<'a>>,
    servers: std::collections::HashMap<Server, RouterConnectedDevice<'a>>,
    options: MulticastForwardingNetworkOptions,
    _marker: IpVersionMarker<I>,
}

impl<'a, I: Ip + FidlMulticastAdminIpExt> MulticastForwardingNetwork<'a, I> {
    const PAYLOAD: &'static str = "Hello multicast";

    async fn new<N: Netstack>(
        name: &'a str,
        sandbox: &'a netemul::TestSandbox,
        router_realm: &'a netemul::TestRealm<'a>,
        servers: Vec<Server>,
        clients: Vec<Client>,
        options: MulticastForwardingNetworkOptions,
    ) -> MulticastForwardingNetwork<'a, I> {
        let MulticastForwardingNetworkOptions {
            source_device,
            enable_multicast_forwarding,
            listen_from_router,
            packet_ttl: _,
        } = options;
        let multicast_socket_addr = create_socket_addr(IpAddrType::Multicast.address(I::VERSION));
        let servers: HashMap<_, _> = futures::stream::iter(servers)
            .then(|server| async move {
                let device = create_router_connected_device::<N>(
                    format!("{}_{}", name, server.name()),
                    sandbox,
                    router_realm,
                    server.config(I::VERSION),
                )
                .await;
                (server, device)
            })
            .collect()
            .await;
        let clients: HashMap<_, _> = futures::stream::iter(clients)
            .then(|client| async move {
                let device = create_router_connected_device::<N>(
                    format!("{}_{}", name, client.name()),
                    sandbox,
                    router_realm,
                    client.config(I::VERSION),
                )
                .await;
                let socket = create_listener_socket(
                    &device.realm,
                    I::VERSION,
                    device.interface.id(),
                    device.ep_addr,
                    multicast_socket_addr,
                )
                .await;
                (client, ClientDevice { device, socket })
            })
            .collect()
            .await;

        for server in servers.keys() {
            let device = servers.get(&server).expect("input server not found");
            set_multicast_forwarding(
                device.router_interface.control(),
                enable_multicast_forwarding,
            )
            .await;
        }

        let router_listener_socket = if listen_from_router {
            let input_server = match source_device {
                SourceDevice::Router(server) | SourceDevice::Server(server) => server,
            };
            let input_server_device = servers.get(&input_server).expect("input server not found");

            Some(
                create_listener_socket(
                    router_realm,
                    I::VERSION,
                    input_server_device.router_interface.id(),
                    input_server_device.router_ep_addr,
                    multicast_socket_addr,
                )
                .await,
            )
        } else {
            None
        };
        MulticastForwardingNetwork {
            router_listener_socket,
            router_realm,
            clients,
            servers,
            options,
            _marker: IpVersionMarker::default(),
        }
    }

    /// Sends a single multicast packet with optional behavior overrides.
    ///
    /// If `dst_addr_type` is specified, it will be used rather than
    /// `IpAddrType::Multicast`.
    async fn send_multicast_packet_with_overrides(&self, dst_addr_type: Option<IpAddrType>) {
        let (realm, addr, id) = match self.options.source_device {
            SourceDevice::Router(server) => {
                let server_device = self.get_server(server);
                (
                    self.router_realm,
                    server_device.router_ep_addr,
                    server_device.router_interface.id(),
                )
            }
            SourceDevice::Server(server) => {
                let server_device = self.get_server(server);
                (&server_device.realm, server_device.ep_addr, server_device.interface.id())
            }
        };

        let dst_addr_type = dst_addr_type.unwrap_or(IpAddrType::Multicast);
        let dst_addr = create_socket_addr(dst_addr_type.address(I::VERSION));
        let server_sock = fasync::net::UdpSocket::bind_in_realm(realm, dst_addr)
            .await
            .expect("bind_in_realm failed for server socket");

        match I::VERSION {
            IpVersion::V4 => {
                let interface_addr = get_ipv4_address_from_subnet(addr);
                server_sock
                    .as_ref()
                    .set_multicast_if_v4(&interface_addr.addr.into())
                    .expect("set_multicast_if_v4 failed");
                server_sock
                    .as_ref()
                    .set_multicast_ttl_v4(self.options.packet_ttl.into())
                    .expect("set_multicast_ttl_v4 failed");
            }
            IpVersion::V6 => {
                server_sock
                    .as_ref()
                    .set_multicast_if_v6(u32::try_from(id).unwrap_or_else(|e| {
                        panic!("failed to convert {} to u32 with error: {:?}", id, e)
                    }))
                    .expect("set_multicast_if_v6 failed");
                server_sock
                    .as_ref()
                    .set_multicast_hops_v6(self.options.packet_ttl.into())
                    .expect("set_multicast_hops_v6 failed");
            }
        }
        let r =
            server_sock.send_to(Self::PAYLOAD.as_bytes(), dst_addr).await.expect("send_to failed");
        assert_eq!(r, Self::PAYLOAD.as_bytes().len());
    }

    /// Like [`send_multicast_packet_with_overrides`] but without any overrides.
    async fn send_multicast_packet(&self) {
        self.send_multicast_packet_with_overrides(None).await
    }

    /// Verifies the receipt of a multicast packet against the provided
    /// `expectations`.
    ///
    /// The `expectations` should contain an entry for each `Client` that should
    /// be verified. If the value of the entry is true, then the `Client` must
    /// receive a multicast packet. If the value is false, then the `Client`
    /// must not receive a multicast packet.
    async fn receive_multicast_packet(
        &self,
        expectations: std::collections::HashMap<Client, bool>,
    ) {
        futures::stream::iter(
            expectations
                .into_iter()
                .map(|(client, expect_forwarded_packet)| {
                    (client.name(), &self.get_client(client).socket, expect_forwarded_packet)
                })
                .chain(
                    self.router_listener_socket
                        .as_ref()
                        .and_then(|socket| Some(("router".to_string(), socket, true))),
                ),
        )
        .for_each_concurrent(None, |(name, socket, expect_packet)| async move {
            let mut buf = [0u8; 1024];
            let timeout = if expect_packet {
                netstack_testing_common::ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT
            } else {
                netstack_testing_common::ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT
            };
            match (
                socket
                    .recv_from(&mut buf[..])
                    .map_ok(Some)
                    .on_timeout(timeout.after_now(), || Ok(None))
                    .await
                    .expect("recv_from failed"),
                expect_packet,
            ) {
                (Some((r, from)), true) => {
                    assert_eq!(from, create_socket_addr(self.get_source_address()));
                    assert_eq!(r, Self::PAYLOAD.as_bytes().len());
                    assert_eq!(&buf[..r], Self::PAYLOAD.as_bytes());
                }
                (Some((_r, from)), false) => {
                    panic!("{} unexpectedly received packet from {:?}", name, from)
                }
                (None, true) => panic!("{} failed to receive packet", name),
                (None, false) => {}
            }
        })
        .await;
    }

    /// Creates a `RoutingTableController`.
    fn create_multicast_controller(&self) -> I::TableControllerProxy {
        self.router_realm
            .connect_to_protocol::<I::TableControllerMarker>()
            .expect("failed to create multicast table controller")
    }

    async fn add_default_route(&self, controller: &I::TableControllerProxy) {
        controller
            .add_route(
                self.default_unicast_source_and_multicast_destination(),
                self.default_multicast_route(),
            )
            .await
            .expect("send request")
            .expect("add_route error");
    }

    /// Waits for the multicast routing table to be cleared.
    ///
    /// The routing table is cleared when the corresponding controller is
    /// dropped. Since this cleanup process is not awaitable, callers may invoke
    /// this function to ensure that a packet eventually becomes unrouteable.
    async fn wait_for_packet_to_become_unrouteable(&self) {
        const MAX_ATTEMPTS: usize = 20;
        const WAIT_BEFORE_RETRY_DURATION: std::time::Duration = std::time::Duration::from_secs(3);
        match fuchsia_backoff::retry_or_last_error(
            std::iter::repeat(WAIT_BEFORE_RETRY_DURATION).take(MAX_ATTEMPTS),
            || async {
                self.send_multicast_packet().await;
                futures::stream::iter(self.clients.values())
                    .map(Ok)
                    .try_for_each(|device| async move {
                        let mut buf = [0u8; 1024];
                        let socket = &device.socket;
                        socket
                            .recv_from(&mut buf[..])
                            .map_ok(Err)
                            .on_timeout(
                                netstack_testing_common::ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT
                                    .after_now(),
                                || Ok(Ok(())),
                            )
                            .await
                            .expect("recv_from failed")
                    })
                    .await
            },
        )
        .await
        {
            Ok(()) => {}
            Err((_r, from)) => panic!("unexpectedly received packet from {:?}", from),
        }
    }

    /// Sends a multicast packet and verifies the receipt of the packet against
    /// the provided `expectations`.
    ///
    /// See `receive_multicast_packet` for details regarding the expected format
    /// of the `expectations`.
    async fn send_and_receive_multicast_packet(
        &self,
        expectations: std::collections::HashMap<Client, bool>,
    ) {
        let send_fut = self.send_multicast_packet();
        let receive_fut = self.receive_multicast_packet(expectations);

        let ((), ()) = futures::future::join(send_fut, receive_fut).await;
    }

    fn get_server(&self, server: Server) -> &RouterConnectedDevice<'_> {
        self.servers.get(&server).unwrap_or_else(|| panic!("server {:?} not found", server))
    }

    fn get_client(&self, client: Client) -> &ClientDevice<'_> {
        self.clients.get(&client).unwrap_or_else(|| panic!("client {:?} not found", client))
    }

    /// Returns the address of the interface that sends multicast packets.
    fn get_source_address(&self) -> fnet::IpAddress {
        let subnet = match self.options.source_device {
            SourceDevice::Router(server) => self.get_server(server).router_ep_addr,
            SourceDevice::Server(server) => self.get_server(server).ep_addr,
        };
        subnet.addr
    }

    /// Returns the interface ID of the router interface handles incoming
    /// multicast packets.
    fn get_source_router_interface_id(&self) -> u64 {
        match self.options.source_device {
            SourceDevice::Router(server) | SourceDevice::Server(server) => {
                self.get_server(server).router_interface.id()
            }
        }
    }

    fn get_device_address(&self, address: DeviceAddress) -> fnet::IpAddress {
        match address {
            DeviceAddress::Router(server) => self.get_server(server).router_ep_addr.addr,
            DeviceAddress::Server(server) => self.get_server(server).ep_addr.addr,
            DeviceAddress::Other(addr) => addr.address(I::VERSION),
        }
    }

    fn default_unicast_source_and_multicast_destination(
        &self,
    ) -> UnicastSourceAndMulticastDestination<I> {
        let source_addr = self.get_source_address();
        let destination_addr = IpAddrType::Multicast.address(I::VERSION);
        self.create_unicast_source_and_multicast_destination(source_addr, destination_addr)
    }

    fn default_multicast_route(&self) -> fnet_multicast_ext::Route {
        fnet_multicast_ext::Route {
            expected_input_interface: self.get_source_router_interface_id(),
            action: fnet_multicast_admin::Action::OutgoingInterfaces(vec![
                fnet_multicast_admin::OutgoingInterfaces {
                    id: self.get_client(Client::A).device.router_interface.id(),
                    min_ttl: self.options.packet_ttl - 1,
                },
            ]),
        }
    }

    fn create_unicast_source_and_multicast_destination(
        &self,
        source: fnet::IpAddress,
        destination: fnet::IpAddress,
    ) -> UnicastSourceAndMulticastDestination<I> {
        I::map_ip_out(
            (source, destination),
            |(source, destination)| UnicastSourceAndMulticastDestination {
                unicast_source: Ipv4Addr::new(get_ipv4_address_from_addr(source).addr),
                multicast_destination: Ipv4Addr::new(get_ipv4_address_from_addr(destination).addr),
            },
            |(source, destination)| UnicastSourceAndMulticastDestination {
                unicast_source: Ipv6Addr::from_bytes(get_ipv6_address_from_addr(source).addr),
                multicast_destination: Ipv6Addr::from_bytes(
                    get_ipv6_address_from_addr(destination).addr,
                ),
            },
        )
    }

    /// Waits for the `controller` to be fully initialized.
    ///
    /// Multicast forwarding is enabled at the protocol level when a controller is
    /// instantiated. However, controller creation is asynchronous. As a result,
    /// some operations may race with the creation of a controller. In such a case,
    /// callers should invoke this function to ensure that the controller is fully
    /// initialized.
    async fn wait_for_controller_to_start(&self, controller: &I::TableControllerProxy) {
        let multicast_addr = IpAddrType::Multicast.address(I::VERSION);
        let invalid_addresses =
            self.create_unicast_source_and_multicast_destination(multicast_addr, multicast_addr);
        assert_eq!(
            controller
                .add_route(invalid_addresses, self.default_multicast_route())
                .await
                .expect("send request"),
            Err(AddRouteError::InvalidAddress)
        );
    }
}

/// The device that should send multicast packets.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum SourceDevice {
    /// The router should use the interface that is connected to the provided
    /// server to send multicast packets.
    Router(Server),
    /// The proivded server should send multicast packets.
    Server(Server),
}

/// Defaultable options for configuring a `MulticastForwardingNetwork`.
#[derive(Derivative)]
#[derivative(Default)]
struct MulticastForwardingNetworkOptions {
    /// The TTL of multicast packets sent over the network.
    #[derivative(Default(value = "2"))]
    packet_ttl: u8,
    /// The device that sends multicast packets.
    #[derivative(Default(value = "SourceDevice::Server(Server::A)"))]
    source_device: SourceDevice,
    #[derivative(Default(value = "true"))]
    enable_multicast_forwarding: bool,
    /// If true, then the router will also act as a listener for multicast
    /// packets.
    #[derivative(Default(value = "false"))]
    listen_from_router: bool,
}

/// Configuration for a device that is connected to a router.
///
/// The device and the router are connected via the interfaces that correspond
/// to `ep_addr` and `router_ep_addr`.
struct RouterConnectedDeviceConfig {
    /// The address of an interface that should be added to the device.
    ep_addr: fnet::Subnet,
    /// The address of a connecting interface that should be added to the
    /// router.
    router_ep_addr: fnet::Subnet,
}

/// A router connected device that has been initialized from a
/// `RouterConnectedDeviceConfig`.
struct RouterConnectedDevice<'a> {
    realm: netemul::TestRealm<'a>,
    ep_addr: fnet::Subnet,
    router_ep_addr: fnet::Subnet,
    router_interface: netemul::TestInterface<'a>,

    _network: netemul::TestNetwork<'a>,
    interface: netemul::TestInterface<'a>,
}

/// A device that is connected to a router and is listening for multicast
/// traffic.
struct ClientDevice<'a> {
    device: RouterConnectedDevice<'a>,
    socket: fasync::net::UdpSocket,
}

fn get_ipv4_address_from_addr(addr: fnet::IpAddress) -> fnet::Ipv4Address {
    assert_matches!(addr, fnet::IpAddress::Ipv4(ipv4) => ipv4)
}

fn get_ipv6_address_from_addr(addr: fnet::IpAddress) -> fnet::Ipv6Address {
    assert_matches!(addr, fnet::IpAddress::Ipv6(ipv6) => ipv6)
}

async fn expect_closed_for_reason<I: Ip + FidlMulticastAdminIpExt>(
    controller: I::TableControllerProxy,
    expected_reason: fnet_multicast_admin::TableControllerCloseReason,
) {
    let reason = controller
        .take_event_stream()
        .try_next()
        .await
        .expect("failed to read controller event")
        .expect("event stream ended unexpectedly");
    assert_eq!(reason, expected_reason);
    assert_eq!(controller.on_closed().await, Ok(zx::Signals::CHANNEL_PEER_CLOSED));
}

/// Adds the `addr` to the `interface`.
async fn add_address(interface: &netemul::TestInterface<'_>, addr: fnet::Subnet) {
    let address_state_provider = interfaces::add_address_wait_assigned(
        &interface.control(),
        addr,
        fidl_fuchsia_net_interfaces_admin::AddressParameters::default(),
    )
    .await
    .expect("add_address_wait_assigned failed");
    let () = address_state_provider.detach().expect("detach failed");
}

/// Creates a `RouterConnectedDevice` from the provided `config` that is
/// connected to the `router_realm`.
async fn create_router_connected_device<'a, N: Netstack>(
    name: String,
    sandbox: &'a netemul::TestSandbox,
    router_realm: &'a netemul::TestRealm<'a>,
    config: RouterConnectedDeviceConfig,
) -> RouterConnectedDevice<'a> {
    let realm = sandbox
        .create_netstack_realm::<N, _>(format!("{}_realm", name))
        .expect("create_netstack_realm failed");
    let network =
        sandbox.create_network(format!("{}_network", name)).await.expect("create_network failed");
    let interface = realm
        .join_network(&network, format!("{}-ep", name))
        .await
        .expect("join_network failed for router connected device");
    add_address(&interface, config.ep_addr).await;

    let router_interface = router_realm
        .join_network(&network, format!("{}-router-ep", name))
        .await
        .expect("join_network failed for router");
    add_address(&router_interface, config.router_ep_addr).await;

    RouterConnectedDevice {
        realm: realm,
        _network: network,
        interface: interface,
        ep_addr: config.ep_addr,
        router_ep_addr: config.router_ep_addr,
        router_interface: router_interface,
    }
}

/// Creates a socket that has joined the `multicast_addr` group.
///
/// The socket joins the group using the interface that corresponds to
/// `interface_address`.
async fn create_listener_socket<'a>(
    realm: &'a netemul::TestRealm<'a>,
    version: IpVersion,
    id: u64,
    interface_address: fnet::Subnet,
    multicast_addr: std::net::SocketAddr,
) -> fasync::net::UdpSocket {
    let socket = fasync::net::UdpSocket::bind_in_realm(realm, multicast_addr)
        .await
        .expect("bind_in_realm failed");

    match version {
        IpVersion::V4 => {
            let iface_addr = get_ipv4_address_from_subnet(interface_address);
            let multicast_v4_addr = match multicast_addr.ip() {
                std::net::IpAddr::V4(ipv4) => ipv4,
                std::net::IpAddr::V6(ipv6) => {
                    panic!("multicast_addr unexpectedly IPv6: {:?}", ipv6)
                }
            };
            socket
                .as_ref()
                .join_multicast_v4(&multicast_v4_addr, &iface_addr.addr.into())
                .expect("join_multicast_v4 failed");
        }
        IpVersion::V6 => {
            let multicast_v6_addr = match multicast_addr.ip() {
                std::net::IpAddr::V4(ipv4) => {
                    panic!("multicast_addr unexpectedly IPv4: {:?}", ipv4)
                }
                std::net::IpAddr::V6(ipv6) => ipv6,
            };
            socket
                .as_ref()
                .join_multicast_v6(
                    &multicast_v6_addr,
                    u32::try_from(id).unwrap_or_else(|e| {
                        panic!("failed to convert {} to u32 with error: {:?}", id, e)
                    }),
                )
                .expect("join_multicast_v6 failed");
        }
    }

    socket
}

/// Sets the multicast forwarding state (enabled or disabled) for the provided
/// `interface`.
async fn set_multicast_forwarding(interface: &fnet_interfaces_ext::admin::Control, enabled: bool) {
    let config = fnet_interfaces_admin::Configuration {
        ipv4: Some(fnet_interfaces_admin::Ipv4Configuration {
            multicast_forwarding: Some(enabled),
            ..Default::default()
        }),
        ipv6: Some(fnet_interfaces_admin::Ipv6Configuration {
            multicast_forwarding: Some(enabled),
            ..Default::default()
        }),
        ..Default::default()
    };

    let _prev_config: fnet_interfaces_admin::Configuration = interface
        .set_configuration(&config)
        .await
        .expect("set_configuration failed")
        .expect("set_configuration error");
}

/// Returns an `fnet::Ipv4Address` from the `subnet` or panics if one does not
/// exist.
fn get_ipv4_address_from_subnet(subnet: fnet::Subnet) -> fnet::Ipv4Address {
    assert_matches!(subnet.addr, fnet::IpAddress::Ipv4(ipv4) => ipv4)
}

fn create_socket_addr(addr: fnet::IpAddress) -> std::net::SocketAddr {
    const PORT: u16 = 1234;
    match addr {
        fnet::IpAddress::Ipv4(fnet::Ipv4Address { addr }) => {
            std::net::SocketAddr::new(std::net::IpAddr::V4(addr.into()), PORT)
        }
        fnet::IpAddress::Ipv6(fnet::Ipv6Address { addr }) => {
            std::net::SocketAddr::new(std::net::IpAddr::V6(addr.into()), PORT)
        }
    }
}

/// Configuration for a client device that has joined a multicast group.
struct ClientConfig {
    route_min_ttl: u8,
    expect_forwarded_packet: bool,
}

/// The address of a particular device.
enum DeviceAddress {
    /// The address of a router interface that is connected to the provided
    /// server.
    Router(Server),
    /// The address of a `Server`.
    Server(Server),
    /// A manually specified address.
    Other(IpAddrType),
}

/// An action that should be executed with the multicast routing controller.
enum ControllerAction {
    None,
    /// Drop the controller (causing the route table to be dropped).
    Drop,
    /// Read the next routing event and expect a missing route event.
    ExpectMissingRouteEvent,
    /// Read the next routing event and expect a wrong input interface event.
    ExpectWrongInputInterfaceEvent,
}

/// Defaultable options for a multicast forwarding test.
#[derive(Derivative)]
#[derivative(Default)]
struct MulticastForwardingTestOptions {
    #[derivative(Default(value = "Server::A"))]
    route_input_interface: Server,
    #[derivative(Default(value = "DeviceAddress::Server(Server::A)"))]
    route_source_address: DeviceAddress,
    #[derivative(Default(value = "ControllerAction::None"))]
    controller_action: ControllerAction,
}

#[netstack_test]
#[variant(N, Netstack)]
#[variant(I, Ip)]
#[test_case(
    "ttl_same_as_route_min_ttl",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: true,
        },
        Client::B => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: true,
        }
    },
    MulticastForwardingNetworkOptions {
        packet_ttl: 1,
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A],
    MulticastForwardingTestOptions::default();
    "ttl same as route min ttl"
)]
#[test_case(
    "packet_sent_from_router",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: true,
        },
        Client::B => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: true,
        }
    },
    MulticastForwardingNetworkOptions {
        packet_ttl: 1,
        source_device: SourceDevice::Router(Server::A),
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A],
    MulticastForwardingTestOptions {
        route_source_address: DeviceAddress::Router(Server::A),
        ..MulticastForwardingTestOptions::default()
    };
    "packet_sent_from_router"
)]
#[test_case(
    "ttl_greater_than_and_less_than_route_min_ttl",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: true,
        },
        Client::B => ClientConfig {
            route_min_ttl: 3,
            expect_forwarded_packet: false,
        }
    },
    MulticastForwardingNetworkOptions {
        packet_ttl: 2,
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A],
    MulticastForwardingTestOptions::default();
    "ttl greater than and less than route min ttl"
)]
#[test_case(
    "unexpected_input_interface",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: false,
        }
    },
    MulticastForwardingNetworkOptions {
        source_device: SourceDevice::Server(Server::A),
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A, Server::B],
    MulticastForwardingTestOptions {
        route_source_address: DeviceAddress::Server(Server::A),
        route_input_interface: Server::B,
        controller_action: ControllerAction::ExpectWrongInputInterfaceEvent,
        ..MulticastForwardingTestOptions::default()
    };
    "unexpected input interface"
)]
#[test_case(
    "multicast_forwarding_disabled",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: false,
        }
    },
    MulticastForwardingNetworkOptions {
        enable_multicast_forwarding: false,
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A],
    MulticastForwardingTestOptions::default();
    "multicast forwarding disabled"
)]
#[test_case(
    "multicast_controller_dropped",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            // Dropping the multicast controller results in the routing
            // table being cleared. As a result, the packet should not be
            // forwarded.
            expect_forwarded_packet: false,
        }
    },
    MulticastForwardingNetworkOptions::default(),
    vec![Server::A],
    MulticastForwardingTestOptions {
        controller_action: ControllerAction::Drop,
        ..MulticastForwardingTestOptions::default()
    };
    "multicast controller dropped"
)]
#[test_case(
    "forwarded_and_delivered_locally",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: true,
        }
    },
    MulticastForwardingNetworkOptions {
        listen_from_router: true,
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A],
    MulticastForwardingTestOptions::default();
    "forwarded and delivered locally"
)]
#[test_case(
    "only_delivered_locally",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 2,
            expect_forwarded_packet: false,
        }
    },
    MulticastForwardingNetworkOptions {
        packet_ttl: 1,
        listen_from_router: true,
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A],
    MulticastForwardingTestOptions::default();
    "only delivered locally"
)]
#[test_case(
    "missing_route",
    hashmap! {
        Client::A => ClientConfig {
            route_min_ttl: 1,
            expect_forwarded_packet: false,
        }
    },
    MulticastForwardingNetworkOptions {
        source_device: SourceDevice::Server(Server::A),
        listen_from_router: true,
        ..MulticastForwardingNetworkOptions::default()
    },
    vec![Server::A, Server::B],
    MulticastForwardingTestOptions {
        route_source_address: DeviceAddress::Server(Server::B),
        controller_action: ControllerAction::ExpectMissingRouteEvent,
        ..MulticastForwardingTestOptions::default()
    };
    "missing route"
)]
async fn multicast_forwarding<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(
    name: &str,
    case_name: &str,
    clients: HashMap<Client, ClientConfig>,
    network_options: MulticastForwardingNetworkOptions,
    servers: Vec<Server>,
    options: MulticastForwardingTestOptions,
) {
    let MulticastForwardingTestOptions {
        route_input_interface,
        route_source_address,
        controller_action,
    } = options;
    let test_name = format!("{}_{}", name, case_name);
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(&test_name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        &test_name,
        &sandbox,
        &router_realm,
        servers,
        clients.keys().cloned().collect(),
        network_options,
    )
    .await;
    let mut controller = Some(test_network.create_multicast_controller());

    let addresses = test_network.create_unicast_source_and_multicast_destination(
        test_network.get_device_address(route_source_address),
        IpAddrType::Multicast.address(I::VERSION),
    );

    let outgoing_interfaces = clients
        .iter()
        .map(|(client, config)| fnet_multicast_admin::OutgoingInterfaces {
            id: test_network.get_client(*client).device.router_interface.id(),
            min_ttl: config.route_min_ttl,
        })
        .collect();

    let route = fnet_multicast_ext::Route {
        expected_input_interface: test_network
            .get_server(route_input_interface)
            .router_interface
            .id(),
        action: fnet_multicast_admin::Action::OutgoingInterfaces(outgoing_interfaces),
    };

    controller
        .as_ref()
        .expect("controller not present")
        .add_route(addresses, route)
        .await
        .expect("failed to send request")
        .expect("add_route error");

    match controller_action {
        ControllerAction::Drop => {
            drop(controller.take());
            test_network.wait_for_packet_to_become_unrouteable().await;
        }
        ControllerAction::None
        | ControllerAction::ExpectMissingRouteEvent
        | ControllerAction::ExpectWrongInputInterfaceEvent => {
            let expectations = clients
                .into_iter()
                .map(|(client, config)| (client, config.expect_forwarded_packet))
                .collect();
            test_network.send_and_receive_multicast_packet(expectations).await;
        }
    }

    let expected_event = match controller_action {
        ControllerAction::ExpectMissingRouteEvent => {
            Some(fnet_multicast_admin::RoutingEvent::MissingRoute(fnet_multicast_admin::Empty {}))
        }
        ControllerAction::ExpectWrongInputInterfaceEvent => {
            Some(fnet_multicast_admin::RoutingEvent::WrongInputInterface(
                fnet_multicast_admin::WrongInputInterface {
                    expected_input_interface: Some(
                        test_network.get_server(route_input_interface).router_interface.id(),
                    ),
                    ..Default::default()
                },
            ))
        }
        ControllerAction::None | ControllerAction::Drop => None,
    };

    match expected_event {
        None => {}
        Some(expected_event) => {
            let WatchRoutingEventsResponse { dropped_events, addresses, input_interface, event } =
                controller
                    .expect("controller not present")
                    .watch_routing_events()
                    .await
                    .expect("watch_routing_events failed");
            assert_eq!(dropped_events, 0);
            assert_eq!(addresses, test_network.default_unicast_source_and_multicast_destination());
            assert_eq!(input_interface, test_network.get_source_router_interface_id());
            assert_eq!(event, expected_event);
        }
    }
}

/// An interface owned by the router.
enum RouterInterface {
    /// A router interface connected to a particular `Client`.
    Client(Client),
    /// A router interface connected to a particular 'Server'.
    Server(Server),
    /// A router interface that does not correspond to a `Client` or a `Server`.
    Other(u64),
}

/// Configuration for a `fnet_multicast_admin::Action`.
enum RouteAction {
    /// A `fnet_multicast_admin::Action::OutgoingInterfaces` action.
    OutgoingInterfaces(Vec<RouterInterface>),
}

/// Defaultable options for configuring a multicast route in an add multicast
/// route test.
#[derive(Derivative)]
#[derivative(Default)]
struct AddMulticastRouteTestOptions {
    #[derivative(Default(value = "Some(RouterInterface::Server(Server::A))"))]
    input_interface: Option<RouterInterface>,
    #[derivative(Default(
        value = "Some(RouteAction::OutgoingInterfaces(vec![RouterInterface::Client(Client::A)]))"
    ))]
    action: Option<RouteAction>,
    #[derivative(Default(value = "DeviceAddress::Server(Server::A)"))]
    source_address: DeviceAddress,
    #[derivative(Default(value = "IpAddrType::Multicast"))]
    destination_address: IpAddrType,
    #[derivative(Default(value = "false"))]
    send_before_route: bool,
}

#[netstack_test]
#[variant(N, Netstack)]
#[variant(I, Ip)]
#[test_case(
    "success",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: true,
    },
    Ok(()),
    vec![Server::A],
    AddMulticastRouteTestOptions::default();
    "success"
)]
#[test_case(
    "success_with_pending_packet",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: true,
    },
    Ok(()),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        send_before_route: true,
        ..Default::default()
    };
    "success with pending packet"
)]
#[test_case(
    "unexpected_input_interface",
    ClientConfig {
        route_min_ttl: 1,
        // A route is successfully added, but its input interface does not
        // match the interface that the packet arrived on. As a result, the
        // pending packet should not be forwarded.
        expect_forwarded_packet: false,
    },
    Ok(()),
    vec![Server::A, Server::B],
    AddMulticastRouteTestOptions {
        input_interface: Some(RouterInterface::Server(Server::B)),
        ..AddMulticastRouteTestOptions::default()
    };
    "unexpected input interface"
)]
#[test_case(
    "unknown_input_interface",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InterfaceNotFound),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        input_interface: Some(RouterInterface::Other(1000)),
        ..AddMulticastRouteTestOptions::default()
    };
    "unknown input interface"
)]
#[test_case(
    "unknown_output_interface",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InterfaceNotFound),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        action: Some(RouteAction::OutgoingInterfaces(vec![RouterInterface::Other(1000)])),
        ..AddMulticastRouteTestOptions::default()
    };
    "unknown output interface"
)]
#[test_case(
    "input_interface_matches_output_interface",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InputCannotBeOutput),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        input_interface: Some(RouterInterface::Server(Server::A)),
        action: Some(RouteAction::OutgoingInterfaces(vec![RouterInterface::Server(Server::A)])),
        ..AddMulticastRouteTestOptions::default()
    };
    "input interface matches output interface"
)]
#[test_case(
    "duplicate_output_interface",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::DuplicateOutput),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        input_interface: Some(RouterInterface::Client(Client::A)),
        action: Some(RouteAction::OutgoingInterfaces(vec![
            RouterInterface::Server(Server::A),
            RouterInterface::Server(Server::A)])),
        ..AddMulticastRouteTestOptions::default()
    };
    "duplicate output interface"
)]
#[test_case(
    "no_outgoing_interfaces",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::RequiredRouteFieldsMissing),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        action: Some(RouteAction::OutgoingInterfaces(vec![])),
        ..AddMulticastRouteTestOptions::default()
    };
    "no outgoing interfaces"
)]
#[test_case(
    "expected_input_interface_not_specified",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::RequiredRouteFieldsMissing),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        input_interface: None,
        ..AddMulticastRouteTestOptions::default()
    };
    "expected input interface not specified"
)]
#[test_case(
    "action_not_specified",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::RequiredRouteFieldsMissing),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        action: None,
        ..AddMulticastRouteTestOptions::default()
    };
    "action not specified"
)]
#[test_case(
    "multicast_source_address",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InvalidAddress),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        source_address: DeviceAddress::Other(IpAddrType::Multicast),
        ..AddMulticastRouteTestOptions::default()
    };
    "multicast source address"
)]
#[test_case(
    "any_source_address",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InvalidAddress),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        source_address: DeviceAddress::Other(IpAddrType::Any),
        ..AddMulticastRouteTestOptions::default()
    };
    "any source address"
)]
#[test_case(
    "link_local_unicast_source_address",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InvalidAddress),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        source_address: DeviceAddress::Other(IpAddrType::LinkLocalUnicast),
        ..AddMulticastRouteTestOptions::default()
    };
    "link-local unicast source address"
)]
#[test_case(
    "unicast_destination_address",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InvalidAddress),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        destination_address: IpAddrType::Unicast,
        ..AddMulticastRouteTestOptions::default()
    };
    "unicast destination address"
)]
#[test_case(
    "link_local_multicast_destination_address",
    ClientConfig {
        route_min_ttl: 1,
        expect_forwarded_packet: false,
    },
    Err(AddRouteError::InvalidAddress),
    vec![Server::A],
    AddMulticastRouteTestOptions {
        destination_address: IpAddrType::LinkLocalMulticast,
        ..AddMulticastRouteTestOptions::default()
    };
    "link-local multicast destination address"
)]
async fn add_multicast_route<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(
    name: &str,
    case_name: &str,
    client: ClientConfig,
    expected_add_route_result: Result<(), AddRouteError>,
    servers: Vec<Server>,
    options: AddMulticastRouteTestOptions,
) {
    let AddMulticastRouteTestOptions {
        input_interface,
        action,
        source_address,
        destination_address,
        send_before_route,
    } = options;

    let test_name = format!("{}_{}", name, case_name);
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(&test_name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        &test_name,
        &sandbox,
        &router_realm,
        servers,
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();

    // The queuing of a pending packet below may race with the creation of the
    // multicast controller. As a result, a wait point is inserted to ensure
    // that the controller is fully initialized before a packet is sent.
    test_network.wait_for_controller_to_start(&controller).await;

    let addresses = test_network.create_unicast_source_and_multicast_destination(
        test_network.get_device_address(source_address),
        destination_address.address(I::VERSION),
    );

    let get_interface_id = |interface| -> u64 {
        match interface {
            RouterInterface::Server(server) => {
                test_network.get_server(server).router_interface.id()
            }
            RouterInterface::Client(client) => {
                test_network.get_client(client).device.router_interface.id()
            }
            RouterInterface::Other(id) => id,
        }
    };

    if send_before_route {
        // Queue a packet that could potentially be forwarded once a multicast route
        // is installed.
        test_network.send_multicast_packet().await;
        // Wait for the "Missing Route" event to ensure the packet has been
        // queued in the pending table.
        let WatchRoutingEventsResponse { dropped_events, addresses, input_interface, event } =
            controller.watch_routing_events().await.expect("watch_routing_events failed");
        assert_eq!(dropped_events, 0);
        assert_eq!(addresses, test_network.default_unicast_source_and_multicast_destination());
        assert_eq!(input_interface, test_network.get_source_router_interface_id());
        assert_eq!(
            event,
            fnet_multicast_admin::RoutingEvent::MissingRoute(fnet_multicast_admin::Empty {})
        );
    }

    let route_action = action.and_then(|action| {
        let outgoing_interfaces = match action {
            RouteAction::OutgoingInterfaces(interfaces) => interfaces
                .into_iter()
                .map(|interface| fnet_multicast_admin::OutgoingInterfaces {
                    id: get_interface_id(interface),
                    min_ttl: client.route_min_ttl,
                })
                .collect(),
        };
        Some(fnet_multicast_admin::Action::OutgoingInterfaces(outgoing_interfaces))
    });
    let route = fnet_multicast_admin::Route {
        expected_input_interface: input_interface
            .and_then(|interface| Some(get_interface_id(interface))),
        action: route_action,
        ..Default::default()
    };

    // NB: Circumvent `fnet_multicast_ext::TableControllerProxy` APIs and
    // interact directly with the FIDL. This allows us to send invalid requests
    // that the extension library prevents with the type system.
    #[derive(GenericOverIp)]
    #[generic_over_ip(I, Ip)]
    struct ProxyHolder<'a, I: Ip + FidlMulticastAdminIpExt>(&'a I::TableControllerProxy);

    let result = I::map_ip_in(
        (addresses, ProxyHolder(&controller)),
        |(addresses, ProxyHolder(controller))| {
            futures::future::Either::Left(
                controller
                    .add_route(&addresses.into(), &route)
                    .map(|r| r.map(|r| r.map_err(fnet_multicast_ext::AddRouteError::from))),
            )
        },
        |(addresses, ProxyHolder(controller))| {
            futures::future::Either::Right(
                controller
                    .add_route(&addresses.into(), &route)
                    .map(|r| r.map(|r| r.map_err(fnet_multicast_ext::AddRouteError::from))),
            )
        },
    )
    .await
    .expect("send request");

    assert_eq!(result, expected_add_route_result);

    if !send_before_route {
        test_network.send_multicast_packet().await;
    }

    test_network
        .receive_multicast_packet(hashmap! {
            Client::A => client.expect_forwarded_packet,
        })
        .await;
}

/// Verify that installing a multicast route overwrites an existing route with
/// the same src/dst address tuple.
#[netstack_test]
#[variant(N, Netstack)]
#[variant(I, Ip)]
async fn overwrite_multicast_route<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(name: &str) {
    // Setup a network with one server and two clients.
    // Originally, install a multicast route to forward packets to Client A, and
    // later overwrite it with a multicast route to forward packets to Client B.
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        name,
        &sandbox,
        &router_realm,
        vec![Server::A],
        vec![Client::A, Client::B],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let addresses = test_network.create_unicast_source_and_multicast_destination(
        test_network.get_device_address(DeviceAddress::Server(Server::A)),
        IpAddrType::Multicast.address(I::VERSION),
    );

    // Construct a route action to forward packets to the given client.
    let route_action_forward_to_client = |client| {
        fnet_multicast_admin::Action::OutgoingInterfaces(vec![
            fnet_multicast_admin::OutgoingInterfaces {
                id: test_network.get_client(client).device.router_interface.id(),
                min_ttl: 1,
            },
        ])
    };

    let controller = test_network.create_multicast_controller();

    for (client_with_route, client_without_route) in
        [(Client::A, Client::B), (Client::B, Client::A)]
    {
        let route = fnet_multicast_ext::Route {
            expected_input_interface: test_network.get_server(Server::A).router_interface.id(),
            action: route_action_forward_to_client(client_with_route),
        };
        controller
            .add_route(addresses.clone(), route)
            .await
            .expect("send request")
            .expect("add route should succeed");
        // Send & receive a multicast packet, verifying the correct client
        // receives the packet.
        test_network
            .send_and_receive_multicast_packet(hashmap! {
                client_with_route => true,
                client_without_route => false,
            })
            .await;
    }
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
async fn multiple_multicast_controllers<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        name,
        &sandbox,
        &router_realm,
        vec![Server::A],
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();
    test_network.add_default_route(&controller).await;

    let closed_controller = test_network.create_multicast_controller();
    expect_closed_for_reason::<I>(
        closed_controller,
        fnet_multicast_admin::TableControllerCloseReason::AlreadyInUse,
    )
    .await;

    // The closed controller should not impact the already active controller.
    // Consequently, a packet should still be forwardable using the route added
    // above.
    test_network
        .send_and_receive_multicast_packet(hashmap! {
            Client::A => true,
        })
        .await;
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
async fn watch_routing_events_hanging<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        name,
        &sandbox,
        &router_realm,
        vec![Server::A],
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();

    test_network.wait_for_controller_to_start(&controller).await;

    let watch_routing_events_fut = async {
        let WatchRoutingEventsResponse { dropped_events, addresses, input_interface, event } =
            controller.watch_routing_events().await.expect("watch_routing_events failed");
        assert_eq!(dropped_events, 0);
        assert_eq!(addresses, test_network.default_unicast_source_and_multicast_destination());
        assert_eq!(input_interface, test_network.get_source_router_interface_id());
        assert_eq!(
            event,
            fnet_multicast_admin::RoutingEvent::MissingRoute(fnet_multicast_admin::Empty {})
        );
    };

    let send_packet_fut = async {
        const WAIT_BEFORE_SEND_DURATION: zx::MonotonicDuration =
            zx::MonotonicDuration::from_seconds(5);
        // Before sending a packet, sleep for a few seconds to ensure that the
        // call to watch_routing_events hangs.
        netstack_testing_common::sleep(WAIT_BEFORE_SEND_DURATION.into_seconds()).await;
        test_network.send_multicast_packet().await;
    };

    let ((), ()) = futures::future::join(watch_routing_events_fut, send_packet_fut).await;
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
async fn watch_routing_events_already_hanging<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(
    name: &str,
) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        name,
        &sandbox,
        &router_realm,
        vec![Server::A],
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();
    test_network.add_default_route(&controller).await;

    // Before the controller is closed due to multiple hanging gets, multicast
    // packets should be forwarded.
    test_network.send_and_receive_multicast_packet(hashmap! { Client::A => true }).await;

    async fn watch_routing_events<I: Ip + FidlMulticastAdminIpExt>(
        controller: &I::TableControllerProxy,
    ) {
        let err =
            controller.watch_routing_events().await.expect_err("should fail with PEER_CLOSED");
        let status = assert_matches!(err, fidl::Error::ClientChannelClosed{status, ..} => status);
        assert_eq!(status, zx::Status::PEER_CLOSED);
    }

    let ((), ()) = futures::future::join(
        watch_routing_events::<I>(&controller),
        watch_routing_events::<I>(&controller),
    )
    .await;

    expect_closed_for_reason::<I>(
        controller,
        fnet_multicast_admin::TableControllerCloseReason::HangingGetError,
    )
    .await;

    // The routing table should be dropped when the controller is closed. As a
    // result, a packet should no longer be forwarded.
    test_network.wait_for_packet_to_become_unrouteable().await;
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
async fn watch_multiple_routing_events<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        name,
        &sandbox,
        &router_realm,
        vec![Server::A],
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();

    test_network.wait_for_controller_to_start(&controller).await;

    // NB: Send to two separate destination addresses. This ensures the Netstack
    // will publish two unique events.
    const DST_ADDRS: [IpAddrType; 2] = [IpAddrType::Multicast, IpAddrType::OtherMulticast];
    for dst_addr in DST_ADDRS {
        test_network.send_multicast_packet_with_overrides(Some(dst_addr)).await;
    }

    // After sending the packet, sleep for a few seconds to allow the netstack
    // time to receive the packets and publish events. This helps reduce the
    // rate of falsely passing.
    const WAIT_AFTER_SEND_DURATION: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(1);
    netstack_testing_common::sleep(WAIT_AFTER_SEND_DURATION.into_seconds()).await;

    // Verify both "Missing Route" events are observed in the correct order.
    for dst_addr in DST_ADDRS {
        let WatchRoutingEventsResponse { dropped_events, addresses, input_interface, event } =
            controller
                .watch_routing_events()
                .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT.after_now(), || {
                    panic!("timed out waiting for event")
                })
                .await
                .expect("watch_routing_events failed");

        let src_addr = test_network.get_source_address();
        let dst_addr = dst_addr.address(I::VERSION);
        let expected_addr =
            test_network.create_unicast_source_and_multicast_destination(src_addr, dst_addr);

        assert_eq!(dropped_events, 0);
        assert_eq!(addresses, expected_addr);
        assert_eq!(input_interface, test_network.get_source_router_interface_id());
        assert_eq!(
            event,
            fnet_multicast_admin::RoutingEvent::MissingRoute(fnet_multicast_admin::Empty {})
        );
    }
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
async fn watch_routing_events_dropped_events<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(
    name: &str,
) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        name,
        &sandbox,
        &router_realm,
        vec![Server::A, Server::B],
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();

    let addresses = test_network.default_unicast_source_and_multicast_destination();
    let route = fnet_multicast_ext::Route {
        expected_input_interface: test_network.get_server(Server::B).router_interface.id(),
        action: fnet_multicast_admin::Action::OutgoingInterfaces(vec![
            fnet_multicast_admin::OutgoingInterfaces {
                id: test_network.get_client(Client::A).device.router_interface.id(),
                min_ttl: 1,
            },
        ]),
    };

    controller.add_route(addresses, route).await.expect("send request").expect("add_route error");

    async fn add_wrong_input_interface_events<I: Ip + FidlMulticastAdminIpExt>(
        test_network: &MulticastForwardingNetwork<'_, I>,
        num: u16,
    ) {
        const WAIT_AFTER_SEND_DURATION: zx::MonotonicDuration =
            zx::MonotonicDuration::from_seconds(5);
        futures::stream::iter(0..num)
            .for_each_concurrent(None, |_| async {
                test_network.send_multicast_packet().await;
            })
            .await;
        // Insert a brief sleep to ensure that events are fully queued before
        // checking expectations.
        netstack_testing_common::sleep(WAIT_AFTER_SEND_DURATION.into_seconds()).await;
    }

    async fn expect_num_dropped_events<I: Ip + FidlMulticastAdminIpExt>(
        controller: &I::TableControllerProxy,
        expected_num_dropped_events: u64,
    ) {
        let WatchRoutingEventsResponse { dropped_events, .. } =
            controller.watch_routing_events().await.expect("watch_routing_events failed");
        assert_eq!(dropped_events, expected_num_dropped_events);
    }

    // Add the maximum number of events to the buffer. No events should be
    // dropped.
    add_wrong_input_interface_events(&test_network, fnet_multicast_admin::MAX_ROUTING_EVENTS).await;
    expect_num_dropped_events::<I>(&controller, 0).await;

    // Push the max events buffer over the limit. Events should be dropped.
    add_wrong_input_interface_events(&test_network, 3).await;
    expect_num_dropped_events::<I>(&controller, 2).await;

    // Immediately reading the next event should result in the dropped events
    // counter getting reset.
    expect_num_dropped_events::<I>(&controller, 0).await;
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
#[test_case(
    "success",
    DeviceAddress::Server(Server::A),
    IpAddrType::Multicast,
    vec![Server::A],
    Ok(());
    "success"
)]
#[test_case(
    "no_matching_route_for_source_address",
    DeviceAddress::Server(Server::B),
    IpAddrType::Multicast,
    vec![Server::A, Server::B],
    Err(DelRouteError::NotFound);
    "no matching route for source address"
)]
#[test_case(
    "no_matching_route_for_destination_address",
    DeviceAddress::Server(Server::A),
    IpAddrType::OtherMulticast,
    vec![Server::A],
    Err(DelRouteError::NotFound);
    "no matching route for destination address"
)]
#[test_case(
    "multicast_source_address",
    DeviceAddress::Other(IpAddrType::Multicast),
    IpAddrType::Multicast,
    vec![Server::A],
    Err(DelRouteError::InvalidAddress);
    "multicast source address"
)]
#[test_case(
    "any_source_address",
    DeviceAddress::Other(IpAddrType::Any),
    IpAddrType::Multicast,
    vec![Server::A],
    Err(DelRouteError::InvalidAddress);
    "any source address"
)]
#[test_case(
    "link_local_unicast_source_address",
    DeviceAddress::Other(IpAddrType::LinkLocalUnicast),
    IpAddrType::Multicast,
    vec![Server::A],
    Err(DelRouteError::InvalidAddress);
    "link local unicast source address"
)]
#[test_case(
    "unicast_destination_address",
    DeviceAddress::Server(Server::A),
    IpAddrType::Unicast,
    vec![Server::A],
    Err(DelRouteError::InvalidAddress);
    "unicast destination address"
)]
#[test_case(
    "link_local_multicast_destination_address",
    DeviceAddress::Server(Server::A),
    IpAddrType::LinkLocalMulticast,
    vec![Server::A],
    Err(DelRouteError::InvalidAddress);
    "link local multicast destination address"
)]
async fn del_multicast_route<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(
    name: &str,
    case_name: &str,
    source_address: DeviceAddress,
    destination_address: IpAddrType,
    servers: Vec<Server>,
    expected_del_route_result: Result<(), DelRouteError>,
) {
    let test_name = format!("{}_{}", name, case_name);
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(&test_name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        &test_name,
        &sandbox,
        &router_realm,
        servers,
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();
    test_network.add_default_route(&controller).await;

    // Before the route is removed, multicast packets should be successfully
    // forwarded.
    test_network
        .send_and_receive_multicast_packet(hashmap! {
            Client::A => true,
        })
        .await;

    let del_addresses = test_network.create_unicast_source_and_multicast_destination(
        test_network.get_device_address(source_address),
        destination_address.address(I::VERSION),
    );

    assert_eq!(
        controller.del_route(del_addresses).await.expect("send_request"),
        expected_del_route_result
    );

    // After del_route has been called, multicast packets should no longer be
    // forwarded if the route was successfully removed.
    test_network
        .send_and_receive_multicast_packet(hashmap! {
            Client::A => expected_del_route_result.is_err(),
        })
        .await;
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
#[test_case(
    "no_matching_route_for_source_address",
    DeviceAddress::Server(Server::B),
    IpAddrType::Multicast,
    vec![Server::A, Server::B],
    GetRouteStatsError::NotFound;
    "no matching route for source address"
)]
#[test_case(
    "no_matching_route_for_destination_address",
    DeviceAddress::Server(Server::A),
    IpAddrType::OtherMulticast,
    vec![Server::A],
    GetRouteStatsError::NotFound;
    "no matching route for destination address"
)]
#[test_case(
    "multicast_source_address",
    DeviceAddress::Other(IpAddrType::Multicast),
    IpAddrType::Multicast,
    vec![Server::A],
    GetRouteStatsError::InvalidAddress;
    "multicast source address"
)]
#[test_case(
    "any_source_address",
    DeviceAddress::Other(IpAddrType::Any),
    IpAddrType::Multicast,
    vec![Server::A],
    GetRouteStatsError::InvalidAddress;
    "any source address"
)]
#[test_case(
    "link_local_unicast_source_address",
    DeviceAddress::Other(IpAddrType::LinkLocalUnicast),
    IpAddrType::Multicast,
    vec![Server::A],
    GetRouteStatsError::InvalidAddress;
    "link local unicast source address"
)]
#[test_case(
    "unicast_destination_address",
    DeviceAddress::Server(Server::A),
    IpAddrType::Unicast,
    vec![Server::A],
    GetRouteStatsError::InvalidAddress;
    "unicast destination address"
)]
#[test_case(
    "link_local_multicast_destination_address",
    DeviceAddress::Server(Server::A),
    IpAddrType::LinkLocalMulticast,
    vec![Server::A],
    GetRouteStatsError::InvalidAddress;
    "link local multicast destination address"
)]
async fn get_route_stats_errors<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(
    name: &str,
    case_name: &str,
    source_address: DeviceAddress,
    destination_address: IpAddrType,
    servers: Vec<Server>,
    expected_error: GetRouteStatsError,
) {
    let test_name = format!("{}_{}", name, case_name);
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(&test_name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        &test_name,
        &sandbox,
        &router_realm,
        servers,
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();
    test_network.add_default_route(&controller).await;

    let get_route_stats_addresses = test_network.create_unicast_source_and_multicast_destination(
        test_network.get_device_address(source_address),
        destination_address.address(I::VERSION),
    );

    assert_eq!(
        controller.get_route_stats(get_route_stats_addresses).await.expect("send request"),
        Err(expected_error)
    );
}

#[netstack_test]
#[variant(I, Ip)]
#[variant(N, Netstack)]
async fn get_route_stats<I: Ip + FidlMulticastAdminIpExt, N: Netstack>(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let router_realm = create_router_realm::<N>(name, &sandbox);
    let test_network = MulticastForwardingNetwork::<I>::new::<N>(
        name,
        &sandbox,
        &router_realm,
        vec![Server::A],
        vec![Client::A],
        MulticastForwardingNetworkOptions::default(),
    )
    .await;

    let controller = test_network.create_multicast_controller();
    test_network.add_default_route(&controller).await;

    async fn get_last_used_timestamp<I: Ip + FidlMulticastAdminIpExt>(
        controller: &I::TableControllerProxy,
        addresses: UnicastSourceAndMulticastDestination<I>,
    ) -> i64 {
        controller
            .get_route_stats(addresses)
            .await
            .expect("send_request")
            .expect("get_route_stats error")
            .last_used
            .expect("last_used missing value")
    }

    // The route should initially be assigned a timestamp that corresponds to
    // when it was created.
    let mut timestamp = get_last_used_timestamp(
        &controller,
        test_network.default_unicast_source_and_multicast_destination(),
    )
    .await;
    assert_gt!(timestamp, 0);

    // Verify that the timestamp is updated each time the route is used.
    for _ in 0..2 {
        fasync::Timer::new(std::time::Duration::from_millis(1)).await;
        test_network.send_and_receive_multicast_packet(hashmap! { Client::A => true }).await;
        let current_timestamp = get_last_used_timestamp(
            &controller,
            test_network.default_unicast_source_and_multicast_destination(),
        )
        .await;
        assert_gt!(current_timestamp, timestamp);
        timestamp = current_timestamp;
    }
}
