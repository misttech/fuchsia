// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use std::os::fd::AsFd;

use assert_matches::assert_matches;
use fidl::Error::ClientChannelClosed;
use fidl_fuchsia_net_ext::IntoExt as _;
use fidl_fuchsia_net_sockets::IpSocketState;
use net_declare::std_ip_v6;
use net_types::ip::{Ip, IpAddress, IpVersion};
use netemul::RealmUdpSocket as _;
use netstack_testing_common::realms::{Netstack3, TestSandboxExt as _};
use netstack_testing_macros::netstack_test;
use test_case::test_case;
use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_matchers as fnet_matchers,
    fidl_fuchsia_net_matchers_ext as fnet_matchers_ext, fidl_fuchsia_net_sockets as fnet_sockets,
    fidl_fuchsia_net_sockets_ext as fnet_sockets_ext, fidl_fuchsia_net_tcp as fnet_tcp,
    fidl_fuchsia_net_udp as fnet_udp, fidl_fuchsia_posix_socket as fposix_socket,
    fuchsia_async as fasync,
};

const MARK_1: u32 = 100;
const MARK_2: u32 = 200;
const LOCAL_PORT: u16 = 1234;
const IPV6_LOOPBACK: std::net::Ipv6Addr = std_ip_v6!("::1");

async fn socket_proxy<F: AsFd>(socket: &F) -> fposix_socket::BaseSocketProxy {
    let channel = fdio::clone_channel(socket).expect("failed to clone channel");
    fposix_socket::BaseSocketProxy::new(fidl::AsyncChannel::from_channel(channel))
}

async fn set_marks(proxy: &fposix_socket::BaseSocketProxy) {
    proxy
        .set_mark(fnet::MarkDomain::Mark1, &fposix_socket::OptionalUint32::Value(MARK_1))
        .await
        .expect("fidl error")
        .expect("set mark");
    proxy
        .set_mark(fnet::MarkDomain::Mark2, &fposix_socket::OptionalUint32::Value(MARK_2))
        .await
        .expect("fidl error")
        .expect("set mark");
}

#[netstack_test]
async fn no_results_when_no_sockets(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");
    let diagnostics = realm
        .connect_to_protocol::<fnet_sockets::DiagnosticsMarker>()
        .expect("connect to protocol");

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    assert_matches!(
        diagnostics
            .iterate_ip(
                server_end,
                fnet_sockets::Extensions::empty(),
                &[fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V4)]
            )
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty)
    );

    let (sockets, has_more) = proxy.next().await.unwrap();
    assert!(sockets.is_empty());
    assert!(!has_more);
    assert_matches!(proxy.next().await, Err(ClientChannelClosed { .. }));
}

#[netstack_test]
async fn invalid_matcher(name: &str) {
    let good_matcher = fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V4);
    let invalid_matcher = fnet_sockets::IpSocketMatcher::BoundInterface(
        fnet_matchers::BoundInterface::Bound(fnet_matchers::Interface::Id(0)),
    );

    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");
    let diagnostics = realm
        .connect_to_protocol::<fnet_sockets::DiagnosticsMarker>()
        .expect("connect to protocol");

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    assert_matches!(
        diagnostics
            .iterate_ip(
                server_end,
                fnet_sockets::Extensions::empty(),
                &[invalid_matcher.clone(), good_matcher.clone(), good_matcher.clone(),]
            )
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::MatcherError(fnet_sockets::IterateIpMatcherError {
            index: Some(0),
            ..
        })
    );
    assert_matches!(proxy.next().await, Err(ClientChannelClosed { .. }));

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    assert_matches!(
        diagnostics
            .iterate_ip(
                server_end,
                fnet_sockets::Extensions::empty(),
                &[good_matcher.clone(), good_matcher, invalid_matcher,]
            )
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::MatcherError(fnet_sockets::IterateIpMatcherError {
            index: Some(2),
            ..
        })
    );
    assert_matches!(proxy.next().await, Err(ClientChannelClosed { .. }));
}

#[derive(Debug, Copy, Clone)]
enum Protocol {
    Udp,
    Tcp,
}

#[derive(Debug, Copy, Clone)]
enum SocketState {
    Listen,
    Connected,
}

