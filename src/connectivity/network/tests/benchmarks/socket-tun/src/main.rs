// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::io::{Read as _, Write as _};

use fidl_fuchsia_hardware_network as fhardware_network;
use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_ext as fnet_ext;
use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;
use fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext;
use fidl_fuchsia_net_resources as fnet_resources;
use fidl_fuchsia_net_routes_ext as fnet_routes_ext;
use fidl_fuchsia_net_tun as fnet_tun;
use fidl_fuchsia_posix_socket as fposix_socket;
use fidl_fuchsia_tracing_controller as ftracing_controller;
use fuchsia_async as fasync;
use fuchsiaperf::{Direction, Unit};
use futures::{AsyncReadExt as _, FutureExt as _};

use libc as _;
use net_declare::fidl_subnet;
use net_types::ip::{Ip, IpAddr, Ipv4, Ipv6};
use netemul::{PacketCapture, TestEndpoint, TestNetwork};
use netstack_testing_common::realms::{KnownServiceProvider, NetstackVersion};
use netstack_testing_common::{devices, interfaces};

/// Addressing for the direct topology, in which the client and server share a
/// link:
///
///   client <-> server
struct DirectSubnets {
    client: fnet::Subnet,
    server: fnet::Subnet,
}

/// Addressing for the forwarding topology, in which the client and server are
/// on separate subnets connected by a router:
///
///   client <-> (client gateway) router (server gateway) <-> server
struct ForwardingSubnets {
    client: fnet::Subnet,
    client_gateway: fnet::Subnet,
    server_gateway: fnet::Subnet,
    server: fnet::Subnet,
}

trait IpExt:
    Ip + fnet_routes_ext::FidlRouteIpExt + fnet_routes_ext::admin::FidlRouteAdminIpExt
{
    const DOMAIN: fposix_socket::Domain;
    const DIRECT: DirectSubnets;
    const FORWARDING: ForwardingSubnets;
}

impl IpExt for Ipv4 {
    const DOMAIN: fposix_socket::Domain = fposix_socket::Domain::Ipv4;
    const DIRECT: DirectSubnets = DirectSubnets {
        client: fidl_subnet!("192.168.1.1/24"),
        server: fidl_subnet!("192.168.1.2/24"),
    };
    const FORWARDING: ForwardingSubnets = ForwardingSubnets {
        client: fidl_subnet!("192.168.1.1/24"),
        client_gateway: fidl_subnet!("192.168.1.2/24"),
        server_gateway: fidl_subnet!("192.168.2.1/24"),
        server: fidl_subnet!("192.168.2.2/24"),
    };
}

impl IpExt for Ipv6 {
    const DOMAIN: fposix_socket::Domain = fposix_socket::Domain::Ipv6;
    const DIRECT: DirectSubnets = DirectSubnets {
        client: fidl_subnet!("2001:db8:1::1/64"),
        server: fidl_subnet!("2001:db8:1::2/64"),
    };
    const FORWARDING: ForwardingSubnets = ForwardingSubnets {
        client: fidl_subnet!("2001:db8:1::1/64"),
        client_gateway: fidl_subnet!("2001:db8:1::2/64"),
        server_gateway: fidl_subnet!("2001:db8:2::1/64"),
        server: fidl_subnet!("2001:db8:2::2/64"),
    };
}

fn generate_send_recv_bufs(size: usize) -> (Vec<u8>, Vec<u8>) {
    (
        // It may be natural to fill the buffer with a single byte, but a
        // property of sending such a message is that any permutation of
        // the bytes look identical, which hides certain TCP bugs where the
        // bytes sent have been permuted due to wrong sequence numbers, e.g.
        // https://fxbug.dev/42079275.
        (0u8..=254).cycle().take(size).collect(),
        // The choice of filling the receive buffer with a non-zero byte
        // is deliberate: the [zero page scanner] may reclaim large memory
        // filled with zeroes (note that it is turned off in environments
        // where the benchmark results are collected, but there is no harm
        // in doing this to be extra safe).
        //
        // [zero page scanner]: https://fuchsia.dev/fuchsia-src/gen/boot-options?hl=en#kernelpage-scannerzero-page-scans-per-seconduint64_t
        vec![255; size],
    )
}

// This type holds FIDL proxies so that the underlying object stays alive.
struct InterfaceFidlProxies {
    _device_control: fnet_interfaces_admin::DeviceControlProxy,
    control: fidl_fuchsia_net_interfaces_ext::admin::Control,
}

