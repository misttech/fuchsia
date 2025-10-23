// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use net_types::ip::{GenericOverIp, Ip};
use packet_formats::ip::{IpExt, IpProto, Ipv4Proto, Ipv6Proto};
use {
    fidl_fuchsia_net_filter_ext as fnet_filter_ext, fidl_fuchsia_net_interfaces as fnet_interfaces,
    fidl_fuchsia_net_matchers_ext as fnet_matchers_ext,
};

use super::{ConversionResult, IpVersionMismatchError, IpVersionStrictness, TryConvertToCoreState};
use crate::bindings::util::IntoCore;

impl TryConvertToCoreState for fnet_filter_ext::Matchers {
    type CoreState<I: IpExt> = netstack3_core::filter::PacketMatcher<I, fnet_interfaces::PortClass>;

    fn try_convert<I: IpExt>(
        self,
        ip_version_strictness: IpVersionStrictness,
    ) -> Result<ConversionResult<Self::CoreState<I>>, IpVersionMismatchError> {
        let Self { in_interface, out_interface, src_addr, dst_addr, transport_protocol } = self;

        let in_interface = in_interface.map(|matcher| matcher.into_core());
        let out_interface = out_interface.map(|matcher| matcher.into_core());

        let src_address = match src_addr
            .map(|matcher| matcher.try_convert::<I>(ip_version_strictness))
            .transpose()?
        {
            Some(ConversionResult::Omit) => return Ok(ConversionResult::Omit),
            Some(ConversionResult::State(matcher)) => Some(matcher),
            None => None,
        };
        let dst_address = match dst_addr
            .map(|matcher| matcher.try_convert::<I>(ip_version_strictness))
            .transpose()?
        {
            Some(ConversionResult::Omit) => return Ok(ConversionResult::Omit),
            Some(ConversionResult::State(matcher)) => Some(matcher),
            None => None,
        };
        let transport_protocol = match transport_protocol
            .map(|matcher| matcher.try_convert::<I>(ip_version_strictness))
            .transpose()?
        {
            Some(ConversionResult::Omit) => return Ok(ConversionResult::Omit),
            Some(ConversionResult::State(matcher)) => Some(matcher),
            None => None,
        };

        Ok(ConversionResult::State(netstack3_core::filter::PacketMatcher {
            in_interface,
            out_interface,
            src_address,
            dst_address,
            transport_protocol,
        }))
    }
}

impl TryConvertToCoreState for fnet_matchers_ext::Address {
    type CoreState<I: IpExt> = netstack3_core::ip::AddressMatcher<I::Addr>;

    fn try_convert<I: IpExt>(
        self,
        ip_version_strictness: IpVersionStrictness,
    ) -> Result<ConversionResult<Self::CoreState<I>>, IpVersionMismatchError> {
        #[derive(GenericOverIp)]
        #[generic_over_ip(I, Ip)]
        pub(super) struct Wrap<I: IpExt>(
            Result<
                ConversionResult<netstack3_core::ip::AddressMatcher<I::Addr>>,
                IpVersionMismatchError,
            >,
        );

        let either_matcher: netstack3_core::ip::AddressMatcherEither = self.into_core();
        let Wrap(result) = I::map_ip_out::<_, Wrap<I>>(
            either_matcher,
            |either_matcher| match either_matcher {
                netstack3_core::ip::AddressMatcherEither::V4(matcher) => {
                    Wrap(Ok(ConversionResult::State(matcher)))
                }
                netstack3_core::ip::AddressMatcherEither::V6(_) => {
                    Wrap(ip_version_strictness.mismatch_result())
                }
            },
            |either_matcher| match either_matcher {
                netstack3_core::ip::AddressMatcherEither::V4(_) => {
                    Wrap(ip_version_strictness.mismatch_result())
                }
                netstack3_core::ip::AddressMatcherEither::V6(matcher) => {
                    Wrap(Ok(ConversionResult::State(matcher)))
                }
            },
        );

        result
    }
}

impl TryConvertToCoreState for fnet_matchers_ext::TransportProtocol {
    type CoreState<I: IpExt> = netstack3_core::filter::TransportProtocolMatcher<I::Proto>;

    fn try_convert<I: IpExt>(
        self,
        ip_version_strictness: IpVersionStrictness,
    ) -> Result<ConversionResult<Self::CoreState<I>>, IpVersionMismatchError> {
        #[derive(GenericOverIp)]
        #[generic_over_ip(I, Ip)]
        pub(super) struct Wrap<I: IpExt>(
            Result<ConversionResult<I::Proto>, IpVersionMismatchError>,
        );

        let matcher = match self {
            fnet_matchers_ext::TransportProtocol::Tcp { src_port, dst_port } => {
                netstack3_core::filter::TransportProtocolMatcher {
                    proto: I::map_ip_out(
                        (),
                        |()| Ipv4Proto::Proto(IpProto::Tcp),
                        |()| Ipv6Proto::Proto(IpProto::Tcp),
                    ),
                    src_port: src_port.map(IntoCore::into_core),
                    dst_port: dst_port.map(IntoCore::into_core),
                }
            }
            fnet_matchers_ext::TransportProtocol::Udp { src_port, dst_port } => {
                netstack3_core::filter::TransportProtocolMatcher {
                    proto: I::map_ip_out(
                        (),
                        |()| Ipv4Proto::Proto(IpProto::Udp),
                        |()| Ipv6Proto::Proto(IpProto::Udp),
                    ),
                    src_port: src_port.map(IntoCore::into_core),
                    dst_port: dst_port.map(IntoCore::into_core),
                }
            }
            fnet_matchers_ext::TransportProtocol::Icmp => {
                let Wrap(result) = I::map_ip_out::<_, Wrap<I>>(
                    (),
                    |()| Wrap(Ok(ConversionResult::State(Ipv4Proto::Icmp))),
                    |()| Wrap(ip_version_strictness.mismatch_result()),
                );
                let proto = match result? {
                    ConversionResult::State(proto) => proto,
                    ConversionResult::Omit => return Ok(ConversionResult::Omit),
                };
                netstack3_core::filter::TransportProtocolMatcher {
                    proto,
                    src_port: None,
                    dst_port: None,
                }
            }
            fnet_matchers_ext::TransportProtocol::Icmpv6 => {
                let Wrap(result) = I::map_ip_out::<_, Wrap<I>>(
                    (),
                    |()| Wrap(ip_version_strictness.mismatch_result()),
                    |()| Wrap(Ok(ConversionResult::State(Ipv6Proto::Icmpv6))),
                );
                let proto = match result? {
                    ConversionResult::State(proto) => proto,
                    ConversionResult::Omit => return Ok(ConversionResult::Omit),
                };
                netstack3_core::filter::TransportProtocolMatcher {
                    proto,
                    src_port: None,
                    dst_port: None,
                }
            }
        };
        Ok(ConversionResult::State(matcher))
    }
}