#[netstack_test]
#[variant(I, Ip)]
#[test_case(Protocol::Udp, SocketState::Listen)]
#[test_case(Protocol::Udp, SocketState::Connected)]
#[test_case(Protocol::Tcp, SocketState::Listen)]
#[test_case(Protocol::Tcp, SocketState::Connected)]
async fn test_sockets<I: Ip>(name: &str, protocol: Protocol, state: SocketState) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");

    let local_addr = std::net::SocketAddr::new(I::LOOPBACK_ADDRESS.to_ip_addr().into(), LOCAL_PORT);
    let domain = match I::VERSION {
        IpVersion::V4 => fposix_socket::Domain::Ipv4,
        IpVersion::V6 => fposix_socket::Domain::Ipv6,
    };

    let socket = match protocol {
        Protocol::Udp => {
            let socket = realm
                .datagram_socket(domain, fposix_socket::DatagramSocketProtocol::Udp)
                .await
                .unwrap();
            socket.bind(&local_addr.into()).unwrap();
            if let SocketState::Connected = state {
                socket.connect(&local_addr.into()).unwrap();
            }
            socket_proxy(&socket).await
        }
        Protocol::Tcp => {
            let socket = realm
                .stream_socket(domain, fposix_socket::StreamSocketProtocol::Tcp)
                .await
                .unwrap();
            socket.bind(&local_addr.into()).unwrap();
            match state {
                SocketState::Listen => socket.listen(1).unwrap(),
                // NOTE: This is a self-connected socket so we can avoid
                // creating other TCP sockets that we'd just have to filter
                // out.
                SocketState::Connected => socket.connect(&local_addr.into()).expect("connect"),
            }
            socket_proxy(&socket).await
        }
    };

    set_marks(&socket).await;

    let diagnostics = realm
        .connect_to_protocol::<fnet_sockets::DiagnosticsMarker>()
        .expect("connect to protocol");

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    let matchers = [
        fnet_sockets_ext::IpSocketMatcher::Family(I::VERSION),
        fnet_sockets_ext::IpSocketMatcher::Proto(match protocol {
            Protocol::Udp => {
                fnet_matchers_ext::SocketTransportProtocol::Udp(fnet_matchers_ext::UdpSocket::Empty)
            }
            Protocol::Tcp => {
                fnet_matchers_ext::SocketTransportProtocol::Tcp(fnet_matchers_ext::TcpSocket::Empty)
            }
        }),
    ]
    .into_iter()
    .map(Into::into)
    .collect::<Vec<_>>();
    assert_matches!(
        diagnostics
            .iterate_ip(server_end, fnet_sockets::Extensions::empty(), &matchers)
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::Ok(_)
    );

    let expected_src_addr = I::LOOPBACK_ADDRESS.to_ip_addr().into_ext();
    let expected_cookie = socket.get_cookie().await.expect("failed to get cookie").unwrap();
    let (expected_dst_addr, expected_dst_port) = match state {
        SocketState::Listen => (None, None),
        SocketState::Connected => (Some(expected_src_addr), Some(LOCAL_PORT)),
    };

    let expected_transport = match protocol {
        Protocol::Udp => {
            fnet_sockets::IpSocketTransportState::Udp(fnet_sockets::IpSocketUdpState {
                src_port: Some(LOCAL_PORT),
                dst_port: expected_dst_port,
                state: Some(match state {
                    SocketState::Listen => fnet_udp::State::Bound,
                    SocketState::Connected => fnet_udp::State::Connected,
                }),
                __source_breaking: fidl::marker::SourceBreaking,
            })
        }
        Protocol::Tcp => {
            fnet_sockets::IpSocketTransportState::Tcp(fnet_sockets::IpSocketTcpState {
                src_port: Some(LOCAL_PORT),
                dst_port: expected_dst_port,
                state: Some(match state {
                    SocketState::Listen => fnet_tcp::State::Listen,
                    SocketState::Connected => fnet_tcp::State::Established,
                }),
                tcp_info: None,
                __source_breaking: fidl::marker::SourceBreaking,
            })
        }
    };

    let expected_socket = fnet_sockets::IpSocketState {
        family: Some(I::VERSION.into_ext()),
        src_addr: Some(expected_src_addr),
        dst_addr: expected_dst_addr,
        cookie: Some(expected_cookie),
        marks: Some(fnet::Marks {
            mark_1: Some(MARK_1),
            mark_2: Some(MARK_2),
            __source_breaking: fidl::marker::SourceBreaking,
        }),
        transport: Some(expected_transport),
        __source_breaking: fidl::marker::SourceBreaking,
    };

    let (batch, has_more) = proxy.next().await.unwrap();
    assert!(!has_more);
    assert_eq!(batch, vec![expected_socket]);
}

#[netstack_test]
async fn paginated_results(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");

    const BASE_PORT: u16 = 20000;
    let mut sockets = Vec::new();
    let mut expected_ports = std::collections::HashSet::new();
    for i in 0..(fnet_sockets::MAX_IP_SOCKET_BATCH_SIZE + 1) {
        let port = BASE_PORT + (i as u16);
        let socket = fasync::net::UdpSocket::bind_in_realm(
            &realm,
            std::net::SocketAddr::from((IPV6_LOOPBACK, port)),
        )
        .await
        .expect("failed to create socket");
        sockets.push(socket);
        assert!(expected_ports.insert(port));
    }

    let diagnostics = realm
        .connect_to_protocol::<fnet_sockets::DiagnosticsMarker>()
        .expect("connect to protocol");

    let (proxy, server_end) = fidl::endpoints::create_proxy::<fnet_sockets::IpIteratorMarker>();
    assert_matches!(
        diagnostics
            .iterate_ip(server_end, fnet_sockets::Extensions::empty(), &[])
            .await
            .expect("failed to call fidl"),
        fnet_sockets::IterateIpResult::Ok(fnet_sockets::Empty)
    );

    let mut observed_ports = std::collections::HashSet::new();
    let mut process_batch = |batch: &[IpSocketState]| {
        for socket in batch {
            let src_port = assert_matches!(
                &socket.transport,
                Some(fnet_sockets::IpSocketTransportState::Udp(udp)) => {
                    assert_matches!(udp.src_port, Some(port) => port)
                }
            );
            assert!(observed_ports.insert(src_port), "duplicate port {} returned", src_port);
        }
    };

    let (batch, has_more) = proxy.next().await.expect("failed to get first batch");
    assert_eq!(batch.len(), fnet_sockets::MAX_IP_SOCKET_BATCH_SIZE as usize);
    assert!(has_more);
    process_batch(&batch);

    let (batch, has_more) = proxy.next().await.expect("failed to get second batch");
    assert_eq!(batch.len(), 1);
    assert!(!has_more);
    process_batch(&batch);

    assert_eq!(observed_ports, expected_ports);

    assert_matches!(proxy.next().await, Err(ClientChannelClosed { .. }));
}
