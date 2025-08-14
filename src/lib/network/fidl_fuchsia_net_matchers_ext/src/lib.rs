// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Extensions for the fuchsia.net.matchers FIDL library.
//!
//! Note that this library as written is not meant for inclusion in the SDK. It
//! is only meant to be used in conjunction with a netstack that is compiled
//! against the same API level of the `fuchsia.net.matchers` FIDL library. This
//! library opts in to compile-time and runtime breakage when the FIDL library
//! is evolved in order to enforce that it is updated along with the FIDL
//! library itself.

use std::fmt::Debug;
use std::num::NonZeroU64;
use std::ops::RangeInclusive;

use fidl::marker::SourceBreaking;
use fidl_fuchsia_net_ext::IntoExt;
use thiserror::Error;
use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_interfaces as fnet_interfaces,
    fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext,
    fidl_fuchsia_net_matchers as fnet_matchers,
};

/// Extension type for [`fnet_matchers::Interface`].
#[derive(Debug, Clone, PartialEq)]
pub enum Interface {
    Id(NonZeroU64),
    Name(fnet_interfaces::Name),
    PortClass(fnet_interfaces_ext::PortClass),
}

/// Errors when creating an [`Interface`].
#[derive(Debug, Error, PartialEq)]
pub enum InterfaceError {
    #[error("interface matcher specified an invalid ID of 0")]
    ZeroId,
    #[error(transparent)]
    UnknownPortClass(fnet_interfaces_ext::UnknownPortClassError),
    #[error("interface union is of an unknown variant")]
    UnknownUnionVariant,
}

impl From<Interface> for fnet_matchers::Interface {
    fn from(matcher: Interface) -> Self {
        match matcher {
            Interface::Id(id) => Self::Id(id.get()),
            Interface::Name(name) => Self::Name(name),
            Interface::PortClass(port_class) => Self::PortClass(port_class.into()),
        }
    }
}

impl TryFrom<fnet_matchers::Interface> for Interface {
    type Error = InterfaceError;

    fn try_from(matcher: fnet_matchers::Interface) -> Result<Self, Self::Error> {
        match matcher {
            fnet_matchers::Interface::Id(id) => {
                let id = NonZeroU64::new(id).ok_or(InterfaceError::ZeroId)?;
                Ok(Self::Id(id))
            }
            fnet_matchers::Interface::Name(name) => Ok(Self::Name(name)),
            fnet_matchers::Interface::PortClass(port_class) => {
                port_class.try_into().map(Self::PortClass).map_err(InterfaceError::UnknownPortClass)
            }
            fnet_matchers::Interface::__SourceBreaking { .. } => {
                Err(InterfaceError::UnknownUnionVariant)
            }
        }
    }
}

/// Extension type for the `Subnet` variant of [`fnet_matchers::Address`].
///
/// This type witnesses to the invariant that the prefix length of the subnet is
/// no greater than the number of bits in the IP address, and that no host bits
/// in the network address are set.
#[derive(Clone, Copy, Eq, Hash, PartialEq)]
pub struct Subnet(fnet::Subnet);

/// Errors when creating a [`Subnet`].
#[derive(Debug, Error, PartialEq)]
pub enum SubnetError {
    #[error("prefix length of subnet is longer than number of bits in IP address")]
    PrefixTooLong,
    #[error("host bits are set in subnet network")]
    HostBitsSet,
}

impl Subnet {
    pub fn get(&self) -> fnet::Subnet {
        let Subnet(subnet) = &self;
        *subnet
    }
}

impl From<Subnet> for fnet::Subnet {
    fn from(subnet: Subnet) -> Self {
        let Subnet(subnet) = subnet;
        subnet
    }
}

impl TryFrom<fnet::Subnet> for Subnet {
    type Error = SubnetError;

    fn try_from(subnet: fnet::Subnet) -> Result<Self, Self::Error> {
        let fnet::Subnet { addr, prefix_len } = subnet;

        // We convert to `net_types::ip::Subnet` to validate the subnet's
        // properties, but we don't store the subnet as that type because we
        // want to avoid forcing all `Resource` types in this library to be
        // parameterized on IP version.
        let result = match addr {
            fnet::IpAddress::Ipv4(v4) => {
                net_types::ip::Subnet::<net_types::ip::Ipv4Addr>::new(v4.into_ext(), prefix_len)
                    .map(|_| Subnet(subnet))
            }
            fnet::IpAddress::Ipv6(v6) => {
                net_types::ip::Subnet::<net_types::ip::Ipv6Addr>::new(v6.into_ext(), prefix_len)
                    .map(|_| Subnet(subnet))
            }
        };
        result.map_err(|e| match e {
            net_types::ip::SubnetError::PrefixTooLong => SubnetError::PrefixTooLong,
            net_types::ip::SubnetError::HostBitsSet => SubnetError::HostBitsSet,
        })
    }
}

