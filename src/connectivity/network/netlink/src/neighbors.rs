// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A module for managing neighbor information by receiving RTM_*NEIGH Netlink
//! messages and maintaining neighbor table state from Netstack.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::net::IpAddr;
use std::num::NonZeroU64;

use crate::Errno;
use crate::client::InternalClient;
use crate::logging::{log_debug, log_warn};
use crate::messaging::Sender;
use crate::protocol_family::ProtocolFamily;
use crate::protocol_family::route::NetlinkRoute;
use crate::util::respond_to_completer;
use derivative::Derivative;
use futures::StreamExt as _;
use futures::channel::oneshot;
use net_types::ip::IpVersion;
use netlink_packet_core::{NLM_F_MULTIPART, NetlinkMessage};
use netlink_packet_route::neighbour::{
    NeighbourAddress, NeighbourAttribute, NeighbourFlags, NeighbourHeader, NeighbourMessage,
    NeighbourState,
};
use netlink_packet_route::route::RouteType;
use netlink_packet_route::{AddressFamily, RouteNetlinkMessage};
use thiserror::Error;

use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_ext as fnet_ext,
    fidl_fuchsia_net_interfaces_ext as fnet_interfaces_ext,
    fidl_fuchsia_net_neighbor as fnet_neighbor, fidl_fuchsia_net_neighbor_ext as fnet_neighbor_ext,
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
        header.ifindex = neighbor.interface.get().try_into().map_err(|_| {
            NetlinkNeighborMessageConversionError::InvalidInterfaceId(neighbor.interface.get())
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

fn neighbor_fidl_ip(
    family: AddressFamily,
    address: Option<&NeighbourAddress>,
) -> Result<fnet::IpAddress, RequestError> {
    match family {
        AddressFamily::Inet => match address {
            Some(NeighbourAddress::Inet(addr)) => Ok(fnet_ext::IpAddress(IpAddr::V4(*addr)).into()),
            Some(_) => Err(RequestError::AddressFamilyMismatch(family)),
            None => Err(RequestError::MissingAddress),
        },
        AddressFamily::Inet6 => match address {
            Some(NeighbourAddress::Inet6(addr)) => {
                Ok(fnet_ext::IpAddress(IpAddr::V6(*addr)).into())
            }
            Some(_) => Err(RequestError::AddressFamilyMismatch(family)),
            None => Err(RequestError::MissingAddress),
        },
        _ => Err(RequestError::InvalidAddressFamily(family)),
    }
}

/// Arguments for an RTM_GETNEIGH [`Request`].
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum GetNeighborArgs {
    Dump { ip_version: Option<IpVersion>, interface: Option<NonZeroU64> },
    Get { ip: fnet::IpAddress, interface: NonZeroU64 },
}

impl GetNeighborArgs {
    // Attempts to convert a netlink_packet_route `NeighbourMessage` into
    // `GetNeighborArgs`.
    pub(crate) fn try_from_rtnl_neighbor(
        message: &NeighbourMessage,
        is_dump: bool,
    ) -> Result<Self, RequestError> {
        if is_dump {
            Self::dump_request_from_rtnl_neighbor(message)
                .inspect_err(|e| log_warn!("{e} in dump neighbors request"))
        } else {
            Self::get_request_from_rtnl_neighbor(message)
                .inspect_err(|e| log_warn!("{e} in get neighbors request"))
        }
    }

    fn dump_request_from_rtnl_neighbor(message: &NeighbourMessage) -> Result<Self, RequestError> {
        let NeighbourHeader { family, flags, .. } = &message.header;
        if flags.contains(NeighbourFlags::Proxy) {
            // Netstack3 does not support ARP/NDP proxying.
            return Err(RequestError::UnsupportedFlags(*flags));
        }
        // TODO(https://fxbug.dev/456508664): Support strict validation of dump
        // requests.
        let ip_version = match family {
            AddressFamily::Unspec => None,
            AddressFamily::Inet => Some(IpVersion::V4),
            AddressFamily::Inet6 => Some(IpVersion::V6),
            family => {
                return Err(RequestError::InvalidAddressFamily(*family));
            }
        };
        // Note that the interface index is pulled from the attribute here,
        // whereas it's pulled from the header for get requests. This is
        // intentional, in order to maintain consistency with Linux's behavior.
        let interface = message
            .attributes
            .iter()
            .find_map(|attr| match attr {
                NeighbourAttribute::IfIndex(ifindex) => Some(u64::from(*ifindex).try_into()),
                _ => None,
            })
            .transpose()
            // 0 is treated as a lack of filter.
            .unwrap_or(None);
        Ok(GetNeighborArgs::Dump { ip_version, interface })
    }

    fn get_request_from_rtnl_neighbor(message: &NeighbourMessage) -> Result<Self, RequestError> {
        let NeighbourHeader { ifindex, family, state, flags, kind } = &message.header;
        if *state != NeighbourState::None {
            return Err(RequestError::InvalidState(*state));
        }
        if *kind != RouteType::Unspec {
            return Err(RequestError::InvalidKind(*kind));
        }
        if flags.intersects(!NeighbourFlags::Proxy) {
            return Err(RequestError::InvalidFlags(*flags));
        }
        if flags.contains(NeighbourFlags::Proxy) {
            // Netstack3 does not support ARP/NDP proxying.
            return Err(RequestError::UnsupportedFlags(*flags));
        }

        let (address, unsupported) = message.attributes.iter().fold(
            (None, false),
            |(address_acc, unsupported_acc), attr| {
                match attr {
                    NeighbourAttribute::Destination(addr) => {
                        // Note: In the event the Destination attribute is
                        // provided multiple times, keep the first.
                        (address_acc.or(Some(addr)), unsupported_acc)
                    }
                    _ => {
                        if !unsupported_acc {
                            // Only log for the first invalid attribute to avoid spamming.
                            log_warn!(
                                "unsupported request attribute: {attr:?} in get neighbor\
                                request; only `DST` is supported"
                            );
                        }
                        (address_acc, true)
                    }
                }
            },
        );
        if unsupported {
            return Err(RequestError::InvalidAttribute);
        }
        let ip = neighbor_fidl_ip(*family, address)?;
        // Note that the interface index is pulled from the header here, whereas
        // it's pulled from the attribute for dump requests. This is
        // intentional, in order to maintain consistency with Linux's behavior.
        let interface =
            u64::from(*ifindex).try_into().map_err(|_| RequestError::MissingInterface)?;
        Ok(GetNeighborArgs::Get { ip, interface })
    }
}

/// [`Request`] arguments associated with neighbors.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub(crate) enum NeighborRequestArgs {
    /// RTM_GETNEIGH
    Get(GetNeighborArgs),
}

/// An error encountered while handling a [`Request`].
#[derive(Copy, Clone, Debug, PartialEq, Eq, Error)]
pub(crate) enum RequestError {
    /// Invalid state in neighbor header.
    #[error("invalid state: {0:?}")]
    InvalidState(NeighbourState),
    /// Invalid kind in neighbor header.
    #[error("invalid kind: {0:?}")]
    InvalidKind(RouteType),
    /// Invalid flags in neighbor header.
    #[error("invalid flags: {0:?}")]
    InvalidFlags(NeighbourFlags),
    /// Unsupported flags.
    #[error("unsupported flags: {0:?}")]
    UnsupportedFlags(NeighbourFlags),
    /// Invalid address family.
    #[error("invalid address family: {0:?}")]
    InvalidAddressFamily(AddressFamily),
    /// Address family in request header doesn't match family of address.
    // In practice this should never be encountered:
    // `NeighbourAddress::parse_with_param` parses the address based on the
    // address family from the header, and a discrepancy between the expected
    // and actual address length results in a parsing failure.
    #[error("address family mismatch; expected={0:?}")]
    AddressFamilyMismatch(AddressFamily),
    /// Request doesn't specify required neighbor address.
    #[error("missing required `DST` attribute")]
    MissingAddress,
    /// Request doesn't specify required interface.
    #[error("missing required interface")]
    MissingInterface,
    /// Request specifies invalid attribute(s).
    #[error("invalid request attribute")]
    InvalidAttribute,
    /// No such neighbor.
    #[error("no such neighbor")]
    NeighborNotFound,
    /// Interface not found.
    #[error("no such interface")]
    InterfaceNotFound,
}

impl From<RequestError> for Errno {
    fn from(value: RequestError) -> Self {
        match value {
            RequestError::InvalidState(_) => Errno::EINVAL,
            RequestError::InvalidKind(_) => Errno::EINVAL,
            RequestError::InvalidFlags(_) => Errno::EINVAL,
            RequestError::UnsupportedFlags(_) => Errno::ENOTSUP,
            RequestError::InvalidAddressFamily(_) => Errno::EAFNOSUPPORT,
            RequestError::AddressFamilyMismatch(_) => Errno::EINVAL,
            RequestError::MissingAddress => Errno::EINVAL,
            RequestError::MissingInterface => Errno::EINVAL,
            RequestError::InvalidAttribute => Errno::EINVAL,
            RequestError::NeighborNotFound => Errno::ENOENT,
            RequestError::InterfaceNotFound => Errno::ENODEV,
        }
    }
}

/// Trait abstracting the ability to check if an interface exists.
pub(crate) trait LookupIfInterfaceExists {
    /// Returns whether an interface exists at the provided index.
    fn exists(&self, interface: NonZeroU64) -> bool;
}

type InterfaceMap = BTreeMap<
    u64,
    fnet_interfaces_ext::PropertiesAndState<
        crate::interfaces::InterfaceState,
        fnet_interfaces_ext::AllInterest,
    >,
>;

impl LookupIfInterfaceExists for InterfaceMap {
    fn exists(&self, interface: NonZeroU64) -> bool {
        self.contains_key(&interface.get())
    }
}

/// A request associated with neighbors.
#[derive(Derivative)]
#[derivative(Debug(bound = ""))]
pub(crate) struct Request<S: Sender<<NetlinkRoute as ProtocolFamily>::Response>> {
    /// The resource and operation-specific argument(s) for this request.
    pub args: NeighborRequestArgs,
    /// The request's sequence number.
    ///
    /// This value will be copied verbatim into any message sent as a result of
    /// this request.
    pub sequence_number: u32,
    /// The client that made the request.
    pub client: InternalClient<NetlinkRoute, S>,
    /// A completer that will have the result of the request sent over.
    pub completer: oneshot::Sender<Result<(), RequestError>>,
}

/// Errors related to handling neighbor events from Netstack.
#[derive(Debug, Error, PartialEq)]
pub(crate) enum HandleWatchEventError {
    /// An event indicated a neighbor was removed that was not previously known.
    #[error("Netstack reported removal of an unknown neighbor: {0:?}")]
    UnknownNeighborRemoved(fnet_neighbor_ext::Entry),
    /// An event indicated a neighbor was changed that was not previously known.
    #[error("Netstack reported change of an unknown neighbor: {0:?}")]
    UnknownNeighborChanged(fnet_neighbor_ext::Entry),
    /// An event indicated a neighbor was added that conflicts with a known
    /// neighbor.
    #[error(
        "Netstack reported addition of a neighbor that already exists: \
        existing={existing:?}, new={new:?}"
    )]
    ConflictingNeighborAdded { existing: fnet_neighbor_ext::Entry, new: fnet_neighbor_ext::Entry },
    /// An `Existing` or `Idle` event was received after collecting the initial
    /// neighbors from the event stream.
    #[error("Netstack reported unexpected event: {0:?}")]
    UnexpectedEventReceived(fnet_neighbor_ext::Event),
}

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, PartialOrd, Ord)]
struct NeighborKey {
    interface: NonZeroU64,
    neighbor: fnet::IpAddress,
}

