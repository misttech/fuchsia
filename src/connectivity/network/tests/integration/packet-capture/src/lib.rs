// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![cfg(test)]

use fidl_fuchsia_net as _;
use fidl_fuchsia_net_debug as fnet_debug;
use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;
use fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext;

use fuchsia_async as fasync;
use net_declare::{fidl_subnet, net_ip_v4, std_ip_v4};
use netemul::RealmUdpSocket as _;
use netstack_testing_common::realms::{Netstack3, TestRealmExt as _, TestSandboxExt as _};
use netstack_testing_macros::netstack_test;
use packet::ParsablePacket as _;

async fn send_and_recv_udp(
    realm: &netemul::TestRealm<'_>,
    bind_addr: std::net::SocketAddr,
    payload: &[u8],
) {
    let sock =
        fasync::net::UdpSocket::bind_in_realm(realm, bind_addr).await.expect("create socket");
    let sent = sock.send_to(payload, bind_addr).await.expect("send_to failed");
    assert_eq!(sent, payload.len());

    let mut recv_buf = vec![0u8; payload.len()];
    let (received, from_addr) = sock.recv_from(&mut recv_buf).await.expect("recv_from failed");
    assert_eq!(received, payload.len());
    assert_eq!(recv_buf.as_slice(), payload);
    assert_eq!(from_addr, bind_addr);
}

fn assert_single_udp_packet<'a>(
    packets_iter: pcap::PcapNgPacketIter<'a>,
    expected_ip: net_types::ip::Ipv4Addr,
    expected_port: u16,
    expected_payload: &[u8],
) {
    let packets = packets_iter.collect::<Result<Vec<_>, _>>().expect("EPB parse error");
    let [epb] = &packets[..] else {
        panic!("expected exactly one packet in the capture, got {:?}", packets);
    };
    assert_eq!(epb.interface_id, 0);
    assert_eq!(usize::try_from(epb.original_length).unwrap(), epb.packet_data.len());
    assert_eq!(usize::try_from(epb.captured_length).unwrap(), epb.packet_data.len());
    let buf = &epb.packet_data;
    let (mut body, _src_mac, _dst_mac, src_ip, dst_ip, proto, _ttl) =
        packet_formats::testutil::parse_ip_packet_in_ethernet_frame::<net_types::ip::Ipv4>(
            &buf,
            packet_formats::ethernet::EthernetFrameLengthCheck::NoCheck,
        )
        .expect("failed to parse IP packet");

    assert_eq!(proto, packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Udp));
    let udp = packet_formats::udp::UdpPacket::parse(
        &mut body,
        packet_formats::udp::UdpParseArgs::new(src_ip, dst_ip),
    )
    .expect("failed to parse UDP packet");

    assert_eq!(udp.src_port().map(|p| p.get()), Some(expected_port));
    assert_eq!(udp.dst_port().get(), expected_port);
    assert_eq!(src_ip, expected_ip);
    assert_eq!(dst_ip, expected_ip);
    assert_eq!(body, expected_payload);
}

#[netstack_test]
async fn rolling_packet_capture_test(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");

    let provider = realm
        .connect_to_protocol::<fnet_debug::PacketCaptureProviderMarker>()
        .expect("connect to PacketCaptureProvider");

    let fnet_interfaces_ext::Properties { id: loopback_id, name: loopback_name, .. } = realm
        .loopback_properties()
        .await
        .expect("failed to get loopback properties")
        .expect("loopback not found");

    let common_params = fnet_debug::CommonPacketCaptureParams {
        interfaces: Some(fnet_debug::InterfaceSpecifier::InterfaceIds(vec![loopback_id.get()])),
        ..Default::default()
    };

    let rolling_params = fnet_debug::RollingPacketCaptureParams::default();

    let rolling_client = provider
        .start_rolling(common_params, &rolling_params)
        .await
        .expect("start_rolling FIDL error")
        .expect("start_rolling error")
        .into_proxy();

    // Bind a UDP socket and send a packet to trigger capture.
    const PORT: u16 = 9875;
    const PAYLOAD: [u8; 4] = [1, 2, 3, 4];
    send_and_recv_udp(&realm, (std_ip_v4!("127.0.0.1"), PORT).into(), &PAYLOAD).await;

    // Stop and download.
    let (file_client, file_server) = fidl::endpoints::create_endpoints();
    rolling_client.stop_and_download(file_server).expect("stop_and_download failed");

    let file_proxy = file_client.into_proxy();
    let bytes = fuchsia_fs::file::read(&file_proxy).await.expect("read file failed");
    let cap = pcap::parse_pcapng(&bytes).unwrap_or_else(|_| {
        panic!("could not parse file with pcap library: {bytes:?}");
    });
    let [interface] = &cap.interfaces[..] else {
        panic!("expected exactly one interface in the capture, got {:?}", cap);
    };
    assert_eq!(
        interface,
        &pcap::ParsedInterfaceDescription {
            link_type: pcap::LinkType::Ethernet,
            snap_len: 0,
            options: pcap::ParsedInterfaceDescriptionOptions {
                if_name: Some(std::borrow::Cow::Borrowed(loopback_name.as_str())),
            },
        }
    );

    assert_single_udp_packet(
        cap.packet_blocks(),
        net_declare::net_ip_v4!("127.0.0.1"),
        PORT,
        &PAYLOAD,
    );
}