impl Debug for Subnet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let fnet::Subnet { addr, prefix_len } = self.0;

        match addr {
            fnet::IpAddress::Ipv4(v4) => {
                let subnet = net_types::ip::Subnet::<net_types::ip::Ipv4Addr>::new(
                    v4.into_ext(),
                    prefix_len,
                );

                match subnet {
                    Ok(inner) => inner.fmt(f),
                    Err(err) => err.fmt(f),
                }
            }
            fnet::IpAddress::Ipv6(v6) => {
                let subnet = net_types::ip::Subnet::<net_types::ip::Ipv6Addr>::new(
                    v6.into_ext(),
                    prefix_len,
                );

                match subnet {
                    Ok(inner) => inner.fmt(f),
                    Err(err) => err.fmt(f),
                }
            }
        }
    }
}

/// Extension type for [`fnet_matchers::AddressRange`].
///
/// This type witnesses to the invariant that `start` is in the same IP family
/// as `end`, and that `start <= end`. (Comparisons are performed on the
/// numerical big-endian representation of the IP address.)
#[derive(Debug, Clone, PartialEq)]
pub struct AddressRange {
    range: RangeInclusive<fnet::IpAddress>,
}

/// Errors when creating an [`AddressRange`].
#[derive(Debug, Error, PartialEq)]
pub enum AddressRangeError {
    #[error("invalid address range (start must be <= end)")]
    Invalid,
    #[error("address range start and end addresses are not the same IP family")]
    FamilyMismatch,
}

impl AddressRange {
    pub fn start(&self) -> fnet::IpAddress {
        *self.range.start()
    }

    pub fn end(&self) -> fnet::IpAddress {
        *self.range.end()
    }
}

impl From<AddressRange> for fnet_matchers::AddressRange {
    fn from(range: AddressRange) -> Self {
        Self { start: range.start(), end: range.end() }
    }
}

impl TryFrom<fnet_matchers::AddressRange> for AddressRange {
    type Error = AddressRangeError;

    fn try_from(range: fnet_matchers::AddressRange) -> Result<Self, Self::Error> {
        let fnet_matchers::AddressRange { start, end } = range;
        match (start, end) {
            (
                fnet::IpAddress::Ipv4(fnet::Ipv4Address { addr: start_bytes }),
                fnet::IpAddress::Ipv4(fnet::Ipv4Address { addr: end_bytes }),
            ) => {
                if u32::from_be_bytes(start_bytes) > u32::from_be_bytes(end_bytes) {
                    Err(AddressRangeError::Invalid)
                } else {
                    Ok(Self { range: start..=end })
                }
            }
            (
                fnet::IpAddress::Ipv6(fnet::Ipv6Address { addr: start_bytes }),
                fnet::IpAddress::Ipv6(fnet::Ipv6Address { addr: end_bytes }),
            ) => {
                if u128::from_be_bytes(start_bytes) > u128::from_be_bytes(end_bytes) {
                    Err(AddressRangeError::Invalid)
                } else {
                    Ok(Self { range: start..=end })
                }
            }
            _ => Err(AddressRangeError::FamilyMismatch),
        }
    }
}

/// Extension type for [`fnet_matchers::Address`].
#[derive(Clone, PartialEq)]
pub enum AddressMatcherType {
    Subnet(Subnet),
    Range(AddressRange),
}

/// Errors when creating an [`AddressMatcherType`].
#[derive(Debug, Error, PartialEq)]
pub enum AddressMatcherTypeError {
    #[error("AddressMatcher is of an unknown variant")]
    UnknownUnionVariant,
    #[error("subnet conversion error: {0}")]
    Subnet(SubnetError),
    #[error("address range conversion error: {0}")]
    AddressRange(AddressRangeError),
}

impl From<SubnetError> for AddressMatcherTypeError {
    fn from(value: SubnetError) -> Self {
        AddressMatcherTypeError::Subnet(value)
    }
}
impl From<AddressRangeError> for AddressMatcherTypeError {
    fn from(value: AddressRangeError) -> Self {
        AddressMatcherTypeError::AddressRange(value)
    }
}

