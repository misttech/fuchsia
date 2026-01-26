// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Test utilities for the fuchsia.net.neighbors FIDL library.
//!
//! This library defines a mix of internal and external test utilities,
//! supporting tests of this `fidl_fuchsia_net_neighbor_ext` crate and tests
//! of clients of the `fuchsia.net.neighbors` FIDL library, respectively.

use futures::{Stream, StreamExt as _};
use {fidl_fuchsia_net as fnet, fidl_fuchsia_net_neighbor as fnet_neighbor};

/// Responds to the given `GetNext` request with the given batch of events.
fn handle_get_next(
    request: fnet_neighbor::EntryIteratorRequest,
    event_batch: Vec<fnet_neighbor::EntryIteratorItem>,
) {
    match request {
        fnet_neighbor::EntryIteratorRequest::GetNext { responder } => {
            responder.send(&event_batch).expect("failed to respond to `GetNext`")
        }
    }
}

/// A fake implementation of the `EntryIterator` protocol.
///
/// Feeds events received in `events` as responses to `GetNext()`.
async fn fake_entry_iterator_impl(
    events: impl Stream<Item = Vec<fnet_neighbor::EntryIteratorItem>>,
    server_end: fidl::endpoints::ServerEnd<fnet_neighbor::EntryIteratorMarker>,
) {
    let request_stream = server_end.into_stream();
    request_stream
        .zip(events)
        .for_each(|(request, event_batch)| {
            handle_get_next(request.expect("failed to receive `GetNext` request"), event_batch);
            futures::future::ready(())
        })
        .await
}

/// Serve an `OpenEntryIterator` request to the `View` protocol by instantiating an
/// entry iterator client backed by the given event stream. The returned future
/// drives the entry iterator implementation.
pub async fn serve_view_request(
    request: fnet_neighbor::ViewRequest,
    event_stream: impl Stream<Item = Vec<fnet_neighbor::EntryIteratorItem>>,
) {
    match request {
        fnet_neighbor::ViewRequest::OpenEntryIterator { it, .. } => {
            fake_entry_iterator_impl(event_stream, it)
        }
    }
    .await
}

/// Generates an arbitrary but valid neighbor entry iterator item that is unique
/// to the given `seed`.
pub(crate) fn generate_event(seed: u32) -> fnet_neighbor::EntryIteratorItem {
    fnet_neighbor::EntryIteratorItem::Added(fnet_neighbor::Entry {
        interface: Some(seed.into()),
        neighbor: Some(fnet::IpAddress::Ipv4(fnet::Ipv4Address { addr: [192, 168, 0, 1] })),
        state: Some(fnet_neighbor::EntryState::Reachable),
        mac: Some(fnet::MacAddress { octets: [0, 1, 2, 3, 4, 5] }),
        updated_at: Some(seed.into()),
        ..Default::default()
    })
}

/// Generates a list of arbitrary but valid neighbor entry iterator items, one
/// for each value in the provided range of `seeds`.
pub(crate) fn generate_events_in_range(
    seeds: std::ops::Range<u32>,
) -> Vec<fnet_neighbor::EntryIteratorItem> {
    seeds.into_iter().map(generate_event).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;
    use futures::FutureExt;
    use test_case::test_case;

    #[test_case(Vec::new(); "no events")]
    #[test_case(vec![0..1]; "single_batch_single_event")]
    #[test_case(vec![0..10]; "single_batch_many_events")]
    #[test_case(vec![0..10, 10..20, 20..30]; "many_batches_many_events")]
    #[fuchsia_async::run_singlethreaded(test)]
    async fn fake_event_iterator_impl_against_shape(test_shape: Vec<std::ops::Range<u32>>) {
        // Build the event stream based on the `test_shape`. Use a channel so
        // that the stream stays open until `close_channel` is called later.
        let (event_stream_sender, event_stream_receiver) =
            futures::channel::mpsc::unbounded::<Vec<fnet_neighbor::EntryIteratorItem>>();
        for batch_shape in &test_shape {
            event_stream_sender
                .unbounded_send(generate_events_in_range(batch_shape.clone()))
                .expect("failed to send event batch");
        }

        // Instantiate the fake EntryIterator implementation.
        let (view, view_server_end) = fidl::endpoints::create_proxy::<fnet_neighbor::ViewMarker>();
        let mut view_event_stream = view_server_end.into_stream();
        let entry_iter_fut = view_event_stream
            .next()
            .then(|req| {
                serve_view_request(
                    req.expect("view event stream unexpectedly ended")
                        .expect("failed to receive `OpenEntryIterator` request"),
                    event_stream_receiver,
                )
            })
            .fuse();
        futures::pin_mut!(entry_iter_fut);

        // Drive the event iterator, asserting it observes the expected data.
        let (entry_iter, entry_iter_server_end) =
            fidl::endpoints::create_proxy::<fnet_neighbor::EntryIteratorMarker>();
        view.open_entry_iterator(entry_iter_server_end, &Default::default())
            .expect("failed to open entry iterator");
        for batch_shape in test_shape {
            futures::select!(
                 () = entry_iter_fut => panic!("fake view implementation unexpectedly finished"),
                events = entry_iter.get_next().fuse() => assert_eq!(
                    events.expect("failed to watch for events"),
                    generate_events_in_range(batch_shape.clone())));
        }

        // Close the event_stream_sender and observe the event iterator finish.
        event_stream_sender.close_channel();
        entry_iter_fut.await;

        // Trying to watch again after we've exhausted the data should
        // result in `PEER_CLOSED`.
        assert_matches!(
            entry_iter.get_next().await,
            Err(fidl::Error::ClientChannelClosed { status: zx_status::Status::PEER_CLOSED, .. })
        );
    }
}
