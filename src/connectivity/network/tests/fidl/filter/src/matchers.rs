// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt::Debug;
use std::num::NonZeroU64;
use std::ops::RangeInclusive;
use std::sync::Arc;

use fidl_fuchsia_net_filter_ext::Matchers;
use net_types::ip::{Ip, IpVersion};
use {
    fidl_fuchsia_ebpf as febpf, fidl_fuchsia_net_ext as fnet_ext,
    fidl_fuchsia_net_matchers as fnet_matchers, fidl_fuchsia_net_matchers_ext as fnet_matchers_ext,
};

use crate::ip_hooks::{
    IcmpSocket, Interfaces, IrrelevantToTest, Ports, SocketType, Subnets, TcpSocket, UdpSocket,
};

/// Defines a matcher used in tests.
///
/// Matchers may be stateful. For stateful matchers `clone()` creates a new
/// instance of the matcher that still shares the internal state with the
/// original instances. `split()` can be used to create an equivalent matcher
/// that doesn't share any state with the original. `verify_called()` and
/// `verify_not_called()` can used to verify that the matcher was executed or
/// not executed, respectively. These methods should be called only on a
/// brand new instance produced by `split()`.
pub(crate) trait Matcher: Clone + Debug {
    type SocketType: SocketType;

    /// Returns an eBPF program used by the matcher, if any. The program should
    /// be registered with the filter controller before the matcher is
    /// installed.
    fn ebpf_program(&self) -> Option<(febpf::ProgramHandle, febpf::VerifiedProgram)>;

    /// Returns matcher defiition that can be used to install the matcher in a
    /// filter rule.
    async fn matcher<I: Ip>(
        &self,
        interfaces: Interfaces<'_>,
        subnets: Subnets,
        ports: Ports,
    ) -> Matchers;

    /// Creates an equivalent matcher that doesn't share any state with the
    /// original.
    fn split(&self) -> Self;

    /// If possible, verifies that the matcher was executed with the specified
    /// parameters.
    fn verify_called(&self, _ip_version: IpVersion);

    /// If possible, verifies that the matcher was not executed.
    fn verify_not_called(&self);
}

pub(crate) trait StatelessMatcher {
    type SocketType: SocketType;

    async fn matcher<I: Ip>(
        &self,
        interfaces: Interfaces<'_>,
        subnets: Subnets,
        ports: Ports,
    ) -> Matchers;
}

impl<T> Matcher for T
where
    T: StatelessMatcher + Clone + Debug,
{
    type SocketType = T::SocketType;

    fn ebpf_program(&self) -> Option<(febpf::ProgramHandle, febpf::VerifiedProgram)> {
        None
    }

    async fn matcher<I: Ip>(
        &self,
        interfaces: Interfaces<'_>,
        subnets: Subnets,
        ports: Ports,
    ) -> Matchers {
        <Self as StatelessMatcher>::matcher::<I>(self, interfaces, subnets, ports).await
    }

    fn split(&self) -> Self {
        self.clone()
    }
    fn verify_called(&self, _ip_version: IpVersion) {}
    fn verify_not_called(&self) {}
}