impl From<AddressMatcherType> for fnet_matchers::AddressMatcherType {
    fn from(matcher: AddressMatcherType) -> Self {
        match matcher {
            AddressMatcherType::Subnet(subnet) => Self::Subnet(subnet.into()),
            AddressMatcherType::Range(range) => Self::Range(range.into()),
        }
    }
}

impl TryFrom<fnet_matchers::AddressMatcherType> for AddressMatcherType {
    type Error = AddressMatcherTypeError;

    fn try_from(matcher: fnet_matchers::AddressMatcherType) -> Result<Self, Self::Error> {
        match matcher {
            fnet_matchers::AddressMatcherType::Subnet(subnet) => {
                Ok(Self::Subnet(subnet.try_into()?))
            }
            fnet_matchers::AddressMatcherType::Range(range) => Ok(Self::Range(range.try_into()?)),
            fnet_matchers::AddressMatcherType::__SourceBreaking { .. } => {
                Err(AddressMatcherTypeError::UnknownUnionVariant)
            }
        }
    }
}

impl Debug for AddressMatcherType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AddressMatcherType::Subnet(subnet) => subnet.fmt(f),
            AddressMatcherType::Range(address_range) => address_range.fmt(f),
        }
    }
}

/// Extension type for [`fnet_matchers::Address`].
#[derive(Debug, Clone, PartialEq)]
pub struct Address {
    pub matcher: AddressMatcherType,
    pub invert: bool,
}

/// Errors when creating an [`Address`].
#[derive(Debug, Error, PartialEq)]
pub enum AddressError {
    #[error("address matcher conversion failure: {0}")]
    AddressMatcherType(AddressMatcherTypeError),
}

impl From<AddressMatcherTypeError> for AddressError {
    fn from(value: AddressMatcherTypeError) -> Self {
        Self::AddressMatcherType(value)
    }
}

impl From<Address> for fnet_matchers::Address {
    fn from(matcher: Address) -> Self {
        let Address { matcher, invert } = matcher;
        Self { matcher: matcher.into(), invert }
    }
}

impl TryFrom<fnet_matchers::Address> for Address {
    type Error = AddressError;

    fn try_from(matcher: fnet_matchers::Address) -> Result<Self, Self::Error> {
        let fnet_matchers::Address { matcher, invert } = matcher;
        Ok(Self { matcher: matcher.try_into()?, invert })
    }
}

/// Extension type for [`fnet_matchers::Port`].
///
/// This type witnesses to the invariant that `start <= end`.
#[derive(Debug, Clone, PartialEq)]
pub struct Port {
    range: RangeInclusive<u16>,
    pub invert: bool,
}

/// Errors when creating a `Port`.
#[derive(Debug, Error, PartialEq)]
pub enum PortError {
    #[error("invalid port range (start must be <= end)")]
    InvalidPortRange,
}

impl Port {
    pub fn new(start: u16, end: u16, invert: bool) -> Result<Self, PortError> {
        if start > end {
            return Err(PortError::InvalidPortRange);
        }
        Ok(Self { range: start..=end, invert })
    }

    pub fn range(&self) -> &RangeInclusive<u16> {
        &self.range
    }

    pub fn start(&self) -> u16 {
        *self.range.start()
    }

    pub fn end(&self) -> u16 {
        *self.range.end()
    }
}

impl From<Port> for fnet_matchers::Port {
    fn from(matcher: Port) -> Self {
        let Port { range, invert } = matcher;
        Self { start: *range.start(), end: *range.end(), invert }
    }
}

impl TryFrom<fnet_matchers::Port> for Port {
    type Error = PortError;

    fn try_from(matcher: fnet_matchers::Port) -> Result<Self, Self::Error> {
        let fnet_matchers::Port { start, end, invert } = matcher;
        if start > end {
            return Err(PortError::InvalidPortRange);
        }
        Ok(Self { range: start..=end, invert })
    }
}

/// Extension type for [`fnet_matchers::PacketTransportProtocol`].
#[derive(Clone, PartialEq)]
pub enum TransportProtocol {
    Tcp { src_port: Option<Port>, dst_port: Option<Port> },
    Udp { src_port: Option<Port>, dst_port: Option<Port> },
    Icmp,
    Icmpv6,
}

