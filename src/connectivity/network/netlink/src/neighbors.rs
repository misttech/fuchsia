// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A module for managing RTM_NEIGHBOR information by receiving RTM_NEIGHBOR
//! Netlink messages and maintaining neighbor table state from Netstack.

use std::net::IpAddr;

use crate::logging::log_warn;
use netlink_packet_core::{NLM_F_MULTIPART, NetlinkMessage};
use netlink_packet_route::neighbour::{
    NeighbourAttribute, NeighbourHeader, NeighbourMessage, NeighbourState,
};
use netlink_packet_route::route::RouteType;
use netlink_packet_route::{AddressFamily, RouteNetlinkMessage};

use {
    fidl_fuchsia_net_ext as fnet_ext, fidl_fuchsia_net_neighbor as fnet_neighbor,
    fidl_fuchsia_net_neighbor_ext as fnet_neighbor_ext,
};

/// NetlinkNeighborMessage conversion related errors.
#[derive(Debug, PartialEq)]
pub(crate) enum NetlinkNeighborMessageConversionError {
    /// Interface id could not be downcasted to fit into the expected u32.
    InvalidInterfaceId(u64),
}

/// A wrapper type for the netlink_packet_route `NeighbourMessage` to enable conversions
/// from [`fnet_neighbor_ext::Entry`].
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NetlinkNeighborMessage(pub(crate) NeighbourMessage);

impl NetlinkNeighborMessage {
    pub(crate) fn optionally_from(
        neighbor: fnet_neighbor_ext::Entry,
    ) -> Option<NetlinkNeighborMessage> {
        match neighbor.try_into() {
            Ok(message) => Some(message),
            Err(NetlinkNeighborMessageConversionError::InvalidInterfaceId(id)) => {
                log_warn!("Invalid interface id found in neighbor table entry: {}", id);
                None
            }
        }
    }

    /// Wrap the inner [`NeighbourMessage`] in an [`RtnlMessage::NewNeighbour`].
    pub(crate) fn into_rtnl_new_neighbor(
        self,
        sequence_number: u32,
        is_dump: bool,
    ) -> NetlinkMessage<RouteNetlinkMessage> {
        let NetlinkNeighborMessage(message) = self;
        let mut msg: NetlinkMessage<RouteNetlinkMessage> =
            RouteNetlinkMessage::NewNeighbour(message).into();
        msg.header.sequence_number = sequence_number;
        if is_dump {
            msg.header.flags |= NLM_F_MULTIPART;
        }
        msg.finalize();
        msg
    }
}

impl TryFrom<fnet_neighbor_ext::Entry> for NetlinkNeighborMessage {
    type Error = NetlinkNeighborMessageConversionError;