// This type holds FIDL proxies so that the underlying objects stay alive.
struct FidlProxies<'a> {
    links: Vec<Link<'a>>,
    _interfaces: Vec<InterfaceFidlProxies>,
}

/// A link between two netstacks.
enum Link<'a> {
    /// Backed by a Tun device pair.
    Tun(fnet_tun::DevicePairProxy),
    /// Backed by a netemul network, which supports packet capture.
    DebugNetwork {
        name: &'static str,
        net: TestNetwork<'a>,
        _ep1: TestEndpoint<'a>,
        _ep2: TestEndpoint<'a>,
    },
}

impl<'a> Link<'a> {
    /// Creates a link, returning it along with the ports at each of its ends.
    ///
    /// The link is backed by a netemul network if `capture_packets` is set,
    /// and by a Tun device pair otherwise.
    async fn new(
        sandbox: &'a netemul::TestSandbox,
        name: &'static str,
        capture_packets: bool,
    ) -> (Self, fhardware_network::PortProxy, fhardware_network::PortProxy) {
        if capture_packets {
            let net = sandbox.create_network(name).await.expect("create network");
            let (left_port, left_port_server_end) =
                fidl::endpoints::create_proxy::<fhardware_network::PortMarker>();
            let (right_port, right_port_server_end) =
                fidl::endpoints::create_proxy::<fhardware_network::PortMarker>();
            let ep1 = net
                .create_endpoint(format!("{name}-ep1"))
                .await
                .expect("failed to create endpoint");
            let ep2 = net
                .create_endpoint(format!("{name}-ep2"))
                .await
                .expect("failed to create endpoint");
            ep1.get_port(left_port_server_end).expect("failed to get port for endpoint");
            ep2.get_port(right_port_server_end).expect("failed to get port for endpoint");
            ep1.set_link_up(true).await.expect("failed to enable endpoint");
            ep2.set_link_up(true).await.expect("failed to enable endpoint");
            (Link::DebugNetwork { name, net, _ep1: ep1, _ep2: ep2 }, left_port, right_port)
        } else {
            let (tun_dev_pair, left_port, right_port) = devices::create_eth_tun_pair().await;
            (Link::Tun(tun_dev_pair), left_port, right_port)
        }
    }

    /// Starts a packet capture on the link named `{prefix}-{link name}`, if
    /// the link supports it.
    async fn start_capture(&mut self, prefix: &str) -> Option<PacketCapture> {
        match self {
            Link::Tun(_tun) => None,
            Link::DebugNetwork { name, net, _ep1, _ep2 } => {
                // We exclusively borrowed `net` so no one should be able to capture
                // packets at the moment.
                Some(
                    net.start_capture(&format!("{prefix}-{name}"))
                        .await
                        .expect("failed to start packet capture"),
                )
            }
        }
    }
}

async fn install_interface(
    realm: &netemul::TestRealm<'_>,
    port: fhardware_network::PortProxy,
    subnets: impl IntoIterator<Item = fnet::Subnet>,
) -> InterfaceFidlProxies {
    let device = {
        let (device, server_end) =
            fidl::endpoints::create_endpoints::<fhardware_network::DeviceMarker>();
        port.get_device(server_end).expect("get device");
        device
    };
    let device_control = devices::install_device(realm, device);

    let control = {
        let port_id =
            port.get_info().await.expect("get port info").id.expect("port ID must be present");
        let (control, server_end) =
            fnet_interfaces_ext::admin::Control::create_endpoints().expect("create endpoints");
        device_control
            .create_interface(&port_id, server_end, fnet_interfaces_admin::Options::default())
            .expect("create interface");
        control
    };
    assert!(control.enable().await.expect("enable interface FIDL call").expect("enable interface"));

    for subnet in subnets {
        let addr_state_provider = interfaces::add_address_wait_assigned(
            &control,
            subnet,
            fnet_interfaces_admin::AddressParameters {
                add_subnet_route: Some(true),
                ..Default::default()
            },
        )
        .await
        .expect("add address and wait for it to be assigned");
        addr_state_provider.detach().expect("address state provider detach FIDL call");
    }

    InterfaceFidlProxies { _device_control: device_control, control }
}