#[derive(Clone, Copy, Debug)]
pub(crate) enum Inversion {
    Default,
    InverseMatch,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct AllTraffic;

impl StatelessMatcher for AllTraffic {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        Matchers::default()
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InterfaceId;

impl StatelessMatcher for InterfaceId {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        let Interfaces { ingress, egress } = interfaces;
        Matchers {
            in_interface: ingress.map(|interface| {
                fnet_matchers_ext::Interface::Id(
                    NonZeroU64::new(interface.id()).expect("interface ID should be nonzero"),
                )
            }),
            out_interface: egress.map(|interface| {
                fnet_matchers_ext::Interface::Id(
                    NonZeroU64::new(interface.id()).expect("interface ID should be nonzero"),
                )
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InterfaceName;

impl StatelessMatcher for InterfaceName {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        async fn get_interface_name(
            interface: &netemul::TestInterface<'_>,
        ) -> fnet_matchers_ext::Interface {
            fnet_matchers_ext::Interface::Name(
                interface.get_interface_name().await.expect("get interface name"),
            )
        }

        let Interfaces { ingress, egress } = interfaces;
        let in_interface = match ingress {
            Some(ingress) => Some(get_interface_name(ingress).await),
            None => None,
        };
        let out_interface = match egress {
            Some(egress) => Some(get_interface_name(egress).await),
            None => None,
        };
        Matchers { in_interface, out_interface, ..Default::default() }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct InterfaceDeviceClass;

impl StatelessMatcher for InterfaceDeviceClass {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        async fn get_port_class(
            interface: &netemul::TestInterface<'_>,
        ) -> fnet_matchers_ext::Interface {
            fnet_matchers_ext::Interface::PortClass(
                interface.get_port_class().await.expect("get port class").into(),
            )
        }

        let Interfaces { ingress, egress } = interfaces;
        let in_interface = match ingress {
            Some(ingress) => Some(get_port_class(ingress).await),
            None => None,
        };
        let out_interface = match egress {
            Some(egress) => Some(get_port_class(egress).await),
            None => None,
        };
        Matchers { in_interface, out_interface, ..Default::default() }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SrcAddressSubnet(pub(crate) Inversion);

impl StatelessMatcher for SrcAddressSubnet {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Subnets { src, other, dst: _ } = subnets;

        Matchers {
            src_addr: Some(match inversion {
                Inversion::Default => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                        fnet_ext::apply_subnet_mask(src)
                            .try_into()
                            .expect("subnet should be valid"),
                    ),
                    invert: false,
                },
                Inversion::InverseMatch => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                        other.try_into().expect("subnet should be valid"),
                    ),
                    invert: true,
                },
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct SrcAddressRange(pub(crate) Inversion);

impl StatelessMatcher for SrcAddressRange {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Subnets { src, other, dst: _ } = subnets;

        Matchers {
            src_addr: Some(match inversion {
                Inversion::Default => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Range(
                        fnet_matchers::AddressRange { start: src.addr, end: src.addr }
                            .try_into()
                            .expect("address range should be valid"),
                    ),
                    invert: false,
                },
                Inversion::InverseMatch => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Range(
                        fnet_matchers::AddressRange { start: other.addr, end: other.addr }
                            .try_into()
                            .expect("address range should be valid"),
                    ),
                    invert: true,
                },
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DstAddressSubnet(pub(crate) Inversion);

impl StatelessMatcher for DstAddressSubnet {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Subnets { dst, other, src: _ } = subnets;

        Matchers {
            dst_addr: Some(match inversion {
                Inversion::Default => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                        fnet_ext::apply_subnet_mask(dst)
                            .try_into()
                            .expect("subnet should be valid"),
                    ),
                    invert: false,
                },
                Inversion::InverseMatch => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                        other.try_into().expect("subnet should be valid"),
                    ),
                    invert: true,
                },
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct DstAddressRange(pub(crate) Inversion);

impl StatelessMatcher for DstAddressRange {
    type SocketType = IrrelevantToTest;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Subnets { dst, other, src: _ } = subnets;

        Matchers {
            dst_addr: Some(match inversion {
                Inversion::Default => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Range(
                        fnet_matchers::AddressRange { start: dst.addr, end: dst.addr }
                            .try_into()
                            .expect("address range should be valid"),
                    ),
                    invert: false,
                },
                Inversion::InverseMatch => fnet_matchers_ext::Address {
                    matcher: fnet_matchers_ext::AddressMatcherType::Range(
                        fnet_matchers::AddressRange { start: other.addr, end: other.addr }
                            .try_into()
                            .expect("address range should be valid"),
                    ),
                    invert: true,
                },
            }),
            ..Default::default()
        }
    }
}

fn unique_ephemeral_port(exclude: &[u16]) -> u16 {
    // RFC 6335 section 6 defines 49152-65535 as the ephemeral port range.
    const RANGE: RangeInclusive<u16> = 49152..=65535;
    for port in RANGE {
        if !exclude.contains(&port) {
            return port;
        }
    }
    panic!("could not find an available port in the ephemeral range")
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Tcp;

impl StatelessMatcher for Tcp {
    type SocketType = TcpSocket;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        Matchers {
            transport_protocol: Some(fnet_matchers_ext::TransportProtocol::Tcp {
                src_port: None,
                dst_port: None,
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TcpSrcPort(pub(crate) Inversion);

impl StatelessMatcher for TcpSrcPort {
    type SocketType = TcpSocket;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Ports { src, dst } = ports;

        Matchers {
            transport_protocol: Some(fnet_matchers_ext::TransportProtocol::Tcp {
                src_port: Some(match inversion {
                    Inversion::Default => {
                        fnet_matchers_ext::Port::new(src, src, /* invert */ false)
                            .expect("should be valid port range")
                    }
                    Inversion::InverseMatch => {
                        let port = unique_ephemeral_port(&[src, dst]);
                        fnet_matchers_ext::Port::new(port, port, /* invert */ true)
                            .expect("should be valid port range")
                    }
                }),
                dst_port: None,
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct TcpDstPort(pub(crate) Inversion);

impl StatelessMatcher for TcpDstPort {
    type SocketType = TcpSocket;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Ports { src, dst } = ports;

        Matchers {
            transport_protocol: Some(fnet_matchers_ext::TransportProtocol::Tcp {
                src_port: None,
                dst_port: Some(match inversion {
                    Inversion::Default => {
                        fnet_matchers_ext::Port::new(dst, dst, /* invert */ false)
                            .expect("should be valid port range")
                    }
                    Inversion::InverseMatch => {
                        let port = unique_ephemeral_port(&[src, dst]);
                        fnet_matchers_ext::Port::new(port, port, /* invert */ true)
                            .expect("should be valid port range")
                    }
                }),
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Udp;

impl StatelessMatcher for Udp {
    type SocketType = UdpSocket;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        Matchers {
            transport_protocol: Some(fnet_matchers_ext::TransportProtocol::Udp {
                src_port: None,
                dst_port: None,
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UdpSrcPort(pub(crate) Inversion);

impl StatelessMatcher for UdpSrcPort {
    type SocketType = UdpSocket;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Ports { src, dst } = ports;

        Matchers {
            transport_protocol: Some(fnet_matchers_ext::TransportProtocol::Udp {
                src_port: Some(match inversion {
                    Inversion::Default => {
                        fnet_matchers_ext::Port::new(src, src, /* invert */ false)
                            .expect("should be valid port range")
                    }
                    Inversion::InverseMatch => {
                        let port = unique_ephemeral_port(&[src, dst]);
                        fnet_matchers_ext::Port::new(port, port, /* invert */ true)
                            .expect("should be valid port range")
                    }
                }),
                dst_port: None,
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct UdpDstPort(pub(crate) Inversion);

impl StatelessMatcher for UdpDstPort {
    type SocketType = UdpSocket;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        ports: Ports,
    ) -> Matchers {
        let Self(inversion) = self;
        let Ports { src, dst } = ports;

        Matchers {
            transport_protocol: Some(fnet_matchers_ext::TransportProtocol::Udp {
                src_port: None,
                dst_port: Some(match inversion {
                    Inversion::Default => {
                        fnet_matchers_ext::Port::new(dst, dst, /* invert */ false)
                            .expect("should be valid port range")
                    }
                    Inversion::InverseMatch => {
                        let port = unique_ephemeral_port(&[src, dst]);
                        fnet_matchers_ext::Port::new(port, port, /* invert */ true)
                            .expect("should be valid port range")
                    }
                }),
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct Icmp;

impl StatelessMatcher for Icmp {
    type SocketType = IcmpSocket;

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        Matchers {
            transport_protocol: Some({
                match I::VERSION {
                    IpVersion::V4 => fnet_matchers_ext::TransportProtocol::Icmp,
                    IpVersion::V6 => fnet_matchers_ext::TransportProtocol::Icmpv6,
                }
            }),
            ..Default::default()
        }
    }
}

#[derive(Clone)]
pub(crate) struct EbpfMatcher {
    program: Arc<ebpf_test_util::TestProgram>,
}

impl Debug for EbpfMatcher {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("EbpfMatcher").field("id", &self.program.get_program_id().id).finish()
    }
}

impl EbpfMatcher {
    pub(crate) fn new() -> Self {
        Self {
            program: Arc::new(ebpf_test_util::TestProgram::load(
                ebpf_api::ProgramType::SocketFilter,
            )),
        }
    }
}

impl Matcher for EbpfMatcher {
    type SocketType = UdpSocket;

    fn ebpf_program(&self) -> Option<(febpf::ProgramHandle, febpf::VerifiedProgram)> {
        Some((self.program.get_program_handle(), self.program.get_fidl_program()))
    }

    async fn matcher<I: Ip>(
        &self,
        _interfaces: Interfaces<'_>,
        _subnets: Subnets,
        _ports: Ports,
    ) -> Matchers {
        Matchers { ebpf_program: Some(self.program.get_program_id()), ..Default::default() }
    }

    fn split(&self) -> Self {
        Self::new()
    }

    fn verify_called(&self, ip_version: IpVersion) {
        let result = self.program.read_test_result();
        assert_eq!(
            result.proto,
            u32::from(u16::from(packet_formats::ethernet::EtherType::from_ip_version(ip_version)))
        );

        // This may be either UDP or ICMP packet. Just make sure that the field was set.
        assert_ne!(result.ip_proto, 0);
    }

    fn verify_not_called(&self) {
        let result = self.program.read_test_result();
        assert_eq!(result.proto, 0);
        assert_eq!(result.ip_proto, 0);
    }
}
