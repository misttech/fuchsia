// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use fidl_fuchsia_posix_socket as fposix_socket;
use fidl_fuchsia_posix_socket_raw as fposix_socket_raw;
use fuchsia_async::TimeoutExt as _;
use fuchsia_async::net::DatagramSocket;
use futures::FutureExt as _;
use net_types::ip::IpVersion;
use netstack_testing_common::realms::{Netstack, TestSandboxExt as _};
use netstack_testing_common::{
    ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT, ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT,
};
use netstack_testing_macros::netstack_test;
use packet::ParsablePacket as _;
use packet_formats::ip::IpProto;
use packet_formats::ipv4::Ipv4Packet;
use socket2::InterfaceIndexOrAddress;
use test_case::test_case;

use crate::MulticastTestIpExt;

#[netstack_test]
#[variant(N, Netstack)]
#[variant(I, Ip)]
#[test_case(None, true; "default_should_loop")]
#[test_case(Some(true), true; "enabled_should_loop")]
#[test_case(Some(false), false; "disabled_shouldnt_loop")]
async fn multicast_loop_on_raw_ip_socket<N: Netstack, I: MulticastTestIpExt>(
    name: &str,
    multicast_loop_value: Option<bool>,
    should_receive: bool,
) {
    let sandbox = netemul::TestSandbox::new().expect("failed to create sandbox");
    let client = sandbox
        .create_netstack_realm::<N, _>(format!("{name}_client"))
        .expect("failed to create client realm");
    let networks = crate::init_multicast_test_networks::<I>(&sandbox, &client).await;

    // NB: Ensure we send the packet over a non-loopback interface, as that
    // would defeat the purpose of the multicast_loop test.
    let iface = &networks[0].iface;

    let send_socket = client
        .raw_socket(
            I::DOMAIN,
            fposix_socket_raw::ProtocolAssociation::Associated(IpProto::Udp.into()),
        )
        .await
        .expect("failed to create socket");
    send_socket
        .bind_device(Some(
            iface.get_interface_name().await.expect("get_interface_name failed").as_bytes(),
        ))
        .expect("failed to bind socket to an interface");

    if let Some(multicast_loop) = multicast_loop_value {
        match I::VERSION {
            IpVersion::V4 => send_socket.set_multicast_loop_v4(multicast_loop),
            IpVersion::V6 => send_socket.set_multicast_loop_v6(multicast_loop),
        }
        .expect("failed to set IPV6_MULTICAST_LOOP");
    }

    let recv_socket = client
        .raw_socket(
            I::DOMAIN,
            fposix_socket_raw::ProtocolAssociation::Associated(IpProto::Udp.into()),
        )
        .await
        .expect("failed to create socket");
    let recv_socket = DatagramSocket::new_from_socket(recv_socket).unwrap();

    // NB: Multicast traffic is dropped before being delivered to raw IP sockets
    // if we don't have any interest in the packet. Register a UDP socket
    // with interest.
    let _multicast_interested_sock = {
        let socket = client
            .datagram_socket(I::DOMAIN, fposix_socket::DatagramSocketProtocol::Udp)
            .await
            .expect("failed to create socket");
        let iface_id = u32::try_from(iface.id()).unwrap();
        match I::MCAST_ADDR.ip() {
            std::net::IpAddr::V4(addr_v4) => socket
                .join_multicast_v4_n(&addr_v4.into(), &InterfaceIndexOrAddress::Index(iface_id))
                .expect("failed to join multicast group"),
            std::net::IpAddr::V6(addr_v6) => socket
                .join_multicast_v6(&addr_v6.into(), iface_id)
                .expect("failed to join multicast group"),
        }
        socket
    };

    let data = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    assert_eq!(
        send_socket.send_to(&data, &I::MCAST_ADDR.into()).expect("failed to send multicast packet"),
        data.len()
    );

    let mut buf = [0u8; 200];
    let recv_fut = recv_socket.recv_from(&mut buf);
    if should_receive {
        let (size, _addr) = recv_fut
            .on_timeout(ASYNC_EVENT_POSITIVE_CHECK_TIMEOUT, || {
                Err(std::io::ErrorKind::TimedOut.into())
            })
            .await
            .expect("recv_from failed");
        match I::VERSION {
            // NB: Raw IPv4 Sockets receive the full IP Header
            IpVersion::V4 => {
                let buffer = packet::Buf::new(buf, 0..size);
                let packet =
                    Ipv4Packet::parse(&mut buffer.as_ref(), ()).expect("parse should succeed");
                assert_eq!(packet.body(), &data[..]);
            }
            IpVersion::V6 => assert_eq!(&buf[..size], &data[..]),
        }
    } else {
        recv_fut
            .map(|output| panic!("unexpected received packet {output:?}"))
            .on_timeout(ASYNC_EVENT_NEGATIVE_CHECK_TIMEOUT, || ())
            .await;
    }
}