#[netstack_test]
async fn packet_capture_multiple_interfaces_test(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");

    // Create two endpoints.
    let ep1 = sandbox.create_endpoint("ep1").await.expect("create endpoint ep1");
    let ep2 = sandbox.create_endpoint("ep2").await.expect("create endpoint ep2");

    async fn setup_interface<'a>(
        realm: &netemul::TestRealm<'a>,
        ep: netemul::TestEndpoint<'a>,
        subnet: fidl_fuchsia_net::Subnet,
    ) -> netemul::TestInterface<'a> {
        let iface = ep.into_interface_in_realm(realm).await.expect("attach ep");
        // Disable IPv6 to avoid NDP and MLD traffic from being captured.
        let _ = iface
            .control()
            .set_configuration(&fnet_interfaces_admin::Configuration {
                ipv6: Some(fnet_interfaces_admin::Ipv6Configuration {
                    enabled: Some(false),
                    ..Default::default()
                }),
                ..Default::default()
            })
            .await
            .expect("set_configuration FIDL error")
            .expect("set_configuration error");
        let _ = iface.control().enable().await.expect("enable iface").expect("enable error");
        iface.set_link_up(true).await.expect("set link up");
        iface.add_address_and_subnet_route(subnet).await.expect("configure address");
        iface
    }

    let if1 = setup_interface(&realm, ep1, fidl_subnet!("192.0.2.1/24")).await;
    let _if2 = setup_interface(&realm, ep2, fidl_subnet!("192.0.2.2/24")).await;

    // Connect to provider.
    let provider = realm
        .connect_to_protocol::<fnet_debug::PacketCaptureProviderMarker>()
        .expect("connect to PacketCaptureProvider");

    // Capture on interface 1.
    let common_params = fnet_debug::CommonPacketCaptureParams {
        interfaces: Some(fnet_debug::InterfaceSpecifier::InterfaceIds(vec![if1.id()])),
        ..Default::default()
    };
    let rolling_params = fnet_debug::RollingPacketCaptureParams::default();

    let rolling_client = provider
        .start_rolling(common_params, &rolling_params)
        .await
        .expect("start_rolling call fail")
        .expect("start_rolling error")
        .into_proxy();

    // Bind sockets and send packets.
    const PORT: u16 = 9876;

    const PAYLOAD1: [u8; 4] = [1, 2, 3, 4];
    const PAYLOAD2: [u8; 4] = [5, 6, 7, 8];

    send_and_recv_udp(&realm, (std_ip_v4!("192.0.2.1"), PORT).into(), &PAYLOAD1).await;
    send_and_recv_udp(&realm, (std_ip_v4!("192.0.2.2"), PORT).into(), &PAYLOAD2).await;

    // Stop and download.
    let (file_client, file_server) = fidl::endpoints::create_endpoints();
    rolling_client.stop_and_download(file_server).expect("stop_and_download failed");

    let file_proxy = file_client.into_proxy();
    let bytes = fuchsia_fs::file::read(&file_proxy).await.expect("read file failed");
    let cap = pcap::parse_pcapng(&bytes).expect("could not parse file with pcap library");

    // Verify capture contains PAYLOAD1 but not PAYLOAD2.
    assert_single_udp_packet(
        cap.packet_blocks(),
        net_declare::net_ip_v4!("192.0.2.1"),
        PORT,
        &PAYLOAD1,
    );
}

