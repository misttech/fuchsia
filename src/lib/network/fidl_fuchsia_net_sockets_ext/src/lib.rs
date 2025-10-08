// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_net_ext::IntoExt;
use net_types::ip;
use thiserror::Error;
use {
    fidl_fuchsia_net_matchers as fnet_matchers, fidl_fuchsia_net_matchers_ext as fnet_matchers_ext,
    fidl_fuchsia_net_sockets as fnet_sockets,
};

/// An extension type for [`fnet_sockets::IpSocketMatcher`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IpSocketMatcher {
    Family(ip::IpVersion),
    SrcAddr(fnet_matchers_ext::Address),
    DstAddr(fnet_matchers_ext::Address),
    Proto(fnet_matchers_ext::SocketTransportProtocol),
    BoundInterface(fnet_matchers_ext::BoundInterface),
    Cookie(fnet_matchers::SocketCookie),
    Mark1(fnet_matchers_ext::Mark),
    Mark2(fnet_matchers_ext::Mark),
}

#[derive(Debug, PartialEq, Error)]
pub enum IpSocketMatcherError {
    #[error("got unexpected union variant: {0}")]
    UnknownUnionVariant(u64),
    #[error("address matcher conversion failure: {0}")]
    Address(fnet_matchers_ext::AddressError),
    #[error("protocol matcher conversion failure: {0}")]
    TransportProtocol(fnet_matchers_ext::SocketTransportProtocolError),
    #[error("bound interface matcher conversion failure: {0}")]
    BoundInterface(fnet_matchers_ext::BoundInterfaceError),
    #[error("mark matcher conversion failure: {0}")]
    Mark(fnet_matchers_ext::MarkError),
}

impl TryFrom<fnet_sockets::IpSocketMatcher> for IpSocketMatcher {
    type Error = IpSocketMatcherError;

    fn try_from(matcher: fnet_sockets::IpSocketMatcher) -> Result<Self, Self::Error> {
        match matcher {
            fnet_sockets::IpSocketMatcher::Family(ip_version) => {
                Ok(Self::Family(ip_version.into_ext()))
            }
            fnet_sockets::IpSocketMatcher::SrcAddr(addr) => {
                Ok(Self::SrcAddr(addr.try_into().map_err(|e| IpSocketMatcherError::Address(e))?))
            }
            fnet_sockets::IpSocketMatcher::DstAddr(addr) => {
                Ok(Self::DstAddr(addr.try_into().map_err(|e| IpSocketMatcherError::Address(e))?))
            }
            fnet_sockets::IpSocketMatcher::Proto(proto) => Ok(Self::Proto(
                proto.try_into().map_err(|e| IpSocketMatcherError::TransportProtocol(e))?,
            )),
            fnet_sockets::IpSocketMatcher::BoundInterface(bound_interface) => {
                Ok(Self::BoundInterface(
                    bound_interface
                        .try_into()
                        .map_err(|e| IpSocketMatcherError::BoundInterface(e))?,
                ))
            }
            fnet_sockets::IpSocketMatcher::Cookie(cookie) => Ok(Self::Cookie(cookie)),
            fnet_sockets::IpSocketMatcher::Mark1(mark) => {
                Ok(Self::Mark1(mark.try_into().map_err(|e| IpSocketMatcherError::Mark(e))?))
            }
            fnet_sockets::IpSocketMatcher::Mark2(mark) => {
                Ok(Self::Mark2(mark.try_into().map_err(|e| IpSocketMatcherError::Mark(e))?))
            }
            fnet_sockets::IpSocketMatcher::__SourceBreaking { unknown_ordinal } => {
                Err(IpSocketMatcherError::UnknownUnionVariant(unknown_ordinal))
            }
        }
    }
}