/// Sets up a client and server sharing a link:
///
///   client <-> server
///
/// Returns the client and server realms, in that order.
async fn setup<'a>(
    sandbox: &'a netemul::TestSandbox,
    netstack_version: NetstackVersion,
    capture_packets: bool,
) -> (netemul::TestRealm<'a>, netemul::TestRealm<'a>, FidlProxies<'a>) {
    let client_realm = sandbox
        .create_realm(
            "client",
            [KnownServiceProvider::SecureStash, KnownServiceProvider::Netstack(netstack_version)],
        )
        .expect("create client netstack");
    let server_realm = sandbox
        .create_realm(
            "server",
            [KnownServiceProvider::SecureStash, KnownServiceProvider::Netstack(netstack_version)],
        )
        .expect("create server netstack");

    let (link, client_port, server_port) = Link::new(sandbox, "direct", capture_packets).await;

    let (client_if, server_if) = futures::future::join(
        install_interface(&client_realm, client_port, [Ipv4::DIRECT.client, Ipv6::DIRECT.client]),
        install_interface(&server_realm, server_port, [Ipv4::DIRECT.server, Ipv6::DIRECT.server]),
    )
    .await;

    (
        client_realm,
        server_realm,
        FidlProxies { links: vec![link], _interfaces: vec![client_if, server_if] },
    )
}

/// Adds a default route via the address in `gateway` through the interface
/// controlled by `control` to the global route table in `realm`.
async fn add_default_route<I: IpExt>(
    realm: &netemul::TestRealm<'_>,
    control: &fnet_interfaces_ext::admin::Control,
    gateway: fnet::Subnet,
) {
    let gateway: I::Addr = I::map_ip_out(
        fnet_ext::IntoExt::<IpAddr>::into_ext(gateway.addr),
        |addr| match addr {
            IpAddr::V4(addr) => addr,
            IpAddr::V6(addr) => panic!("expected IPv4 gateway, got {addr}"),
        },
        |addr| match addr {
            IpAddr::V6(addr) => addr,
            IpAddr::V4(addr) => panic!("expected IPv6 gateway, got {addr}"),
        },
    );
    let root_routes = realm
        .connect_to_protocol::<I::GlobalRouteTableMarker>()
        .expect("connect to global route table");
    let route_set = fnet_routes_ext::admin::new_global_route_set::<I>(&root_routes)
        .expect("create global route set");
    let fnet_resources::GrantForInterfaceAuthorization { interface_id, token } =
        control.get_authorization_for_interface().await.expect("get interface authorization");
    fnet_routes_ext::admin::authenticate_for_interface::<I>(
        &route_set,
        fnet_resources::ProofOfInterfaceAuthorization { interface_id, token },
    )
    .await
    .expect("authenticate for interface FIDL call")
    .expect("authenticate for interface");
    let route = fnet_routes_ext::Route::<I>::new_forward_with_inherited_metric(
        net_types::ip::Subnet::new(I::UNSPECIFIED_ADDRESS, 0).expect("default subnet"),
        interface_id,
        Some(net_types::SpecifiedAddr::new(gateway).expect("gateway must be specified")),
    );
    assert!(
        fnet_routes_ext::admin::add_route::<I>(
            &route_set,
            &route.try_into().expect("convert route to FIDL"),
        )
        .await
        .expect("add route FIDL call")
        .expect("add route")
    );
}