impl From<&fnet_neighbor_ext::Entry> for NeighborKey {
    fn from(
        fnet_neighbor_ext::Entry { interface, neighbor, .. }: &fnet_neighbor_ext::Entry,
    ) -> NeighborKey {
        NeighborKey { interface: *interface, neighbor: *neighbor }
    }
}

/// Handles asynchronous work related to RTM_*NEIGH messages.
///
/// Can respond to RTM_*NEIGH message requests.
pub(crate) struct NeighborsWorker {
    neighbor_table: HashMap<NeighborKey, fnet_neighbor_ext::Entry>,
}

impl NeighborsWorker {
    /// Create the Netlink Neighbors Worker.
    ///
    /// Panics if the existing neighbors cannot be retrieved from
    /// `neighbors_view` or if the response contains conflicting neighbors.
    pub(crate) async fn create(
        neighbors_view: &fnet_neighbor::ViewProxy,
    ) -> (
        Self,
        impl futures::Stream<
            Item = Result<fnet_neighbor_ext::Event, fnet_neighbor_ext::EntryIteratorError>,
        > + Unpin
        + 'static,
    ) {
        let mut neighbor_event_stream = Box::pin(
            fnet_neighbor_ext::event_stream_from_view(neighbors_view)
                .expect("connecting to fuchsia.net.neighbors.View FIDL should succeed"),
        );
        let existing_neighbors: HashSet<fnet_neighbor_ext::Entry> =
            fnet_neighbor_ext::collect_neighbors_until_idle(neighbor_event_stream.by_ref())
                .await
                .expect("determining existing neighbors should succeed");
        let existing_count = existing_neighbors.len();
        let neighbor_table = existing_neighbors
            .into_iter()
            .map(|e| (NeighborKey::from(&e), e))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            neighbor_table.len(),
            existing_count,
            "conflicting existing entry in neighbor table"
        );
        (Self { neighbor_table }, neighbor_event_stream)
    }

    pub(crate) fn handle_neighbor_watcher_event(
        &mut self,
        event: fnet_neighbor_ext::Event,
    ) -> Result<(), HandleWatchEventError> {
        match event {
            fnet_neighbor_ext::Event::Removed(entry) => {
                match self.neighbor_table.remove(&(&entry).into()) {
                    Some(_) => Ok(()),
                    None => Err(HandleWatchEventError::UnknownNeighborRemoved(entry)),
                }
            }
            fnet_neighbor_ext::Event::Added(entry) => {
                match self.neighbor_table.insert((&entry).into(), entry.clone()) {
                    Some(existing) => Err(HandleWatchEventError::ConflictingNeighborAdded {
                        existing,
                        new: entry,
                    }),
                    None => Ok(()),
                }
            }
            fnet_neighbor_ext::Event::Changed(entry) => {
                match self.neighbor_table.insert((&entry).into(), entry.clone()) {
                    Some(_) => Ok(()),
                    None => Err(HandleWatchEventError::UnknownNeighborChanged(entry)),
                }
            }
            e @ fnet_neighbor_ext::Event::Existing(_) | e @ fnet_neighbor_ext::Event::Idle => {
                Err(HandleWatchEventError::UnexpectedEventReceived(e))
            }
        }
    }

    pub(crate) fn handle_request<S: Sender<<NetlinkRoute as ProtocolFamily>::Response>>(
        &mut self,
        Request { args, mut client, sequence_number, completer }: Request<S>,
        interface_lookup: &impl LookupIfInterfaceExists,
    ) {
        let result = match args {
            NeighborRequestArgs::Get(args) => match args {
                GetNeighborArgs::Dump { ip_version, interface } => {
                    self.neighbor_table
                        .values()
                        .filter(|n| {
                            ip_version.map_or(true, |ip_version| match n.neighbor {
                                fnet::IpAddress::Ipv4(_) => ip_version == IpVersion::V4,
                                fnet::IpAddress::Ipv6(_) => ip_version == IpVersion::V6,
                            })
                        })
                        .filter(|n| interface.map_or(true, |i| n.interface == i))
                        .filter_map(|e| NetlinkNeighborMessage::optionally_from(e.clone()))
                        .for_each(|m| {
                            client.send_unicast(m.into_rtnl_new_neighbor(sequence_number, true));
                        });
                    Ok(())
                }
                GetNeighborArgs::Get { ip, interface } => {
                    let neighbor = self
                        .neighbor_table
                        .get(&NeighborKey { interface, neighbor: ip })
                        .map(|e| NetlinkNeighborMessage::optionally_from(e.clone()))
                        .flatten();
                    match neighbor {
                        Some(msg) => {
                            client.send_unicast(msg.into_rtnl_new_neighbor(sequence_number, false));
                            Ok(())
                        }
                        None => {
                            let err = if interface_lookup.exists(interface) {
                                RequestError::NeighborNotFound
                            } else {
                                RequestError::InterfaceNotFound
                            };
                            Err(err)
                        }
                    }
                }
            },
        };

        log_debug!("handled request {args:?} from {client} with result = {result:?}");
        respond_to_completer(client, completer, result, args);
    }
}

