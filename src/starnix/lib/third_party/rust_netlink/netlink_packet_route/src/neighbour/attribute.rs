// SPDX-License-Identifier: MIT

use super::{NeighbourAddress, NeighbourCacheInfo, NeighbourCacheInfoBuffer, NeighbourError};
use crate::route::RouteProtocol;
use crate::AddressFamily;
use byteorder::{BigEndian, ByteOrder, NativeEndian};
use netlink_packet_utils::nla::{DefaultNla, Nla, NlaBuffer};
use netlink_packet_utils::parsers::{parse_u16, parse_u16_be, parse_u32};
use netlink_packet_utils::{Emitable, Parseable, ParseableParametrized};

const NDA_DST: u16 = 1;
const NDA_LLADDR: u16 = 2;
const NDA_CACHEINFO: u16 = 3;
const NDA_PROBES: u16 = 4;
const NDA_VLAN: u16 = 5;
const NDA_PORT: u16 = 6;
const NDA_VNI: u16 = 7;
const NDA_IFINDEX: u16 = 8;
// Kernel constant name is NDA_MASTER
const NDA_CONTROLLER: u16 = 9;
const NDA_LINK_NETNSID: u16 = 10;
const NDA_SRC_VNI: u16 = 11;
const NDA_PROTOCOL: u16 = 12;
// const NDA_NH_ID: u16 = 13;
// const NDA_FDB_EXT_ATTRS: u16 = 14;

#[derive(Debug, PartialEq, Eq, Clone)]
#[non_exhaustive]
pub enum NeighbourAttribute {
    Destination(NeighbourAddress),
    LinkLocalAddress(Vec<u8>),
    CacheInfo(NeighbourCacheInfo),
    Probes(u32),
    Vlan(u16),
    Port(u16),
    Vni(u32),
    IfIndex(u32),
    Controller(u32),
    LinkNetNsId(u32),
    SourceVni(u32),
    Protocol(RouteProtocol),
    Other(DefaultNla),
}

impl Nla for NeighbourAttribute {
    fn value_len(&self) -> usize {
        match self {
            Self::LinkLocalAddress(bytes) => bytes.len(),
            Self::Destination(v) => v.buffer_len(),
            Self::CacheInfo(v) => v.buffer_len(),
            Self::Vlan(_) | Self::Port(_) => 2,
            Self::Protocol(v) => v.buffer_len(),
            Self::Probes(_)
            | Self::LinkNetNsId(_)
            | Self::Controller(_)
            | Self::Vni(_)
            | Self::IfIndex(_)
            | Self::SourceVni(_) => 4,
            Self::Other(attr) => attr.value_len(),
        }
    }

    fn emit_value(&self, buffer: &mut [u8]) {
        match self {
            Self::Destination(v) => v.emit(buffer),
            Self::LinkLocalAddress(bytes) => buffer.copy_from_slice(bytes.as_slice()),
            Self::CacheInfo(v) => v.emit(buffer),
            Self::Vlan(value) => NativeEndian::write_u16(buffer, *value),
            Self::Port(value) => BigEndian::write_u16(buffer, *value),
            Self::Probes(value)
            | Self::LinkNetNsId(value)
            | Self::Controller(value)
            | Self::Vni(value)
            | Self::IfIndex(value)
            | Self::SourceVni(value) => NativeEndian::write_u32(buffer, *value),
            Self::Protocol(v) => v.emit(buffer),
            Self::Other(attr) => attr.emit_value(buffer),
        }
    }

    fn kind(&self) -> u16 {
        match self {
            Self::Destination(_) => NDA_DST,
            Self::LinkLocalAddress(_) => NDA_LLADDR,
            Self::CacheInfo(_) => NDA_CACHEINFO,
            Self::Probes(_) => NDA_PROBES,
            Self::Vlan(_) => NDA_VLAN,
            Self::Port(_) => NDA_PORT,
            Self::Vni(_) => NDA_VNI,
            Self::IfIndex(_) => NDA_IFINDEX,
            Self::Controller(_) => NDA_CONTROLLER,
            Self::LinkNetNsId(_) => NDA_LINK_NETNSID,
            Self::SourceVni(_) => NDA_SRC_VNI,
            Self::Protocol(_) => NDA_PROTOCOL,
            Self::Other(nla) => nla.kind(),
        }
    }
}

impl<'a, T: AsRef<[u8]> + ?Sized> ParseableParametrized<NlaBuffer<&'a T>, AddressFamily>
    for NeighbourAttribute
{
    type Error = NeighbourError;
    fn parse_with_param(
        buf: &NlaBuffer<&'a T>,
        address_family: AddressFamily,
    ) -> Result<Self, Self::Error> {
        let payload = buf.value();
        Ok(match buf.kind() {
            NDA_DST => Self::Destination(
                NeighbourAddress::parse_with_param(address_family, payload)
                    .map_err(|error| NeighbourError::InvalidValue { kind: "NDA_DST", error })?,
            ),
            NDA_LLADDR => Self::LinkLocalAddress(payload.to_vec()),
            NDA_CACHEINFO => Self::CacheInfo(
                NeighbourCacheInfoBuffer::new_checked(payload)
                    .and_then(|buffer| NeighbourCacheInfo::parse(&buffer))
                    .map_err(|error| NeighbourError::InvalidValue {
                        kind: "NDA_CACHEINFO",
                        error,
                    })?,
            ),
            NDA_PROBES => Self::Probes(
                parse_u32(payload)
                    .map_err(|error| NeighbourError::InvalidValue { kind: "NDA_PROBES", error })?,
            ),
            NDA_VLAN => Self::Vlan(
                parse_u16(payload)
                    .map_err(|error| NeighbourError::InvalidValue { kind: "NDA_VLAN", error })?,
            ),
            NDA_PORT => Self::Port(
                parse_u16_be(payload)
                    .map_err(|error| NeighbourError::InvalidValue { kind: "NDA_PORT", error })?,
            ),
            NDA_VNI => Self::Vni(
                parse_u32(payload)
                    .map_err(|error| NeighbourError::InvalidValue { kind: "NDA_VNI", error })?,
            ),
            NDA_IFINDEX => Self::IfIndex(
                parse_u32(payload)
                    .map_err(|error| NeighbourError::InvalidValue { kind: "NDA_IFINDEX", error })?,
            ),
            NDA_CONTROLLER => {
                Self::Controller(parse_u32(payload).map_err(|error| {
                    NeighbourError::InvalidValue { kind: "NDA_CONTROLLER", error }
                })?)
            }
            NDA_LINK_NETNSID => Self::LinkNetNsId(parse_u32(payload).map_err(|error| {
                NeighbourError::InvalidValue { kind: "NDA_LINK_NETNSID", error }
            })?),
            NDA_SRC_VNI => Self::SourceVni(
                parse_u32(payload)
                    .map_err(|error| NeighbourError::InvalidValue { kind: "NDA_SRC_VNI", error })?,
            ),
            NDA_PROTOCOL => Self::Protocol(RouteProtocol::parse(payload)?),
            kind => Self::Other(
                DefaultNla::parse(buf)
                    .map_err(|error| NeighbourError::UnknownNLA { kind, error })?,
            ),
        })
    }
}