/// Errors when creating a [`TransportProtocol`].
#[derive(Debug, Error, PartialEq)]
pub enum TransportProtocolError {
    #[error("invalid port: {0}")]
    Port(PortError),
    #[error("TransportProtocol is of an unknown variant")]
    UnknownUnionVariant,
}

impl From<PortError> for TransportProtocolError {
    fn from(value: PortError) -> Self {
        TransportProtocolError::Port(value)
    }
}

impl From<TransportProtocol> for fnet_matchers::PacketTransportProtocol {
    fn from(matcher: TransportProtocol) -> Self {
        match matcher {
            TransportProtocol::Tcp { src_port, dst_port } => Self::Tcp(fnet_matchers::TcpPacket {
                src_port: src_port.map(Into::into),
                dst_port: dst_port.map(Into::into),
                __source_breaking: SourceBreaking,
            }),
            TransportProtocol::Udp { src_port, dst_port } => Self::Udp(fnet_matchers::UdpPacket {
                src_port: src_port.map(Into::into),
                dst_port: dst_port.map(Into::into),
                __source_breaking: SourceBreaking,
            }),
            TransportProtocol::Icmp => Self::Icmp(fnet_matchers::IcmpPacket::default()),
            TransportProtocol::Icmpv6 => Self::Icmpv6(fnet_matchers::Icmpv6Packet::default()),
        }
    }
}

impl TryFrom<fnet_matchers::PacketTransportProtocol> for TransportProtocol {
    type Error = TransportProtocolError;

    fn try_from(matcher: fnet_matchers::PacketTransportProtocol) -> Result<Self, Self::Error> {
        match matcher {
            fnet_matchers::PacketTransportProtocol::Tcp(fnet_matchers::TcpPacket {
                src_port,
                dst_port,
                __source_breaking,
            }) => Ok(Self::Tcp {
                src_port: src_port.map(TryInto::try_into).transpose()?,
                dst_port: dst_port.map(TryInto::try_into).transpose()?,
            }),
            fnet_matchers::PacketTransportProtocol::Udp(fnet_matchers::UdpPacket {
                src_port,
                dst_port,
                __source_breaking,
            }) => Ok(Self::Udp {
                src_port: src_port.map(TryInto::try_into).transpose()?,
                dst_port: dst_port.map(TryInto::try_into).transpose()?,
            }),
            fnet_matchers::PacketTransportProtocol::Icmp(fnet_matchers::IcmpPacket {
                __source_breaking,
            }) => Ok(Self::Icmp),
            fnet_matchers::PacketTransportProtocol::Icmpv6(fnet_matchers::Icmpv6Packet {
                __source_breaking,
            }) => Ok(Self::Icmpv6),
            fnet_matchers::PacketTransportProtocol::__SourceBreaking { .. } => {
                Err(TransportProtocolError::UnknownUnionVariant)
            }
        }
    }
}

impl Debug for TransportProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Omit empty fields.
        match self {
            TransportProtocol::Tcp { src_port, dst_port } => {
                let mut debug_struct = f.debug_struct("Tcp");

                // Omit empty fields.
                if let Some(port) = &src_port {
                    let _ = debug_struct.field("src_port", port);
                }

                if let Some(port) = &dst_port {
                    let _ = debug_struct.field("dst_port", port);
                }

                debug_struct.finish()
            }
            TransportProtocol::Udp { src_port, dst_port } => {
                let mut debug_struct = f.debug_struct("Udp");

                // Omit empty fields.
                if let Some(port) = &src_port {
                    let _ = debug_struct.field("src_port", port);
                }

                if let Some(port) = &dst_port {
                    let _ = debug_struct.field("dst_port", port);
                }

                debug_struct.finish()
            }
            TransportProtocol::Icmp => f.write_str("Icmp"),
            TransportProtocol::Icmpv6 => f.write_str("Icmpv6"),
        }
    }
}

#[cfg(test)]
mod tests {
    use net_declare::{fidl_ip, fidl_subnet};
    use test_case::test_case;

    use super::*;

