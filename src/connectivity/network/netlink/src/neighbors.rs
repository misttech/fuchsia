// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A module for managing neighbor information by receiving RTM_*NEIGH Netlink
//! messages and maintaining neighbor table state from Netstack.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;

use crate::logging::log_warn;
use futures::StreamExt as _;
use netlink_packet_core::{NLM_F_MULTIPART, NetlinkMessage};
use netlink_packet_route::neighbour::{
    NeighbourAttribute, NeighbourHeader, NeighbourMessage, NeighbourState,
};
use netlink_packet_route::route::RouteType;
use netlink_packet_route::{AddressFamily, RouteNetlinkMessage};
use thiserror::Error;

use {
    fidl_fuchsia_net as fnet, fidl_fuchsia_net_ext as fnet_ext,
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

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
struct NeighborKey {
    interface: u64,
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
    // TODO(https://fxbug.dev/285127384): Consider whether switching to a
    // `BTreeMap` for stable iteration order would aid in testing.
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
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use fidl_fuchsia_net as fnet;
    use fidl_fuchsia_net_neighbor::ViewRequest;
    use fidl_fuchsia_net_neighbor_ext::testutil::EventSpec;
    use futures::FutureExt;
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

    #[test]
    fn neighbor_keyed_by_interface_and_ip() {
        let entry = fnet_neighbor_ext::Entry {
            interface: 1,
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

        let different_iface = fnet_neighbor_ext::Entry { interface: 2, ..entry };
        assert_ne!(NeighborKey::from(&entry), NeighborKey::from(&different_iface));

        let different_ip = fnet_neighbor_ext::Entry { neighbor: fidl_ip!("192.168.0.2"), ..entry };
        assert_ne!(NeighborKey::from(&entry), NeighborKey::from(&different_ip));

        let different_iface_and_ip =
            fnet_neighbor_ext::Entry { interface: 2, neighbor: fidl_ip!("192.168.0.2"), ..entry };
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
}