/// Sets up a client and server on separate subnets connected via a router:
///
///   client <-> (client gateway) router (server gateway) <-> server
///
/// Returns the client, router, and server realms, in that order.
async fn setup_forwarding<'a>(
    sandbox: &'a netemul::TestSandbox,
    netstack_version: NetstackVersion,
    capture_packets: bool,
) -> (netemul::TestRealm<'a>, netemul::TestRealm<'a>, netemul::TestRealm<'a>, FidlProxies<'a>) {
    let create_realm = |name| {
        sandbox
            .create_realm(
                name,
                [
                    KnownServiceProvider::SecureStash,
                    KnownServiceProvider::Netstack(netstack_version),
                ],
            )
            .expect("create netstack realm")
    };
    let client_realm = create_realm("forward-client");
    let router_realm = create_realm("forward-router");
    let server_realm = create_realm("forward-server");

    let (client_link, client_port, client_gateway_port) =
        Link::new(sandbox, "client", capture_packets).await;
    let (server_link, server_gateway_port, server_port) =
        Link::new(sandbox, "server", capture_packets).await;

    let (client_if, client_gateway_if, server_gateway_if, server_if) = futures::future::join4(
        install_interface(
            &client_realm,
            client_port,
            [Ipv4::FORWARDING.client, Ipv6::FORWARDING.client],
        ),
        install_interface(
            &router_realm,
            client_gateway_port,
            [Ipv4::FORWARDING.client_gateway, Ipv6::FORWARDING.client_gateway],
        ),
        install_interface(
            &router_realm,
            server_gateway_port,
            [Ipv4::FORWARDING.server_gateway, Ipv6::FORWARDING.server_gateway],
        ),
        install_interface(
            &server_realm,
            server_port,
            [Ipv4::FORWARDING.server, Ipv6::FORWARDING.server],
        ),
    )
    .await;

    for router_if in [&client_gateway_if, &server_gateway_if] {
        let _: fnet_interfaces_admin::Configuration = router_if
            .control
            .set_configuration(&fnet_interfaces_admin::Configuration {
                ipv4: Some(fnet_interfaces_admin::Ipv4Configuration {
                    unicast_forwarding: Some(true),
                    ..Default::default()
                }),
                ipv6: Some(fnet_interfaces_admin::Ipv6Configuration {
                    unicast_forwarding: Some(true),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect("set configuration FIDL call")
            .expect("set configuration");
    }

    add_default_route::<Ipv4>(&client_realm, &client_if.control, Ipv4::FORWARDING.client_gateway)
        .await;
    add_default_route::<Ipv6>(&client_realm, &client_if.control, Ipv6::FORWARDING.client_gateway)
        .await;
    add_default_route::<Ipv4>(&server_realm, &server_if.control, Ipv4::FORWARDING.server_gateway)
        .await;
    add_default_route::<Ipv6>(&server_realm, &server_if.control, Ipv6::FORWARDING.server_gateway)
        .await;

    (
        client_realm,
        router_realm,
        server_realm,
        FidlProxies {
            links: vec![client_link, server_link],
            _interfaces: vec![client_if, client_gateway_if, server_gateway_if, server_if],
        },
    )
}

fn format_byte_count(byte_count: usize) -> String {
    if byte_count >= 1024 {
        format!("{}KiB", byte_count / 1024)
    } else {
        format!("{}B", byte_count)
    }
}

/// Connects a TCP socket in `client_realm` to a listener bound to
/// `server_addr` in `server_realm`, sizing the socket buffers for `transfer`
/// bytes.
async fn connect_tcp<'a, I: IpExt>(
    client_realm: &netemul::TestRealm<'a>,
    server_realm: &netemul::TestRealm<'a>,
    server_addr: fnet::IpAddress,
    transfer: usize,
) -> (socket2::Socket, socket2::Socket) {
    let (listen_sock, client_sock) = futures::future::join(
        server_realm
            .stream_socket(I::DOMAIN, fposix_socket::StreamSocketProtocol::Tcp)
            .map(|r| r.expect("create listening socket")),
        client_realm
            .stream_socket(I::DOMAIN, fposix_socket::StreamSocketProtocol::Tcp)
            .map(|r| r.expect("create client socket")),
    )
    .await;

    // Since we want to avoid including the overhead of the async
    // executor in the benchmarked read/write steps, intentionally keep
    // the sockets non-async-aware and use `socket2` calls directly to
    // connect the sockets.
    let bind_sockaddr = {
        let fnet_ext::IpAddress(listen_addr) = server_addr.into();
        socket2::SockAddr::from(std::net::SocketAddr::from((listen_addr, 0)))
    };
    listen_sock.bind(&bind_sockaddr).expect("bind");
    listen_sock.listen(0).expect("listen");
    let listen_sockaddr = listen_sock.local_addr().expect("local addr");

    const BUFFER_SIZE_MULTIPLER: usize = 4;

    // Set send buffer to a multiple of the transfer size to ensure we can
    // write `transfer` bytes before reading it on the other end. The
    // multiplier allows the receiver to delay acknowledgements but the
    // transfer still proceeds. This is especially impactful for small
    // transfer sizes.
    client_sock
        .set_send_buffer_size(transfer * BUFFER_SIZE_MULTIPLER)
        .expect("set send buffer size");

    // Also set the receive buffer size.
    //
    // We use the same multiplier on the transfer size so silly window
    // avoidance doesn't kick in in-between test iterations which causes
    // pollution in the results.
    //
    // This ensures fairness in the benchmark since TCP will base the window
    // value on the available receive buffer size and different numbers will
    // skew the test results.
    listen_sock
        .set_recv_buffer_size(transfer * BUFFER_SIZE_MULTIPLER)
        .expect("set receive buffer size");

    // Disable the Nagle algorithm, it introduces artificial
    // latency that defeats this benchmark.
    client_sock.set_tcp_nodelay(true).expect("set TCP NODELAY to true");
    client_sock.connect(&listen_sockaddr).expect("connect");

    let (server_sock, _): (_, socket2::SockAddr) = listen_sock.accept().expect("accept");

    (client_sock, server_sock)
}

/// Writes all of `buf` to `sock`.
fn write_all(mut sock: &socket2::Socket, buf: &[u8]) {
    let mut transferred = 0;
    while transferred < buf.len() {
        fuchsia_trace::duration_begin!("tun_socket_benchmarks", "tcp_write");
        let wrote = sock.write(&buf[transferred..]).expect("write failed");
        fuchsia_trace::duration_end!(
            "tun_socket_benchmarks", "tcp_write",
            "bytes_written" => wrote as u64
        );
        transferred += wrote;
    }
}

/// Fills `buf` from `sock`.
fn read_exact(mut sock: &socket2::Socket, buf: &mut [u8]) {
    let mut transferred = 0;
    while transferred < buf.len() {
        fuchsia_trace::duration_begin!("tun_socket_benchmarks", "tcp_read");
        let read = sock.read(&mut buf[transferred..]).expect("read failed");
        fuchsia_trace::duration_end!(
            "tun_socket_benchmarks", "tcp_read",
            "bytes_read" => read as u64
        );
        transferred += read;
    }
}

/// Benchmarks transferring `transfer` bytes from `client_realm` to
/// `server_addr` in `server_realm`, split evenly across `flows` concurrent TCP
/// connections.
///
/// Each iteration writes each connection's share of the transfer before
/// reading any of it on the receiving end. With multiple connections, the
/// shares are written concurrently from separate threads.
async fn bench_tcp<'a, I: IpExt>(
    test_suite: &'static str,
    iter_count: usize,
    name: &'static str,
    client_realm: &netemul::TestRealm<'a>,
    server_realm: &netemul::TestRealm<'a>,
    server_addr: fnet::IpAddress,
    transfer: usize,
    flows: usize,
    links: &mut [Link<'a>],
) -> fuchsiaperf::FuchsiaPerfBenchmarkResult {
    assert_eq!(transfer % flows, 0, "transfer must divide evenly across flows");
    let flow_transfer = transfer / flows;

    let label = if flows > 1 {
        format!("{}/TCP/{}/{}/{}Flows", name, I::NAME, format_byte_count(transfer), flows)
    } else {
        format!("{}/TCP/{}/{}", name, I::NAME, format_byte_count(transfer))
    };
    let capture_prefix = label.replace("/", "-");
    let _packet_captures: Vec<Option<PacketCapture>> =
        futures::future::join_all(links.iter_mut().map(|link| link.start_capture(&capture_prefix)))
            .await;
    let mut sockets = Vec::with_capacity(flows);
    for _ in 0..flows {
        sockets
            .push(connect_tcp::<I>(client_realm, server_realm, server_addr, flow_transfer).await);
    }

    fuchsia_trace::duration!("tun_socket_benchmarks", "test_group", "label" => &*label);
    let (client_socks, server_socks): (Vec<_>, Vec<_>) = sockets.into_iter().unzip();
    // Every flow sends the same bytes, so they share a send buffer.
    let (send_buf, recv_buf) = generate_send_recv_bufs(flow_transfer);
    let send_buf = send_buf.as_slice();
    // With multiple flows, each flow is written from its own thread so that
    // the flows are written concurrently and their segments can be interleaved
    // within a receive batch. The threads are spawned up front and synchronized
    // with barriers to keep thread creation out of the measured duration.
    //
    // A single flow is written inline to avoid the synchronization overhead.
    let barriers = (flows > 1).then(|| {
        let start = std::sync::Barrier::new(flows + 1);
        let done = std::sync::Barrier::new(flows + 1);
        (start, done)
    });
    let values = std::thread::scope(|scope| {
        if let Some((start, done)) = &barriers {
            for client_sock in &client_socks {
                let _: std::thread::ScopedJoinHandle<'_, ()> = scope.spawn(move || {
                    for _ in 0..iter_count {
                        let _: std::sync::BarrierWaitResult = start.wait();
                        write_all(client_sock, send_buf);
                        let _: std::sync::BarrierWaitResult = done.wait();
                    }
                });
            }
        }

        (0..iter_count)
            .map(|_| {
                let mut recv_bufs = vec![recv_buf.clone(); flows];

                fuchsia_trace::duration!("tun_socket_benchmarks", "test_case");
                let now = std::time::Instant::now();
                match &barriers {
                    Some((start, done)) => {
                        let _: std::sync::BarrierWaitResult = start.wait();
                        let _: std::sync::BarrierWaitResult = done.wait();
                    }
                    None => {
                        for client_sock in &client_socks {
                            write_all(client_sock, send_buf);
                        }
                    }
                }
                for (server_sock, recv_buf) in server_socks.iter().zip(recv_bufs.iter_mut()) {
                    read_exact(server_sock, recv_buf);
                }
                let duration = now.elapsed().as_nanos() as f64;
                for recv_buf in &recv_bufs {
                    assert_eq!(recv_buf, send_buf);
                }
                duration
            })
            .collect()
    });

    fuchsiaperf::FuchsiaPerfBenchmarkResult {
        test_suite: test_suite.into(),
        label: label.clone(),
        unit: Unit::Nanoseconds,
        direction: Direction::SmallerBetter,
        values,
    }
}

async fn bench_udp<'a, I: IpExt>(
    test_suite: &'static str,
    iter_count: usize,
    client_realm: &netemul::TestRealm<'a>,
    server_realm: &netemul::TestRealm<'a>,
    message_size: usize,
    message_count: usize,
) -> fuchsiaperf::FuchsiaPerfBenchmarkResult {
    let label = if message_count > 1 {
        format!(
            "MultiWriteRead/UDP/{}/{}/{}Messages",
            I::NAME,
            format_byte_count(message_size),
            message_count
        )
    } else {
        format!("WriteRead/UDP/{}/{}", I::NAME, format_byte_count(message_size))
    };

    let (mut client_sock, mut server_sock) = {
        let (server_sock, client_sock) = futures::future::join(
            server_realm
                .datagram_socket(I::DOMAIN, fposix_socket::DatagramSocketProtocol::Udp)
                .map(|r| r.expect("create server socket")),
            client_realm
                .datagram_socket(I::DOMAIN, fposix_socket::DatagramSocketProtocol::Udp)
                .map(|r| r.expect("create client socket")),
        )
        .await;

        // Since we want to avoid including the async executor from the benchmark,
        // here we "manually" use non-blocking calls to connect sockets instead of
        // using the normal async facilities.
        let bind_sockaddr = {
            let fnet_ext::IpAddress(listen_addr) = I::DIRECT.server.addr.into();
            socket2::SockAddr::from(std::net::SocketAddr::from((listen_addr, 0)))
        };
        server_sock.bind(&bind_sockaddr).expect("bind");
        let server_sockaddr = server_sock.local_addr().expect("local addr");

        let transfer_size = message_size * message_count;

        // Set receive buffer large enough to hold the entire write so it can
        // complete before reading. Netstacks charge a per-message bookkeeping
        // overhead against the receive buffer in addition to the payload, so
        // leave room for it on top of the payload bytes.
        const PER_MESSAGE_RCVBUF_OVERHEAD: usize = 256;
        server_sock
            .set_recv_buffer_size((message_size + PER_MESSAGE_RCVBUF_OVERHEAD) * message_count)
            .expect("set receive buffer size");

        // Also set the send buffer size to the transfer value.
        //
        // This ensures fairness in the benchmark when comparing platforms so
        // we're not relying on default value differences.
        client_sock
            .set_send_buffer_size(transfer_size)
            .expect("set send buffer size to transfer size");

        client_sock.connect(&server_sockaddr).expect("connect");

        (client_sock, server_sock)
    };

    fuchsia_trace::duration!("tun_socket_benchmarks", "test_group", "label" => &*label);
    let values = (0..iter_count)
        .map(|_| {
            // Allocate buffers that can hold the total transfer size so that the
            // assertion that the bytes transferred are as expected can be done after
            // the entire transfer has completed and excluded from the benchmark
            // duration.
            let (send_buf, mut recv_buf) = generate_send_recv_bufs(message_count * message_size);

            fuchsia_trace::duration!("tun_socket_benchmarks", "test_case");
            let now = std::time::Instant::now();
            for message_bytes in send_buf.chunks(message_size) {
                let wrote = {
                    fuchsia_trace::duration!("tun_socket_benchmarks", "udp_write");
                    client_sock.write(message_bytes.as_ref()).expect("write failed")
                };
                assert_eq!(wrote, message_size);
            }
            for recv_bytes in recv_buf.chunks_mut(message_size) {
                let read = {
                    fuchsia_trace::duration!("tun_socket_benchmarks", "udp_read");
                    server_sock.read(recv_bytes.as_mut()).expect("read failed")
                };
                assert_eq!(read, message_size);
            }
            let duration = now.elapsed().as_nanos() as f64;
            assert_eq!(send_buf, recv_buf);
            duration
        })
        .collect();

    fuchsiaperf::FuchsiaPerfBenchmarkResult {
        test_suite: test_suite.into(),
        label: label.clone(),
        unit: Unit::Nanoseconds,
        direction: Direction::SmallerBetter,
        values,
    }
}