#[cfg(test)]
mod tests {
    use crate::client::ClientTable;
    use crate::client::testutil::{CLIENT_ID_1, new_fake_client};
    use crate::interfaces::testutil::FakeInterfacesHandler;
    use crate::messaging::testutil::FakeSender;
    use crate::route_eventloop::{
        EventLoopComponent, EventLoopInputs, EventLoopSpec, IncludedWorkers, Optional, Required,
        UnifiedRequest,
    };

    use super::*;

    use assert_matches::assert_matches;
    use fidl_fuchsia_net_neighbor::ViewRequest;
    use fidl_fuchsia_net_neighbor_ext::testutil::EventSpec;
    use futures::channel::mpsc;
    use futures::{FutureExt as _, SinkExt as _, TryStreamExt as _};
    use maplit::hashset;
    use net_declare::{fidl_ip, std_ip_v4, std_ip_v6};
    use netlink_packet_core::NetlinkPayload;
    use netlink_packet_route::neighbour::{NeighbourAddress, NeighbourFlags};
    use std::collections::HashSet;
    use test_case::test_case;
    use {
        fidl_fuchsia_net as fnet, fidl_fuchsia_net_interfaces as fnet_interfaces,
        fidl_fuchsia_net_root as fnet_root,
    };

    fn valid_neighbor_entry() -> fnet_neighbor_ext::Entry {
        fnet_neighbor_ext::Entry {
            interface: NonZeroU64::new(1).unwrap(),
            neighbor: fidl_ip!("192.168.0.1"),
            state: fnet_neighbor::EntryState::Reachable,
            mac: Some(fnet::MacAddress { octets: [0, 1, 2, 3, 4, 5] }),
            updated_at: 123456,
        }
    }

    #[test]
    fn netlink_neighbor_message_from_entry_invalid_iface_id() {
        let entry = fnet_neighbor_ext::Entry {
            interface: NonZeroU64::new(u64::MAX).unwrap(),
            ..valid_neighbor_entry()
        };

        assert_eq!(
            NetlinkNeighborMessage::try_from(entry),
            Err(NetlinkNeighborMessageConversionError::InvalidInterfaceId(u64::MAX))
        );
    }