#[netstack_test]
async fn packet_capture_rolling_discard_oldest_test(name: &str) {
    let sandbox = netemul::TestSandbox::new().expect("create sandbox");
    let realm = sandbox.create_netstack_realm::<Netstack3, _>(name).expect("create realm");

    let provider = realm
        .connect_to_protocol::<fnet_debug::PacketCaptureProviderMarker>()
        .expect("connect to PacketCaptureProvider");

    let fnet_interfaces_ext::Properties { id: loopback_id, .. } = realm
        .loopback_properties()
        .await
        .expect("failed to get loopback properties")
        .expect("loopback not found");

    // Capture on loopback with small buffer.
    let common_params = fnet_debug::CommonPacketCaptureParams {
        interfaces: Some(fnet_debug::InterfaceSpecifier::InterfaceIds(vec![loopback_id.get()])),
        ..Default::default()
    };
    // Use MIN_BUFFER_SIZE.
    let rolling_params = fnet_debug::RollingPacketCaptureParams {
        capture_size: Some(fnet_debug::MIN_BUFFER_SIZE),
        ..Default::default()
    };

    let rolling_client = provider
        .start_rolling(common_params, &rolling_params)
        .await
        .expect("start_rolling call fail")
        .expect("start_rolling error")
        .into_proxy();

    // Bind socket and send packets.
    const PORT: u16 = 9877;
    let bind_addr = std::net::SocketAddr::from(([127, 0, 0, 1], PORT));
    let sock =
        fasync::net::UdpSocket::bind_in_realm(&realm, bind_addr).await.expect("create socket");

    // Send enough packets such that not all of them can be stored in
    // MIN_BUFFER_SIZE.
    const PAYLOAD_SIZE: usize = 65000;
    let max_capturable_count = usize::try_from(fnet_debug::MIN_BUFFER_SIZE).unwrap() / PAYLOAD_SIZE;
    let packet_count = max_capturable_count * 2;
    for i in 0..packet_count {
        let payload: Vec<u8> =
            std::iter::once(i as u8).chain((0..=255u8).cycle()).take(PAYLOAD_SIZE).collect();
        let sent = sock.send_to(&payload[..], bind_addr).await.expect("send_to failed");
        assert_eq!(sent, PAYLOAD_SIZE);

        // Actually receive the packets.
        let mut recv_buf = vec![0u8; PAYLOAD_SIZE];
        let (received, from_addr) = sock.recv_from(&mut recv_buf).await.expect("recv_from failed");
        assert_eq!(received, PAYLOAD_SIZE);
        assert_eq!(&recv_buf[..], &payload[..]);
        assert_eq!(from_addr, bind_addr);
    }

    // Stop and download.
    let (file_client, file_server) = fidl::endpoints::create_endpoints();
    rolling_client.stop_and_download(file_server).expect("stop_and_download failed");

    let file_proxy = file_client.into_proxy();
    let bytes = fuchsia_fs::file::read(&file_proxy).await.expect("read file failed");
    let cap = pcap::parse_pcapng(&bytes).expect("could not parse file with pcap library");

    let want_tail_bytes = (0..=255u8).cycle().take(PAYLOAD_SIZE - 1).collect::<Vec<_>>();
    let identifier_bytes = cap
        .packet_blocks()
        .map(|epb| {
            let epb = epb
                .unwrap_or_else(|e| panic!("failed to parse packet block {e:?}\nbytes: {bytes:?}"));
            let buf = &epb.packet_data;
            let (mut body, _src_mac, _dst_mac, src_ip, dst_ip, proto, _ttl) =
                packet_formats::testutil::parse_ip_packet_in_ethernet_frame::<net_types::ip::Ipv4>(
                    &buf,
                    packet_formats::ethernet::EthernetFrameLengthCheck::NoCheck,
                )
                .expect("failed to parse IP packet");

            assert_eq!(
                proto,
                packet_formats::ip::Ipv4Proto::Proto(packet_formats::ip::IpProto::Udp)
            );
            let udp = packet_formats::udp::UdpPacket::parse(
                &mut body,
                packet_formats::udp::UdpParseArgs::new(src_ip, dst_ip),
            )
            .expect("failed to parse UDP packet");

            assert_eq!(udp.src_port().map(|p| p.get()), Some(PORT));
            assert_eq!(udp.dst_port().get(), PORT);
            assert_eq!(src_ip, net_ip_v4!("127.0.0.1"));
            assert_eq!(dst_ip, net_ip_v4!("127.0.0.1"));

            // Compare all bytes of the body except the first byte.
            assert_eq!(body.len(), PAYLOAD_SIZE);
            assert_eq!(body[1..], want_tail_bytes);
            body[0]
        })
        .collect::<Vec<_>>();

    assert!(
        identifier_bytes.len() <= max_capturable_count,
        "got {} packets but expected no more than {}",
        identifier_bytes.len(),
        max_capturable_count,
    );
    let start = u8::try_from(packet_count - identifier_bytes.len()).unwrap();
    let want = (start..u8::try_from(packet_count).unwrap()).collect::<Vec<_>>();
    assert_eq!(identifier_bytes, want);
}