#[derive(argh::FromArgs)]
/// E2E socket benchmark arguments.
struct Args {
    /// run in perf mode and write metrics in fuchsiaperf JSON format to
    /// the provided path
    #[argh(option)]
    output_fuchsiaperf: Option<std::path::PathBuf>,

    /// run with trace events enabled
    #[argh(switch)]
    tracing: bool,

    /// produce packet capture
    #[argh(switch)]
    pcap: bool,
}

#[fuchsia::main]
async fn main() {
    let Args { output_fuchsiaperf, tracing, pcap } = argh::from_env();
    let iter_count = if output_fuchsiaperf.is_some() {
        1000
    } else {
        // This iteration count is used both for the test variants to keep
        // run time low, and for the trace variants to ensure that the trace
        // file generated aren't too large and result in truncated trace
        // events.
        3
    };
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");

    let test_suite = "fuchsia.network.socket.tun.netstack3";
    let (client_realm, server_realm, mut fidl_proxies) =
        setup(&sandbox, NetstackVersion::ProdNetstack3, pcap).await;
    let (
        forward_client_realm,
        _forward_router_realm,
        forward_server_realm,
        mut forward_fidl_proxies,
    ) = setup_forwarding(&sandbox, NetstackVersion::ProdNetstack3, pcap).await;

    let tracer = if tracing {
        // TODO(https://fxbug.dev/42096938): Use race-free trace provider
        // initialization when available.
        fuchsia_trace_provider::trace_provider_create_with_fdio();

        let file = std::fs::File::create("/custom_artifacts/trace.fxt").expect("create trace file");
        let trace_provisioner = fuchsia_component::client::connect_to_protocol::<
            ftracing_controller::ProvisionerMarker,
        >()
        .expect("connect to tracing controller");
        let (tracing_socket, tracing_socket_write) = fidl::Socket::create_stream();
        let (trace_session, server) =
            fidl::endpoints::create_proxy::<ftracing_controller::SessionMarker>();
        trace_provisioner
            .initialize_tracing(
                server,
                &ftracing_controller::TraceConfig {
                    categories: Some(
                        [
                            "kernel:meta",
                            "kernel:sched",
                            "kernel:syscall",
                            "net",
                            "tun_socket_benchmarks",
                        ]
                        .into_iter()
                        .map(ToString::to_string)
                        .collect(),
                    ),
                    // Since oneshot mode is used, set the buffer size as large
                    // as possible so that trace events don't get dropped.
                    buffer_size_megabytes_hint: Some(64),
                    ..Default::default()
                },
                tracing_socket_write,
            )
            .expect("initialize tracing FIDL");
        trace_session
            .start_tracing(&ftracing_controller::StartOptions::default())
            .await
            .expect("starting tracing FIDL")
            .expect("start tracing");
        Some((trace_session, fasync::Socket::from_socket(tracing_socket), file))
    } else {
        None
    };

    let metrics = {
        let mut metrics = Vec::new();
        for transfer in [1 << 10, 10 << 10, 100 << 10, 500 << 10, 1000 << 10] {
            metrics.push(
                bench_tcp::<Ipv4>(
                    test_suite,
                    iter_count,
                    "WriteRead",
                    &client_realm,
                    &server_realm,
                    Ipv4::DIRECT.server.addr,
                    transfer,
                    1,
                    &mut fidl_proxies.links,
                )
                .await,
            );
            metrics.push(
                bench_tcp::<Ipv6>(
                    test_suite,
                    iter_count,
                    "WriteRead",
                    &client_realm,
                    &server_realm,
                    Ipv6::DIRECT.server.addr,
                    transfer,
                    1,
                    &mut fidl_proxies.links,
                )
                .await,
            );
        }

        // NB: The transfer size here is the total across all flows, so that
        // results are comparable with the single-flow results of the same
        // transfer size. Small transfer sizes are omitted since splitting them
        // across flows leaves too little data per flow to meaningfully
        // interleave.
        for transfer in [100 << 10, 1000 << 10] {
            for flows in [2, 4, 8] {
                metrics.push(
                    bench_tcp::<Ipv4>(
                        test_suite,
                        iter_count,
                        "WriteRead",
                        &client_realm,
                        &server_realm,
                        Ipv4::DIRECT.server.addr,
                        transfer,
                        flows,
                        &mut fidl_proxies.links,
                    )
                    .await,
                );
                metrics.push(
                    bench_tcp::<Ipv6>(
                        test_suite,
                        iter_count,
                        "WriteRead",
                        &client_realm,
                        &server_realm,
                        Ipv6::DIRECT.server.addr,
                        transfer,
                        flows,
                        &mut fidl_proxies.links,
                    )
                    .await,
                );
            }
        }

        for transfer in [1 << 10, 10 << 10, 100 << 10, 500 << 10, 1000 << 10] {
            metrics.push(
                bench_tcp::<Ipv4>(
                    test_suite,
                    iter_count,
                    "Forward",
                    &forward_client_realm,
                    &forward_server_realm,
                    Ipv4::FORWARDING.server.addr,
                    transfer,
                    1,
                    &mut forward_fidl_proxies.links,
                )
                .await,
            );
            metrics.push(
                bench_tcp::<Ipv6>(
                    test_suite,
                    iter_count,
                    "Forward",
                    &forward_client_realm,
                    &forward_server_realm,
                    Ipv6::FORWARDING.server.addr,
                    transfer,
                    1,
                    &mut forward_fidl_proxies.links,
                )
                .await,
            );
        }

        // NB: All of these message sizes are kept below the MTU of 1500 bytes
        // so that fragmentation is not needed (NS3 doesn't currently support
        // fragmentation c.f. https://fxbug.dev/42079055).
        for message_size in [1, 100, 1 << 10] {
            // NB: The 1, 10, and 50 message counts match those in the loopback socket
            // benchmark to facilitate comparison of results, and 250 is as large as
            // reasonably possible to stay within Infra runtime limits.
            for message_count in [1, 10, 50, 250] {
                metrics.push(
                    bench_udp::<net_types::ip::Ipv4>(
                        test_suite,
                        iter_count,
                        &client_realm,
                        &server_realm,
                        message_size,
                        message_count,
                    )
                    .await,
                );
                metrics.push(
                    bench_udp::<net_types::ip::Ipv6>(
                        test_suite,
                        iter_count,
                        &client_realm,
                        &server_realm,
                        message_size,
                        message_count,
                    )
                    .await,
                );
            }
        }
        metrics
    };

    if let Some((trace_session, mut tracing_socket, mut file)) = tracer {
        let mut trace = Vec::new();
        let stop_and_terminate_fut = async move {
            let res = trace_session
                .stop_tracing(&ftracing_controller::StopOptions {
                    write_results: Some(true),
                    ..Default::default()
                })
                .await
                .map(|r| r.expect("stop tracing"));
            res.expect("stop tracing")
        };
        // NB: Terminating tracing is essentially infallible right now, since
        // StopResult is not a result, but a FIDL table with no fields in it,
        // thus it's safe to ignore.
        let (_, _): (ftracing_controller::StopResult, usize) = futures::future::join(
            stop_and_terminate_fut,
            tracing_socket.read_to_end(&mut trace).map(|r| r.expect("read trace")),
        )
        .await;

        file.write_all(trace.as_ref()).expect("write trace to file");
    }

    if let Some(output_fuchsiaperf) = output_fuchsiaperf {
        let metrics_json =
            serde_json::to_string_pretty(&metrics).expect("serialize metrics as JSON");
        std::fs::write(output_fuchsiaperf, metrics_json).expect("write metrics as custom artifact");
    }
}