    #[test_case(
        fnet_matchers::Interface::Id(1),
        Interface::Id(NonZeroU64::new(1).unwrap());
        "Interface"
    )]
    #[test_case(
        fnet_matchers::AddressMatcherType::Subnet(fidl_subnet!("192.0.2.0/24")),
        AddressMatcherType::Subnet(Subnet(fidl_subnet!("192.0.2.0/24")));
        "AddressMatcherType"
    )]
    #[test_case(
        fnet_matchers::Address {
            matcher: fnet_matchers::AddressMatcherType::Subnet(fidl_subnet!("192.0.2.0/24")),
            invert: true,
        },
        Address {
            matcher: AddressMatcherType::Subnet(Subnet(fidl_subnet!("192.0.2.0/24"))),
            invert: true,
        };
        "Address"
    )]
    #[test_case(
        fnet_matchers::AddressRange {
            start: fidl_ip!("192.0.2.0"),
            end: fidl_ip!("192.0.2.1"),
        },
        AddressRange {
            range: fidl_ip!("192.0.2.0")..=fidl_ip!("192.0.2.1"),
        };
        "AddressRange"
    )]
    #[test_case(
        fnet_matchers::PacketTransportProtocol::Udp(fnet_matchers::UdpPacket {
            src_port: Some(fnet_matchers::Port { start: 1024, end: u16::MAX, invert: false }),
            dst_port: None,
            ..Default::default()
        }),
        TransportProtocol::Udp {
            src_port: Some(Port { range: 1024..=u16::MAX, invert: false }),
            dst_port: None,
        };
        "TransportProtocol"
    )]
    fn convert_from_fidl_and_back<F, E>(fidl_type: F, local_type: E)
    where
        E: TryFrom<F> + Clone + Debug + PartialEq,
        <E as TryFrom<F>>::Error: Debug + PartialEq,
        F: From<E> + Clone + Debug + PartialEq,
    {
        assert_eq!(fidl_type.clone().try_into(), Ok(local_type.clone()));
        assert_eq!(<_ as Into<F>>::into(local_type), fidl_type.clone());
    }

    #[test]
    fn interface_matcher_try_from_unknown_variant() {
        assert_eq!(
            Interface::try_from(fnet_matchers::Interface::__SourceBreaking { unknown_ordinal: 0 }),
            Err(InterfaceError::UnknownUnionVariant)
        );
    }

    #[test]
    fn interface_matcher_try_from_invalid() {
        assert_eq!(
            Interface::try_from(fnet_matchers::Interface::Id(0)),
            Err(InterfaceError::ZeroId)
        );
    }

    #[test]
    fn address_matcher_type_try_from_unknown_variant() {
        assert_eq!(
            AddressMatcherType::try_from(fnet_matchers::AddressMatcherType::__SourceBreaking {
                unknown_ordinal: 0
            }),
            Err(AddressMatcherTypeError::UnknownUnionVariant)
        );
    }

    #[test]
    fn subnet_try_from_invalid() {
        assert_eq!(
            Subnet::try_from(fnet::Subnet { addr: fidl_ip!("192.0.2.1"), prefix_len: 33 }),
            Err(SubnetError::PrefixTooLong)
        );
        assert_eq!(Subnet::try_from(fidl_subnet!("192.0.2.1/24")), Err(SubnetError::HostBitsSet));
    }

    #[test]
    fn address_range_try_from_invalid() {
        assert_eq!(
            AddressRange::try_from(fnet_matchers::AddressRange {
                start: fidl_ip!("192.0.2.1"),
                end: fidl_ip!("192.0.2.0"),
            }),
            Err(AddressRangeError::Invalid)
        );
        assert_eq!(
            AddressRange::try_from(fnet_matchers::AddressRange {
                start: fidl_ip!("2001:db8::1"),
                end: fidl_ip!("2001:db8::"),
            }),
            Err(AddressRangeError::Invalid)
        );
    }

    #[test]
    fn address_range_try_from_family_mismatch() {
        assert_eq!(
            AddressRange::try_from(fnet_matchers::AddressRange {
                start: fidl_ip!("192.0.2.0"),
                end: fidl_ip!("2001:db8::"),
            }),
            Err(AddressRangeError::FamilyMismatch)
        );
    }

    #[test]
    fn port_matcher_try_from_invalid() {
        assert_eq!(
            Port::try_from(fnet_matchers::Port { start: 1, end: 0, invert: false }),
            Err(PortError::InvalidPortRange)
        );
    }

    #[test]
    fn transport_protocol_try_from_unknown_variant() {
        assert_eq!(
            TransportProtocol::try_from(fnet_matchers::PacketTransportProtocol::__SourceBreaking {
                unknown_ordinal: 0
            }),
            Err(TransportProtocolError::UnknownUnionVariant)
        );
    }
}