impl From<IpSocketMatcher> for fnet_sockets::IpSocketMatcher {
    fn from(value: IpSocketMatcher) -> Self {
        match value {
            IpSocketMatcher::Family(ip_version) => {
                fnet_sockets::IpSocketMatcher::Family(ip_version.into_ext())
            }
            IpSocketMatcher::SrcAddr(address) => {
                fnet_sockets::IpSocketMatcher::SrcAddr(address.into())
            }
            IpSocketMatcher::DstAddr(address) => {
                fnet_sockets::IpSocketMatcher::DstAddr(address.into())
            }
            IpSocketMatcher::Proto(socket_transport_protocol) => {
                fnet_sockets::IpSocketMatcher::Proto(socket_transport_protocol.into())
            }
            IpSocketMatcher::BoundInterface(mark) => {
                fnet_sockets::IpSocketMatcher::BoundInterface(mark.into())
            }
            IpSocketMatcher::Cookie(socket_cookie) => {
                fnet_sockets::IpSocketMatcher::Cookie(socket_cookie)
            }
            IpSocketMatcher::Mark1(mark) => fnet_sockets::IpSocketMatcher::Mark1(mark.into()),
            IpSocketMatcher::Mark2(mark) => fnet_sockets::IpSocketMatcher::Mark2(mark.into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::num::NonZeroU64;

    use super::*;
    use fidl_fuchsia_net as fnet;
    use net_declare::fidl_subnet;
    use test_case::test_case;

    #[test_case(
        fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V4),
        IpSocketMatcher::Family(ip::IpVersion::V4);
        "FamilyIpv4"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Family(fnet::IpVersion::V6),
        IpSocketMatcher::Family(ip::IpVersion::V6);
        "FamilyIpv6"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::SrcAddr(fnet_matchers::Address {
            matcher: fnet_matchers::AddressMatcherType::Subnet(fidl_subnet!("192.0.2.0/24")),
            invert: true,
        }),
        IpSocketMatcher::SrcAddr(fnet_matchers_ext::Address {
            matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                fnet_matchers_ext::Subnet::try_from(fidl_subnet!("192.0.2.0/24")).unwrap()
            ),
            invert: true,
        });
        "SrcAddr"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::DstAddr(fnet_matchers::Address {
            matcher: fnet_matchers::AddressMatcherType::Subnet(fidl_subnet!("2001:db8::/32")),
            invert: false,
        }),
        IpSocketMatcher::DstAddr(fnet_matchers_ext::Address {
            matcher: fnet_matchers_ext::AddressMatcherType::Subnet(
                fnet_matchers_ext::Subnet::try_from(fidl_subnet!("2001:db8::/32")).unwrap()
            ),
            invert: false,
        });
        "DstAddr"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Tcp(
            fnet_matchers::TcpSocket::Empty(fnet_matchers::Empty)
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Tcp(
            fnet_matchers_ext::TcpSocket::Empty
        ));
        "ProtoTcp"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(fnet_matchers::SocketTransportProtocol::Udp(
            fnet_matchers::UdpSocket::Empty(fnet_matchers::Empty)
        )),
        IpSocketMatcher::Proto(fnet_matchers_ext::SocketTransportProtocol::Udp(
            fnet_matchers_ext::UdpSocket::Empty
        ));
        "ProtoUdp"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::BoundInterface(fnet_matchers::BoundInterface::Unbound(
            fnet_matchers::Unbound
        )),
        IpSocketMatcher::BoundInterface(fnet_matchers_ext::BoundInterface::Unbound);
        "BoundInterfaceUnbound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::BoundInterface(fnet_matchers::BoundInterface::Bound(
            fnet_matchers::Interface::Id(1)
        )),
        IpSocketMatcher::BoundInterface(fnet_matchers_ext::BoundInterface::Bound(
            fnet_matchers_ext::Interface::Id(NonZeroU64::new(1).unwrap())
        ));
        "BoundInterfaceBound"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Cookie(fnet_matchers::SocketCookie {
            cookie: 12345,
            invert: false,
        }),
        IpSocketMatcher::Cookie(fnet_matchers::SocketCookie {
            cookie: 12345,
            invert: false,
        });
        "Cookie"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Mark1(fnet_matchers::Mark::Unmarked(fnet_matchers::Unmarked)),
        IpSocketMatcher::Mark1(fnet_matchers_ext::Mark::Unmarked);
        "Mark1"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Mark2(fnet_matchers::Mark::Unmarked(fnet_matchers::Unmarked)),
        IpSocketMatcher::Mark2(fnet_matchers_ext::Mark::Unmarked);
        "Mark2"
    )]
    fn convert_from_fidl_and_back<F, E>(fidl_type: F, local_type: E)
    where
        E: TryFrom<F> + Clone + std::fmt::Debug + PartialEq,
        <E as TryFrom<F>>::Error: std::fmt::Debug + PartialEq,
        F: From<E> + Clone + std::fmt::Debug + PartialEq,
    {
        assert_eq!(fidl_type.clone().try_into(), Ok(local_type.clone()));
        assert_eq!(<_ as Into<F>>::into(local_type), fidl_type.clone());
    }

    #[test_case(
        fnet_sockets::IpSocketMatcher::__SourceBreaking { unknown_ordinal: 100 } =>
            Err(IpSocketMatcherError::UnknownUnionVariant(100));
        "UnknownUnionVariant"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::SrcAddr(fnet_matchers::Address {
            matcher: fnet_matchers::AddressMatcherType::__SourceBreaking { unknown_ordinal: 100 },
            invert: false,
        }) => Err(IpSocketMatcherError::Address(fnet_matchers_ext::AddressError::AddressMatcherType(
            fnet_matchers_ext::AddressMatcherTypeError::UnknownUnionVariant
        )));
        "AddressError"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Proto(
            fnet_matchers::SocketTransportProtocol::__SourceBreaking { unknown_ordinal: 100 }
        ) => Err(IpSocketMatcherError::TransportProtocol(
            fnet_matchers_ext::SocketTransportProtocolError::UnknownUnionVariant(100)
        ));
        "TransportProtocolError"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::BoundInterface(
            fnet_matchers::BoundInterface::__SourceBreaking { unknown_ordinal: 100 }
        ) => Err(IpSocketMatcherError::BoundInterface(
            fnet_matchers_ext::BoundInterfaceError::UnknownUnionVariant(100)
        ));
        "BoundInterfaceError"
    )]
    #[test_case(
        fnet_sockets::IpSocketMatcher::Mark1(
            fnet_matchers::Mark::__SourceBreaking { unknown_ordinal: 100 }
        ) => Err(IpSocketMatcherError::Mark(
            fnet_matchers_ext::MarkError::UnknownUnionVariant(100)
        ));
        "MarkError"
    )]
    fn ip_socket_matcher_try_from_error(
        fidl: fnet_sockets::IpSocketMatcher,
    ) -> Result<IpSocketMatcher, IpSocketMatcherError> {
        IpSocketMatcher::try_from(fidl)
    }
}
