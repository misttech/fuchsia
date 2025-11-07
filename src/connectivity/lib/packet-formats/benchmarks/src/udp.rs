// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::num::NonZeroU16;

use itertools::Itertools as _;
use net_types::ip::IpVersion;
use packet::{Buf, InnerPacketBuilder as _, PacketBuilder as _, ParseBuffer as _};
use packet_formats::ethernet::{EthernetFrame, EthernetFrameLengthCheck};
use packet_formats::ip::{IpPacket as _, IpProto};
use packet_formats::udp::{UdpPacket, UdpPacketBuilder, UdpParseArgs};

use crate::ip::{ExtractedIpInfo, IpBenchmarkConfig, IpExt};
use crate::{Bencher, BenchmarkGroup};

const SRC_PORT: Option<NonZeroU16> = NonZeroU16::new(1234);
const DST_PORT: NonZeroU16 = NonZeroU16::new(4321).unwrap();

#[derive(Copy, Clone, Debug)]
struct UdpBenchmarkConfig {
    ip: IpBenchmarkConfig,
    payload_size: usize,
}

impl UdpBenchmarkConfig {
    fn combinations() -> impl Iterator<Item = Self> + Clone {
        IpBenchmarkConfig::combinations()
            .cartesian_product([1, 5, 25])
            .map(|(ip, payload_size)| Self { ip, payload_size: payload_size << 10 })
    }
}

#[derive(Debug, Eq, PartialEq)]
#[cfg_attr(not(test), allow(unused))]
struct ExtractedUdpInfo<I: IpExt> {
    ip_info: ExtractedIpInfo<I>,
    src_port: Option<NonZeroU16>,
    dst_port: NonZeroU16,
    payload_size: usize,
}

impl<I: IpExt> ExtractedUdpInfo<I> {
    #[cfg(test)]
    fn expected(options: &UdpBenchmarkConfig) -> Self {
        let UdpBenchmarkConfig { ip, payload_size } = options;
        Self {
            ip_info: ExtractedIpInfo::expected(ip, IpProto::Udp.into()),
            src_port: SRC_PORT,
            dst_port: DST_PORT,
            payload_size: *payload_size,
        }
    }
}

fn make_udp_datagram<I: IpExt>(options: &UdpBenchmarkConfig, payload: &[u8]) -> Vec<u8> {
    let UdpBenchmarkConfig { ip, payload_size: _ } = options;
    let datagram = UdpPacketBuilder::new(I::SRC_ADDR, I::DST_ADDR, SRC_PORT, DST_PORT)
        .wrap_body(payload.into_serializer());
    I::make_packet(ip, IpProto::Udp, datagram)
}

fn bench_parse<I: IpExt, B: Bencher>(bencher: &mut B, options: &UdpBenchmarkConfig) {
    let UdpBenchmarkConfig { ip, payload_size } = options;
    let payload = (0..*payload_size).into_iter().map(|x| x as u8).collect::<Vec<_>>();
    let datagram = make_udp_datagram::<I>(options, &payload[..]);
    bencher.iter(|| {
        let mut buffer = Buf::new(&datagram[..], ..);
        if ip.ethernet {
            // Don't do anything with the Ethernet header, we only have variants
            // for it to catch alignment variations.
            let _ = buffer
                .parse_with::<_, EthernetFrame<&[u8]>>(EthernetFrameLengthCheck::NoCheck)
                .unwrap();
        }
        let packet = buffer.parse::<I::Packet<&[u8]>>().unwrap();
        let ip_info = I::extract_info(&packet);
        let args = UdpParseArgs::new(packet.src_ip(), packet.dst_ip());
        drop(packet);
        let packet = buffer.parse_with::<_, UdpPacket<&[u8]>>(args).unwrap();
        let udp_info = ExtractedUdpInfo {
            ip_info,
            src_port: packet.src_port(),
            dst_port: packet.dst_port(),
            payload_size: packet.body().len(),
        };

        #[cfg(test)]
        assert_eq!(udp_info, ExtractedUdpInfo::expected(&options));

        udp_info
    });
}

pub(crate) fn get_benches<G: BenchmarkGroup>(group: &mut G) {
    let iter = [IpVersion::V4, IpVersion::V6]
        .into_iter()
        .cartesian_product(UdpBenchmarkConfig::combinations());
    for (ip_version, udp) in iter {
        let UdpBenchmarkConfig { ip, payload_size } = &udp;
        let name =
            format!("parse/{}/UDP/{}KiB", ip.bench_name_particle(ip_version), *payload_size >> 10);

        group.register(name, move |bencher| {
            net_types::for_any_ip_version!(ip_version, I, bench_parse::<I, _>(bencher, &udp));
        });
    }
}