    fn try_from(
        neighbor: fnet_neighbor_ext::Entry,
    ) -> Result<NetlinkNeighborMessage, NetlinkNeighborMessageConversionError> {
        let mut header = NeighbourHeader::default();
        let fnet_ext::IpAddress(addr) = neighbor.neighbor.into();
        header.family = match addr {
            IpAddr::V4(_) => AddressFamily::Inet,
            IpAddr::V6(_) => AddressFamily::Inet6,
        };
        header.ifindex = neighbor.interface.try_into().map_err(|_| {
            NetlinkNeighborMessageConversionError::InvalidInterfaceId(neighbor.interface)
        })?;
        header.state = match neighbor.state {
            fnet_neighbor::EntryState::Delay => NeighbourState::Delay,
            fnet_neighbor::EntryState::Incomplete => NeighbourState::Incomplete,
            fnet_neighbor::EntryState::Probe => NeighbourState::Probe,
            fnet_neighbor::EntryState::Reachable => NeighbourState::Reachable,
            fnet_neighbor::EntryState::Stale => NeighbourState::Stale,
            fnet_neighbor::EntryState::Static => NeighbourState::Permanent,
            fnet_neighbor::EntryState::Unreachable => NeighbourState::Failed,
        };
        // TODO(https://fxbug.dev/285127384): Can this sometimes be inferred from `addr`?
        header.kind = RouteType::Unspec;

        let mut attributes = vec![];
        attributes.push(NeighbourAttribute::Destination(match addr {
            IpAddr::V4(addr) => addr.into(),
            IpAddr::V6(addr) => addr.into(),
        }));
        if let Some(mac) = neighbor.mac {
            attributes.push(NeighbourAttribute::LinkLocalAddress(mac.octets.into()));
        }
        // TODO(https://fxbug.dev/285127384): Determine whether it's necessary
        // to populate `CacheInfo`.

        let mut msg = NeighbourMessage::default();
        msg.header = header;
        msg.attributes = attributes;
        Ok(NetlinkNeighborMessage(msg))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use fidl_fuchsia_net as fnet;
    use net_declare::{fidl_ip, std_ip_v4, std_ip_v6};
    use netlink_packet_core::NetlinkPayload;
    use netlink_packet_route::neighbour::{NeighbourAddress, NeighbourFlags};
    use test_case::test_case;

    fn valid_neighbor_entry() -> fnet_neighbor_ext::Entry {
        fnet_neighbor_ext::Entry {
            interface: 1,
            neighbor: fidl_ip!("192.168.0.1"),
            state: fnet_neighbor::EntryState::Reachable,
            mac: Some(fnet::MacAddress { octets: [0, 1, 2, 3, 4, 5] }),
            updated_at: 123456,
        }
    }

    #[test]
    fn netlink_neighbor_message_from_entry_invalid_iface_id() {
        let entry = fnet_neighbor_ext::Entry { interface: u64::MAX, ..valid_neighbor_entry() };

        assert_eq!(
            NetlinkNeighborMessage::try_from(entry),
            Err(NetlinkNeighborMessageConversionError::InvalidInterfaceId(u64::MAX))
        );
    }

    #[test]
    fn netlink_neighbor_message_from_entry_valid_iface_id() {
        assert_matches!(
            NetlinkNeighborMessage::try_from(fnet_neighbor_ext::Entry {
                interface: 1,
                ..valid_neighbor_entry()
            }),
            Ok(NetlinkNeighborMessage(NeighbourMessage {
                header: NeighbourHeader { ifindex: 1, .. },
                ..
            }))
        );
    }

    #[test_case(fnet_neighbor::EntryState::Delay, NeighbourState::Delay; "delay")]
    #[test_case(fnet_neighbor::EntryState::Incomplete, NeighbourState::Incomplete; "incomplete")]
    #[test_case(fnet_neighbor::EntryState::Probe, NeighbourState::Probe; "probe")]
    #[test_case(fnet_neighbor::EntryState::Reachable, NeighbourState::Reachable; "reachable")]
    #[test_case(fnet_neighbor::EntryState::Stale, NeighbourState::Stale; "stale")]
    #[test_case(fnet_neighbor::EntryState::Static, NeighbourState::Permanent; "permanent")]
    #[test_case(fnet_neighbor::EntryState::Unreachable, NeighbourState::Failed; "failed")]
    fn netlink_neighbor_message_from_entry_state_converted(
        fidl_state: fnet_neighbor::EntryState,
        expected: NeighbourState,
    ) {
        assert_matches!(
            NetlinkNeighborMessage::try_from(fnet_neighbor_ext::Entry {
                state: fidl_state,
                ..valid_neighbor_entry()
            }),
            Ok(NetlinkNeighborMessage(NeighbourMessage {
                header: NeighbourHeader { state, .. },
                ..
            })) if state == expected
        );
    }

    #[test]
    fn netlink_neighbor_message_from_entry_ipv4() {
        let fidl_entry = fnet_neighbor_ext::Entry {
            neighbor: fidl_ip!("192.168.0.1"),
            ..valid_neighbor_entry()
        };
        let NetlinkNeighborMessage(message) =
            fidl_entry.try_into().expect("should be able to convert valid neighbor entry");

        assert_eq!(message.header.family, AddressFamily::Inet);
        let expected_address: NeighbourAddress = std_ip_v4!("192.168.0.1").into();
        assert_matches!(
            &message.attributes[..],
            [
                NeighbourAttribute::Destination(address),
                NeighbourAttribute::LinkLocalAddress(_)
            ] if *address == expected_address
        );
    }

    #[test]
    fn netlink_neighbor_message_from_entry_ipv6() {
        let fidl_entry =
            fnet_neighbor_ext::Entry { neighbor: fidl_ip!("fe80::1"), ..valid_neighbor_entry() };
        let NetlinkNeighborMessage(message) =
            fidl_entry.try_into().expect("should be able to convert valid neighbor entry");

        assert_eq!(message.header.family, AddressFamily::Inet6);
        let expected_address: NeighbourAddress = std_ip_v6!("fe80::1").into();
        assert_matches!(
            &message.attributes[..],
            [
                NeighbourAttribute::Destination(address),
                NeighbourAttribute::LinkLocalAddress(_)
            ] if *address == expected_address
        );
    }

    #[test]
    fn netlink_neighbor_message_from_entry_address_link_local_present() {
        let fidl_entry = fnet_neighbor_ext::Entry {
            mac: Some(fnet::MacAddress { octets: [0, 1, 2, 3, 4, 5] }),
            ..valid_neighbor_entry()
        };
        let NetlinkNeighborMessage(message) =
            fidl_entry.try_into().expect("should be able to convert valid neighbor entry");

        assert_matches!(
            &message.attributes[..],
            [
                NeighbourAttribute::Destination(_),
                NeighbourAttribute::LinkLocalAddress(addr)
            ] if addr == &[0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn netlink_neighbor_message_from_entry_address_link_local_absent() {
        let fidl_entry = fnet_neighbor_ext::Entry { mac: None, ..valid_neighbor_entry() };
        let NetlinkNeighborMessage(message) =
            fidl_entry.try_into().expect("should be able to convert valid neighbor entry");

        assert_matches!(&message.attributes[..], [NeighbourAttribute::Destination(_)]);
    }

    #[test]
    fn netlink_neighbor_message_optionally_from_failure() {
        assert_eq!(
            NetlinkNeighborMessage::optionally_from(fnet_neighbor_ext::Entry {
                interface: u64::MAX,
                ..valid_neighbor_entry()
            }),
            None
        );
    }

    #[test]
    fn netlink_neighbor_message_optionally_from_success() {
        let fidl_entry = fnet_neighbor_ext::Entry {
            interface: 1,
            neighbor: fidl_ip!("192.168.0.1"),
            state: fnet_neighbor::EntryState::Reachable,
            mac: None,
            updated_at: 123456,
        };

        let mut expected_message = NeighbourMessage::default();
        expected_message.header = NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            state: NeighbourState::Reachable,
            flags: NeighbourFlags::empty(),
            kind: RouteType::Unspec,
        };
        expected_message.attributes =
            vec![NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into())];

        assert_eq!(
            NetlinkNeighborMessage::optionally_from(fidl_entry),
            Some(NetlinkNeighborMessage(expected_message))
        );
    }

    #[test]
    fn netlink_neighbor_message_into_rtnl_new_neighbor() {
        let message: NetlinkNeighborMessage = valid_neighbor_entry()
            .try_into()
            .expect("should be able to convert valid neighbor entry");
        let NetlinkNeighborMessage(payload) = &message;

        let expected_payload =
            NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(payload.clone()));

        let result = message.clone().into_rtnl_new_neighbor(1, true);
        assert_eq!(result.payload, expected_payload);
        assert_eq!(result.header.sequence_number, 1);
        assert_eq!(result.header.flags & NLM_F_MULTIPART, NLM_F_MULTIPART);

        let result = message.into_rtnl_new_neighbor(1, false);
        assert_eq!(result.payload, expected_payload);
        assert_ne!(result.header.flags & NLM_F_MULTIPART, NLM_F_MULTIPART);
    }
}