    #[test]
    fn netlink_neighbor_message_from_entry_valid_iface_id() {
        assert_matches!(
            NetlinkNeighborMessage::try_from(fnet_neighbor_ext::Entry {
                interface: NonZeroU64::new(1).unwrap(),
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
                interface: NonZeroU64::new(u64::MAX).unwrap(),
                ..valid_neighbor_entry()
            }),
            None
        );
    }

    #[test]
    fn netlink_neighbor_message_optionally_from_success() {
        let fidl_entry = fnet_neighbor_ext::Entry {
            interface: NonZeroU64::new(1).unwrap(),
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

    #[test]
    fn neighbor_keyed_by_interface_and_ip() {
        let entry = fnet_neighbor_ext::Entry {
            interface: NonZeroU64::new(1).unwrap(),
            neighbor: fidl_ip!("192.168.0.1"),
            mac: None,
            state: fnet_neighbor::EntryState::Reachable,
            updated_at: 123456,
        };

        let same_iface_and_ip = fnet_neighbor_ext::Entry {
            mac: Some(fnet::MacAddress { octets: [0, 1, 2, 3, 4, 5] }),
            state: fnet_neighbor::EntryState::Stale,
            updated_at: 654321,
            ..entry
        };
        assert_eq!(NeighborKey::from(&entry), NeighborKey::from(&same_iface_and_ip));

        let different_iface =
            fnet_neighbor_ext::Entry { interface: NonZeroU64::new(2).unwrap(), ..entry };
        assert_ne!(NeighborKey::from(&entry), NeighborKey::from(&different_iface));

        let different_ip = fnet_neighbor_ext::Entry { neighbor: fidl_ip!("192.168.0.2"), ..entry };
        assert_ne!(NeighborKey::from(&entry), NeighborKey::from(&different_ip));

        let different_iface_and_ip = fnet_neighbor_ext::Entry {
            interface: NonZeroU64::new(2).unwrap(),
            neighbor: fidl_ip!("192.168.0.2"),
            ..entry
        };
        assert_ne!(NeighborKey::from(&entry), NeighborKey::from(&different_iface_and_ip));
    }

    #[fuchsia::test]
    #[should_panic(expected = "determining existing neighbors should succeed")]
    async fn neighbors_worker_create_panics_on_view_protocol_error() {
        let (view, view_server_end) = fidl::endpoints::create_proxy::<fnet_neighbor::ViewMarker>();
        // Close the channel without responding.
        drop(view_server_end);

        let (_worker, _remaining) = NeighborsWorker::create(&view).await;
    }

    #[fuchsia::test]
    #[should_panic(expected = "determining existing neighbors should succeed")]
    async fn neighbors_worker_create_panics_on_event_stream_error() {
        let (view, view_server_end) = fidl::endpoints::create_proxy::<fnet_neighbor::ViewMarker>();
        let mut view_request_stream = view_server_end.into_stream();

        let entry_iter_fut = view_request_stream
            .next()
            .then(|req| {
                match req
                    .expect("View request_stream unexpectedly ended")
                    .expect("failed to receive `OpenEntryIterator` request")
                {
                    ViewRequest::OpenEntryIterator { it, .. } => {
                        // Close the channel without responding.
                        drop(it);
                        futures::future::ready(())
                    }
                }
            })
            .fuse();

        let worker_fut = NeighborsWorker::create(&view);

        let ((), (_worker, _remaining)) = futures::join!(entry_iter_fut, worker_fut);
    }

    #[fuchsia::test]
    #[should_panic(expected = "conflicting existing entry")]
    async fn neighbors_worker_create_panics_on_conflicting_entry() {
        let events: Vec<_> = [
            // Create two neighbors with the same `NeighborKey` but differing
            // fields; truly duplicate entries are ignored.
            fnet_neighbor_ext::Entry {
                state: fnet_neighbor::EntryState::Reachable,
                ..valid_neighbor_entry()
            },
            fnet_neighbor_ext::Entry {
                state: fnet_neighbor::EntryState::Stale,
                ..valid_neighbor_entry()
            },
        ]
        .into_iter()
        .map(Into::into)
        .map(fnet_neighbor::EntryIteratorItem::Existing)
        .chain(std::iter::once(fnet_neighbor::EntryIteratorItem::Idle(fnet_neighbor::IdleEvent)))
        .collect();
        let batches = vec![events];
        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(batches));

        let worker_fut = NeighborsWorker::create(&view);

        let ((), (_worker, _remaining)) = futures::join!(server_fut, worker_fut);
    }

    #[fuchsia::test]
    async fn neighbors_worker_create_success() {
        use fnet_neighbor_ext::testutil::EventSpec::*;
        let events = fnet_neighbor_ext::testutil::generate_events_from_spec(&[
            Existing(1),
            Existing(2),
            Existing(3),
            Idle,
            Added(4),
        ]);
        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(vec![
                events.clone(),
            ]));

        let worker_fut = NeighborsWorker::create(&view);

        let ((), (worker, event_stream)) = futures::join!(server_fut, worker_fut);

        let remaining_events: Vec<_> = event_stream.collect().await;
        assert_matches!(
            &remaining_events[..],
            [
                Ok(fnet_neighbor_ext::Event::Added(_)),
                Err(fnet_neighbor_ext::EntryIteratorError::Fidl(
                    fidl::Error::ClientChannelClosed { .. }
                ))
            ]
        );

        for event in events {
            match event {
                fnet_neighbor::EntryIteratorItem::Existing(fidl_entry) => {
                    let entry: fnet_neighbor_ext::Entry = fidl_entry.try_into().unwrap();
                    assert_eq!(worker.neighbor_table.get(&(&entry).into()), Some(&entry));
                }
                _ => {}
            }
        }
    }

    #[test_case(
        EventSpec::Added(2),
        |e| matches!(e, HandleWatchEventError::ConflictingNeighborAdded { .. });
        "conflicting added"
    )]
    #[test_case(
        EventSpec::Removed(4),
        |e| matches!(e, HandleWatchEventError::UnknownNeighborRemoved(_));
        "unknown removed"
    )]
    #[test_case(
        EventSpec::Changed(4),
        |e| matches!(e, HandleWatchEventError::UnknownNeighborChanged(_));
        "unknown changed"
    )]
    #[test_case(
        EventSpec::Existing(4),
        |e| matches!(e, HandleWatchEventError::UnexpectedEventReceived(_));
        "existing after initial collection"
    )]
    #[test_case(
        EventSpec::Idle,
        |e| matches!(e, HandleWatchEventError::UnexpectedEventReceived(_));
        "idle after initial collection"
    )]
    #[fuchsia::test]
    async fn neighbors_worker_handle_watch_event_failure(
        spec: EventSpec,
        error_matcher: fn(&HandleWatchEventError) -> bool,
    ) {
        use fnet_neighbor_ext::testutil::EventSpec::*;
        let events = fnet_neighbor_ext::testutil::generate_events_from_spec(&[
            Existing(1),
            Existing(2),
            Existing(3),
            Idle,
            spec,
        ]);
        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(vec![
                events.clone(),
            ]));

        let worker_fut = NeighborsWorker::create(&view);

        let ((), (mut worker, event_stream)) = futures::join!(server_fut, worker_fut);

        let remaining_events: Vec<_> = event_stream.collect().await;
        assert_eq!(remaining_events.len(), 2);
        match &remaining_events[0] {
            Ok(event) => {
                assert_matches!(
                    worker.handle_neighbor_watcher_event(event.clone()),
                    Err(error) if error_matcher(&error)
                );
            }
            _ => panic!("expected bad event in stream"),
        }
        match &remaining_events[1] {
            Err(fnet_neighbor_ext::EntryIteratorError::Fidl(
                fidl::Error::ClientChannelClosed { .. },
            )) => {}
            _ => panic!("expected PEER_CLOSED error at end of stream"),
        }
    }

    #[fuchsia::test]
    async fn neighbors_worker_handle_added_event() {
        use fnet_neighbor_ext::testutil::EventSpec::*;
        let events = fnet_neighbor_ext::testutil::generate_events_from_spec(&[
            Existing(1),
            Existing(2),
            Existing(3),
            Idle,
            Added(4),
        ]);
        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(vec![
                events.clone(),
            ]));

        let worker_fut = NeighborsWorker::create(&view);

        let ((), (mut worker, event_stream)) = futures::join!(server_fut, worker_fut);

        let remaining_events: Vec<_> = event_stream.collect().await;
        assert_eq!(remaining_events.len(), 2);
        match &remaining_events[0] {
            Ok(e @ fnet_neighbor_ext::Event::Added(entry)) => {
                let key = NeighborKey::from(entry);
                assert_eq!(worker.neighbor_table.get(&key), None);
                assert_matches!(worker.handle_neighbor_watcher_event(e.clone()), Ok(_));
                assert_eq!(worker.neighbor_table.get(&key), Some(entry));
            }
            _ => panic!("expected Added event in stream"),
        }
        match &remaining_events[1] {
            Err(fnet_neighbor_ext::EntryIteratorError::Fidl(
                fidl::Error::ClientChannelClosed { .. },
            )) => {}
            _ => panic!("expected PEER_CLOSED error at end of stream"),
        }
    }

    #[fuchsia::test]
    async fn neighbors_worker_handle_removed_event() {
        use fnet_neighbor_ext::testutil::EventSpec::*;
        let events = fnet_neighbor_ext::testutil::generate_events_from_spec(&[
            Existing(1),
            Existing(2),
            Existing(3),
            Idle,
            Removed(2),
        ]);
        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(vec![
                events.clone(),
            ]));

        let worker_fut = NeighborsWorker::create(&view);

        let ((), (mut worker, event_stream)) = futures::join!(server_fut, worker_fut);

        let remaining_events: Vec<_> = event_stream.collect().await;
        assert_eq!(remaining_events.len(), 2);
        match &remaining_events[0] {
            Ok(e @ fnet_neighbor_ext::Event::Removed(entry)) => {
                let key = NeighborKey::from(entry);
                assert_eq!(worker.neighbor_table.get(&key), Some(entry));
                assert_matches!(worker.handle_neighbor_watcher_event(e.clone()), Ok(_));
                assert_eq!(worker.neighbor_table.get(&key), None);
            }
            _ => panic!("expected Removed event in stream"),
        }
        match &remaining_events[1] {
            Err(fnet_neighbor_ext::EntryIteratorError::Fidl(
                fidl::Error::ClientChannelClosed { .. },
            )) => {}
            _ => panic!("expected PEER_CLOSED error at end of stream"),
        }
    }

    #[fuchsia::test]
    async fn neighbors_worker_handle_changed_event() {
        use fnet_neighbor_ext::testutil::EventSpec::*;
        let mut events = fnet_neighbor_ext::testutil::generate_events_from_spec(&[
            Existing(1),
            Existing(2),
            Existing(3),
            Idle,
            Changed(2),
        ]);
        match &mut events[1] {
            fnet_neighbor::EntryIteratorItem::Existing(entry) => {
                entry.updated_at = Some(1234);
            }
            _ => panic!("expected Existing event in stream"),
        }
        match &mut events[4] {
            fnet_neighbor::EntryIteratorItem::Changed(entry) => {
                entry.updated_at = Some(5678);
            }
            _ => panic!("expected Changed event in stream"),
        }

        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(vec![
                events.clone(),
            ]));

        let worker_fut = NeighborsWorker::create(&view);

        let ((), (mut worker, event_stream)) = futures::join!(server_fut, worker_fut);

        let remaining_events: Vec<_> = event_stream.collect().await;
        assert_eq!(remaining_events.len(), 2);
        match &remaining_events[0] {
            Ok(e @ fnet_neighbor_ext::Event::Changed(entry)) => {
                let key = NeighborKey::from(entry);
                assert_matches!(
                    worker.neighbor_table.get(&key),
                    Some(fnet_neighbor_ext::Entry { updated_at: 1234, .. })
                );
                assert_matches!(worker.handle_neighbor_watcher_event(e.clone()), Ok(_));
                assert_matches!(
                    worker.neighbor_table.get(&key),
                    Some(fnet_neighbor_ext::Entry { updated_at: 5678, .. })
                );
            }
            _ => panic!("expected Changed event in stream"),
        }
        match &remaining_events[1] {
            Err(fnet_neighbor_ext::EntryIteratorError::Fidl(
                fidl::Error::ClientChannelClosed { .. },
            )) => {}
            _ => panic!("expected PEER_CLOSED error at end of stream"),
        }
    }

    impl LookupIfInterfaceExists for HashSet<u64> {
        fn exists(&self, idx: NonZeroU64) -> bool {
            self.contains(&idx.get())
        }
    }

    #[test_case(HashSet::new(), RequestError::InterfaceNotFound; "interface does not exist")]
    #[test_case(
        hashset!{1}, RequestError::NeighborNotFound;
        "interface exists"
    )]
    #[fuchsia::test]
    async fn neighbors_worker_handle_get_neighbor_not_found(
        interface_lookup: HashSet<u64>,
        expected_error: RequestError,
    ) {
        let (mut sender_sink, client, _async_work_drain_task) =
            new_fake_client(CLIENT_ID_1, vec![]);
        let (completer, completer_rcv) = oneshot::channel();
        let request = Request {
            args: NeighborRequestArgs::Get(GetNeighborArgs::Get {
                ip: fidl_ip!("192.168.0.1"),
                interface: NonZeroU64::new(1).unwrap(),
            }),
            sequence_number: 1,
            client,
            completer,
        };

        let events: Vec<_> = [
            fnet_neighbor_ext::Entry {
                interface: NonZeroU64::new(2).unwrap(),
                neighbor: fidl_ip!("192.168.0.1"),
                ..valid_neighbor_entry()
            },
            fnet_neighbor_ext::Entry {
                interface: NonZeroU64::new(1).unwrap(),
                neighbor: fidl_ip!("fe80::2"),
                ..valid_neighbor_entry()
            },
        ]
        .into_iter()
        .map(Into::into)
        .map(fnet_neighbor::EntryIteratorItem::Existing)
        .chain(std::iter::once(fnet_neighbor::EntryIteratorItem::Idle(fnet_neighbor::IdleEvent)))
        .collect();

        let batches = vec![events];
        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(batches));

        let worker_fut = NeighborsWorker::create(&view);
        let ((), (mut worker, _event_stream)) = futures::join!(server_fut, worker_fut);

        worker.handle_request(request, &interface_lookup);

        let result = completer_rcv.await.expect("completer channel should not be closed");
        assert_matches!(result, Err(e) if e == expected_error);
        assert_eq!(&sender_sink.take_messages()[..], &[]);
    }

    #[test_case(
        GetNeighborArgs::Dump{ ip_version: None, interface: None },
        &[1, 2, 3, 4];
        "dump all"
    )]
    #[test_case(
        GetNeighborArgs::Dump{ ip_version: Some(IpVersion::V4), interface: None },
        &[1, 3];
        "dump ipv4 only"
    )]
    #[test_case(
        GetNeighborArgs::Dump{ ip_version: Some(IpVersion::V6), interface: None },
        &[2, 4];
        "dump ipv6 only"
    )]
    #[test_case(
        GetNeighborArgs::Dump{
            ip_version: Some(IpVersion::V6),
            interface: Some(NonZeroU64::new(4).unwrap())
        },
        &[4];
        "dump interface 4 ipv6"
    )]
    #[test_case(
        GetNeighborArgs::Dump{
            ip_version: Some(IpVersion::V4),
            interface: Some(NonZeroU64::new(4).unwrap())
        },
        &[];
        "dump interface 4 ipv4"
    )]
    #[test_case(
        GetNeighborArgs::Get{ ip: fidl_ip!("192.168.0.1"), interface: NonZeroU64::new(1).unwrap() },
        &[1];
        "get ipv4"
    )]
    #[test_case(
        GetNeighborArgs::Get{ ip: fidl_ip!("fe80::2"), interface: NonZeroU64::new(2).unwrap() },
        &[2];
        "get ipv6"
    )]
    #[fuchsia::test]
    async fn neighbors_worker_handle_get_request(
        get_args: GetNeighborArgs,
        expected_ifindexes: &[u32],
    ) {
        let (mut sender_sink, client, _async_work_drain_task) =
            new_fake_client(CLIENT_ID_1, vec![]);
        let (completer, completer_rcv) = oneshot::channel();
        let request = Request {
            args: NeighborRequestArgs::Get(get_args),
            sequence_number: 1,
            client,
            completer,
        };

        let events: Vec<_> = [
            fnet_neighbor_ext::Entry {
                interface: NonZeroU64::new(1).unwrap(),
                neighbor: fidl_ip!("192.168.0.1"),
                ..valid_neighbor_entry()
            },
            fnet_neighbor_ext::Entry {
                interface: NonZeroU64::new(2).unwrap(),
                neighbor: fidl_ip!("fe80::2"),
                ..valid_neighbor_entry()
            },
            fnet_neighbor_ext::Entry {
                interface: NonZeroU64::new(3).unwrap(),
                neighbor: fidl_ip!("192.168.0.3"),
                ..valid_neighbor_entry()
            },
            fnet_neighbor_ext::Entry {
                interface: NonZeroU64::new(4).unwrap(),
                neighbor: fidl_ip!("fe80::4"),
                ..valid_neighbor_entry()
            },
        ]
        .into_iter()
        .map(Into::into)
        .map(fnet_neighbor::EntryIteratorItem::Existing)
        .chain(std::iter::once(fnet_neighbor::EntryIteratorItem::Idle(fnet_neighbor::IdleEvent)))
        .collect();

        let batches = vec![events];
        let (view, server_fut) =
            fnet_neighbor_ext::testutil::create_fake_view(futures::stream::iter(batches));

        let worker_fut = NeighborsWorker::create(&view);
        let ((), (mut worker, _event_stream)) = futures::join!(server_fut, worker_fut);

        worker.handle_request(request, &BTreeMap::new());

        completer_rcv
            .await
            .expect("completer channel should not be closed")
            .expect("request handling result should be OK");

        let mut ifindexes_seen = Vec::new();
        for sent_message in sender_sink.take_messages() {
            match sent_message.message.payload {
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(
                    NeighbourMessage { header: NeighbourHeader { ifindex, .. }, .. },
                )) => {
                    ifindexes_seen.push(ifindex);
                }
                _ => panic!("unexpected message sent"),
            }
        }
        ifindexes_seen.sort();
        assert_eq!(&ifindexes_seen[..], expected_ifindexes);
    }

    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
        ] => Ok(GetNeighborArgs::Get {
            ip: fidl_ip!("192.168.0.1"),
            interface: NonZeroU64::new(1).unwrap(),
        });
        "get ipv4 success"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet6,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v6!("fe80::1").into()),
        ] => Ok(GetNeighborArgs::Get {
            ip: fidl_ip!("fe80::1"),
            interface: NonZeroU64::new(1).unwrap(),
        });
        "get ipv6 success"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            state: NeighbourState::Reachable,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
        ] => Err(RequestError::InvalidState(NeighbourState::Reachable));
        "get invalid request state"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            kind: RouteType::Broadcast,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
        ] => Err(RequestError::InvalidKind(RouteType::Broadcast));
        "get invalid request kind"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            flags: NeighbourFlags::Router,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
        ] => Err(RequestError::InvalidFlags(NeighbourFlags::Router));
        "get invalid request flag"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            flags: NeighbourFlags::Proxy,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
        ] => Err(RequestError::UnsupportedFlags(NeighbourFlags::Proxy));
        "get unsupported request flag"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet6,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
        ] => Err(RequestError::AddressFamilyMismatch(AddressFamily::Inet6));
        "get address family mismatch"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            family: AddressFamily::Inet,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
        ] => Err(RequestError::MissingInterface);
        "get interface unspecified"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            ..Default::default()
        },
        &[] => Err(RequestError::MissingAddress);
        "get destination unspecified"
    )]
    #[test_case(
        false,
        NeighbourHeader {
            ifindex: 1,
            family: AddressFamily::Inet,
            ..Default::default()
        },
        &[
            NeighbourAttribute::Destination(std_ip_v4!("192.168.0.1").into()),
            NeighbourAttribute::LinkLocalAddress(vec![0, 1, 2, 3, 4, 5]),
        ] => Err(RequestError::InvalidAttribute);
        "get invalid attribute"
    )]
    #[test_case(
        true,
        NeighbourHeader::default(),
        &[] => Ok(GetNeighborArgs::Dump {
            ip_version: None,
            interface: None,
        });
        "dump all"
    )]
    #[test_case(
        true,
        NeighbourHeader {
            family: AddressFamily::Inet,
            ..Default::default()
        },
        &[] => Ok(GetNeighborArgs::Dump {
            ip_version: Some(IpVersion::V4),
            interface: None,
        });
        "dump ipv4 only"
    )]
    #[test_case(
        true,
        NeighbourHeader {
            family: AddressFamily::Inet6,
            ..Default::default()
        },
        &[] => Ok(GetNeighborArgs::Dump {
            ip_version: Some(IpVersion::V6),
            interface: None,
        });
        "dump ipv6 only"
    )]
    #[test_case(
        true,
        NeighbourHeader::default(),
        &[
            NeighbourAttribute::IfIndex(1),
        ] => Ok(GetNeighborArgs::Dump {
            ip_version: None,
            interface: Some(NonZeroU64::new(1).unwrap()),
        });
        "dump interface 1"
    )]
    #[test_case(
        true,
        NeighbourHeader::default(),
        &[
            NeighbourAttribute::IfIndex(0),
        ] => Ok(GetNeighborArgs::Dump {
            ip_version: None,
            interface: None,
        });
        "dump interface 0 treated as all interfaces"
    )]
    #[test_case(
        true,
        NeighbourHeader {
            family: AddressFamily::Local,
            ..Default::default()
        },
        &[] => Err(RequestError::InvalidAddressFamily(AddressFamily::Local));
        "dump invalid address family"
    )]
    #[test_case(
        true,
        NeighbourHeader::default(),
        &[
            NeighbourAttribute::LinkLocalAddress(vec![0, 1, 2, 3, 4, 5]),
        ] => Ok(GetNeighborArgs::Dump {
            ip_version: None,
            interface: None,
        });
        "dump unsupported attribute ignored (non-strict)"
    )]
    #[test_case(
        true,
        NeighbourHeader {
            flags: NeighbourFlags::Proxy,
            ..Default::default()
        },
        &[] => Err(RequestError::UnsupportedFlags(NeighbourFlags::Proxy));
        "dump unsupported request flag"
    )]
    #[fuchsia::test]
    fn get_neighbor_args_try_from_rtnl_neighbor(
        is_dump: bool,
        header: NeighbourHeader,
        attrs: &[NeighbourAttribute],
    ) -> Result<GetNeighborArgs, RequestError> {
        let mut message = NeighbourMessage::default();
        message.header = header;
        message.attributes = attrs.to_vec();
        GetNeighborArgs::try_from_rtnl_neighbor(&message, is_dump)
    }

    #[test_case(
        RequestError::InvalidState(NeighbourState::Reachable) => Errno::EINVAL;
        "invalid state"
    )]
    #[test_case(RequestError::InvalidKind(RouteType::Broadcast) => Errno::EINVAL; "invalid kind")]
    #[test_case(
        RequestError::InvalidFlags(NeighbourFlags::Router) => Errno::EINVAL;
        "invalid flags"
    )]
    #[test_case(
        RequestError::UnsupportedFlags(NeighbourFlags::Proxy) => Errno::ENOTSUP;
        "unsupported flags"
    )]
    #[test_case(
        RequestError::InvalidAddressFamily(AddressFamily::Local) => Errno::EAFNOSUPPORT;
        "invalid address family"
    )]
    #[test_case(
        RequestError::AddressFamilyMismatch(AddressFamily::Inet6) => Errno::EINVAL;
        "address family mismatch"
    )]
    #[test_case(RequestError::MissingInterface => Errno::EINVAL; "interface unspecified")]
    #[test_case(RequestError::MissingAddress => Errno::EINVAL; "destination unspecified")]
    #[test_case(RequestError::InvalidAttribute => Errno::EINVAL; "invalid attribute")]
    #[test_case(RequestError::NeighborNotFound => Errno::ENOENT; "neighbor not found")]
    #[test_case(RequestError::InterfaceNotFound => Errno::ENODEV; "interface not found")]
    #[fuchsia::test]
    fn request_error_into_errno(error: RequestError) -> Errno {
        error.into()
    }

    enum OnlyNeighbors {}
    impl EventLoopSpec for OnlyNeighbors {
        type NeighborWorker = Required;

        type InterfacesProxy = Optional;
        type InterfacesStateProxy = Optional;
        type V4RoutesState = Optional;
        type V6RoutesState = Optional;
        type V4RoutesSetProvider = Optional;
        type V6RoutesSetProvider = Optional;
        type V4RouteTableProvider = Optional;
        type V6RouteTableProvider = Optional;
        type InterfacesHandler = Optional;
        type RouteClients = Optional;

        type RoutesV4Worker = Optional;
        type RoutesV6Worker = Optional;
        type InterfacesWorker = Optional;
        type RuleV4Worker = Optional;
        type RuleV6Worker = Optional;
        type NduseroptWorker = Optional;
    }

    const TEST_SEQUENCE_NUMBER: u32 = 1234;

    #[fuchsia::test]
    async fn event_loop_with_watch_events_and_get_request() {
        let included_workers = IncludedWorkers {
            routes_v4: EventLoopComponent::Absent(Optional),
            routes_v6: EventLoopComponent::Absent(Optional),
            interfaces: EventLoopComponent::Present(()),
            rules_v4: EventLoopComponent::Absent(Optional),
            rules_v6: EventLoopComponent::Absent(Optional),
            neighbors: EventLoopComponent::Present(()),
            nduseropt: EventLoopComponent::Absent(Optional),
        };
        let (mut request_sink, request_stream) = mpsc::channel(1);

        // Configure fake neighbor watch events.

        let scope = fuchsia_async::Scope::new();
        let (neighbors_view, neighbor_event_sender) = {
            use fnet_neighbor_ext::testutil::EventSpec::*;
            let events = fnet_neighbor_ext::testutil::generate_events_from_spec(&[
                Existing(1),
                Existing(2),
                Existing(3),
                Idle,
                Added(4),
            ]);
            let (event_sender, event_receiver) = mpsc::unbounded();
            event_sender.unbounded_send(events).expect("failed to send events");
            let (neighbors_view, neighbors_fut) =
                fnet_neighbor_ext::testutil::create_fake_view(event_receiver);
            let _join_handle = scope.spawn(neighbors_fut);
            (neighbors_view, event_sender)
        };

        // Configure fake interface watch events.

        let (interfaces_handler, _interfaces_handler_sink) =
            crate::interfaces::testutil::FakeInterfacesHandler::new();
        let (interfaces_proxy, _interfaces) =
            fidl::endpoints::create_proxy::<fnet_root::InterfacesMarker>();
        let (interfaces_state_proxy, interfaces_state) =
            fidl::endpoints::create_proxy::<fnet_interfaces::StateMarker>();
        let route_clients = ClientTable::default();

        let interface_event_sender = {
            let if_stream = interfaces_state.into_stream();
            let if_watcher_stream = if_stream
                .and_then(|req| match req {
                    fnet_interfaces::StateRequest::GetWatcher {
                        options: _,
                        watcher,
                        control_handle: _,
                    } => futures::future::ready(Ok(watcher.into_stream())),
                })
                .try_flatten()
                .map(|res| res.expect("watcher stream error"));
            let (event_sender, event_receiver) = mpsc::unbounded();
            event_sender
                .unbounded_send(fnet_interfaces::Event::Idle(fnet_interfaces::Empty))
                .expect("failed to send event");
            let interfaces_fut =
                if_watcher_stream.zip(event_receiver).for_each(|(req, update)| async move {
                    match req {
                        fnet_interfaces::WatcherRequest::Watch { responder } => {
                            responder.send(&update).expect("send watch response")
                        }
                    }
                });
            let _join_handle = scope.spawn(interfaces_fut);
            event_sender
        };

        // Set up the event loop.

        let (_async_work_sink, async_work_receiver) = mpsc::unbounded();
        let base_inputs: EventLoopInputs<
            FakeInterfacesHandler,
            FakeSender<RouteNetlinkMessage>,
            OnlyNeighbors,
        > = EventLoopInputs {
            neighbors_view: EventLoopComponent::Present(neighbors_view),

            route_clients: EventLoopComponent::Present(route_clients),
            interfaces_handler: EventLoopComponent::Present(interfaces_handler),
            interfaces_proxy: EventLoopComponent::Present(interfaces_proxy),
            interfaces_state_proxy: EventLoopComponent::Present(interfaces_state_proxy),

            async_work_receiver,

            v4_routes_state: EventLoopComponent::Absent(Optional),
            v6_routes_state: EventLoopComponent::Absent(Optional),
            v4_main_route_table: EventLoopComponent::Absent(Optional),
            v6_main_route_table: EventLoopComponent::Absent(Optional),
            v4_route_table_provider: EventLoopComponent::Absent(Optional),
            v6_route_table_provider: EventLoopComponent::Absent(Optional),
            v4_rule_table: EventLoopComponent::Absent(Optional),
            v6_rule_table: EventLoopComponent::Absent(Optional),
            ndp_option_watcher_provider: EventLoopComponent::Absent(Optional),

            unified_request_stream: request_stream,
        };

        let mut state = base_inputs.initialize(included_workers).await;
        // Wait for `Added` event to be processed.
        state.run_one_step_in_tests().await;

        // Send a dump request and check the response.

        let (mut response_sink, neighbor_client, async_work_drain_task) =
            crate::client::testutil::new_fake_client::<NetlinkRoute>(
                crate::client::testutil::CLIENT_ID_1,
                [],
            );
        let _join_handle = scope.spawn(async_work_drain_task);

        let (completer, waiter) = oneshot::channel();
        let get_request: UnifiedRequest<FakeSender<RouteNetlinkMessage>> =
            UnifiedRequest::NeighborsRequest(Request {
                args: NeighborRequestArgs::Get(GetNeighborArgs::Dump {
                    ip_version: None,
                    interface: None,
                }),
                sequence_number: TEST_SEQUENCE_NUMBER,
                client: neighbor_client.clone(),
                completer,
            });
        request_sink.send(get_request).await.unwrap();

        // Wait for client request to be processed.
        state.run_one_step_in_tests().await;
        assert_matches!(waiter.await.unwrap(), Ok(()));

        let responses = response_sink.take_messages();
        assert_eq!(responses.len(), 4); // 3 existing + 1 added.
        for response in responses {
            assert_matches!(
                response.message.payload,
                NetlinkPayload::InnerMessage(RouteNetlinkMessage::NewNeighbour(_))
            );
        }

        neighbor_event_sender.close_channel();
        interface_event_sender.close_channel();
        drop(neighbor_client);
        scope.join().await;
    }
}
