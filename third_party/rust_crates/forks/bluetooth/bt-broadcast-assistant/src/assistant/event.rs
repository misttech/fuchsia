// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::pin::Pin;
use futures::stream::{FusedStream, Stream, StreamExt};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::Poll;

use bt_bap::types::{BroadcastAudioSourceEndpoint, BroadcastId};
use bt_common::core::{AdvertisingSetId, PeriodicAdvertisingInterval};
use bt_common::packet_encoding::Decodable;
use bt_common::packet_encoding::Error as PacketError;
use bt_common::PeerId;
use bt_gatt::central::{AdvertisingDatum, ScanResult};

use crate::assistant::{
    DiscoveredBroadcastSources, Error, BASIC_AUDIO_ANNOUNCEMENT_SERVICE,
    BROADCAST_AUDIO_ANNOUNCEMENT_SERVICE,
};
use crate::types::BroadcastSource;

#[derive(Debug)]
pub enum Event {
    FoundBroadcastSource { peer: PeerId, source: BroadcastSource },
    CouldNotParseAdvertisingData { peer: PeerId, error: PacketError },
}

/// A stream of discovered broadcast sources.
/// This stream polls the scan results from GATT client to discover
/// available broadcast sources.
pub struct EventStream<T: bt_gatt::GattTypes> {
    scan_result_stream: Pin<Box<<T as bt_gatt::GattTypes>::ScanResultStream>>,
    terminated: bool,

    broadcast_sources: Arc<DiscoveredBroadcastSources>,
    broadcast_source_scan_started: Arc<AtomicBool>,
}

impl<T: bt_gatt::GattTypes> EventStream<T> {
    pub(crate) fn new(
        scan_result_stream: T::ScanResultStream,
        broadcast_sources: Arc<DiscoveredBroadcastSources>,
        broadcast_source_scan_started: Arc<AtomicBool>,
    ) -> Self {
        Self {
            scan_result_stream: Box::pin(scan_result_stream),
            terminated: false,
            broadcast_sources,
            broadcast_source_scan_started,
        }
    }

    /// Returns the broadcast source if the scanned peer is a broadcast source.
    /// Returns an error if parsing of the scan result data fails and None if
    /// the scanned peer is not a broadcast source.
    fn try_into_broadcast_source(
        scan_result: &ScanResult,
    ) -> Result<Option<BroadcastSource>, PacketError> {
        let mut source = None;
        for datum in &scan_result.advertised {
            let AdvertisingDatum::ServiceData(uuid, data) = datum else {
                continue;
            };
            if *uuid == BROADCAST_AUDIO_ANNOUNCEMENT_SERVICE {
                let bid = BroadcastId::decode(data.as_slice()).0?;
                source.get_or_insert(BroadcastSource::default()).with_broadcast_id(bid);
            } else if *uuid == BASIC_AUDIO_ANNOUNCEMENT_SERVICE {
                // TODO(dayeonglee): revisit when we implement periodic advertisement.
                let base = BroadcastAudioSourceEndpoint::decode(data.as_slice()).0?;
                source.get_or_insert(BroadcastSource::default()).with_endpoint(base);
            }
        }
        if let Some(src) = &mut source {
            src.advertising_sid = scan_result.advertising_sid.map(AdvertisingSetId);
            src.periodic_advertising_interval =
                scan_result.periodic_advertising_interval.map(PeriodicAdvertisingInterval);
        }
        Ok(source)
    }
}

impl<T: bt_gatt::GattTypes> Drop for EventStream<T> {
    fn drop(&mut self) {
        self.broadcast_source_scan_started.store(false, Ordering::Relaxed);
    }
}

impl<T: bt_gatt::GattTypes> FusedStream for EventStream<T> {
    fn is_terminated(&self) -> bool {
        self.terminated
    }
}

impl<T: bt_gatt::GattTypes> Stream for EventStream<T> {
    type Item = Result<Event, Error>;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        if self.terminated {
            return Poll::Ready(None);
        }

        // Poll scan result stream to check if there were any newly discovered peers
        // that we're interested in.
        loop {
            let Some(Ok(scanned)) = futures::ready!(self.scan_result_stream.poll_next_unpin(cx))
            else {
                self.terminated = true;
                self.broadcast_source_scan_started.store(false, Ordering::Relaxed);
                return Poll::Ready(Some(Err(Error::CentralScanTerminated)));
            };

            let found_source = match Self::try_into_broadcast_source(&scanned) {
                Err(error) => {
                    return Poll::Ready(Some(Ok(Event::CouldNotParseAdvertisingData {
                        peer: scanned.id,
                        error,
                    })));
                }
                Ok(None) => continue,
                Ok(Some(source)) => source,
            };

            // If we found a broadcast source, we add its information in the
            // internal records.
            let (broadcast_source, changed) =
                self.broadcast_sources.merge_broadcast_source_data(&scanned.id, &found_source);

            // Broadcast found event is relayed to the client iff complete
            // information has been gathered.
            if broadcast_source.into_add_source() && changed {
                return Poll::Ready(Some(Ok(Event::FoundBroadcastSource {
                    peer: scanned.id,
                    source: broadcast_source,
                })));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use assert_matches::assert_matches;

    use bt_common::core::{AddressType, AdvertisingSetId};
    use bt_gatt::central::{AdvertisingDatum, PeerName};
    use bt_gatt::test_utils::{FakeTypes, ScannedResultStream, ScannedResultStreamController};
    use bt_gatt::types::Error as BtGattError;
    use bt_gatt::types::GattError;

    fn setup_stream() -> (EventStream<FakeTypes>, ScannedResultStreamController) {
        let fake_scan_result_stream = ScannedResultStream::new();
        let controller = fake_scan_result_stream.controller();
        let broadcast_sources = DiscoveredBroadcastSources::new();
        let broadcast_source_scan_started = Arc::new(AtomicBool::new(false));

        (
            EventStream::<FakeTypes>::new(
                fake_scan_result_stream,
                broadcast_sources,
                broadcast_source_scan_started,
            ),
            controller,
        )
    }

    #[test]
    fn poll_found_broadcast_source_events() {
        let (mut stream, scan_result_controller) = setup_stream();

        // Scanned a broadcast source and its broadcast id.
        let broadcast_source_pid = PeerId(1005);

        scan_result_controller.add_scanned_result(Ok(ScanResult {
            id: broadcast_source_pid,
            connectable: true,
            name: PeerName::Unknown,
            advertised: vec![AdvertisingDatum::ServiceData(
                BROADCAST_AUDIO_ANNOUNCEMENT_SERVICE,
                vec![0x01, 0x02, 0x03],
            )],
            advertising_sid: Some(0),
            periodic_advertising_interval: None,
        }));

        // Found broadcast source event shouldn't have been sent since braodcast source
        // information isn't complete.
        let mut noop_cx = futures::task::Context::from_waker(futures::task::noop_waker_ref());
        assert!(stream.poll_next_unpin(&mut noop_cx).is_pending());

        // Pretend somehow address, address type, and advertising sid were
        // filled out. This completes the broadcast source information.
        // TODO(b/308481381): replace this block with sending a central scan result that
        // contains the data.
        let _ = stream.broadcast_sources.merge_broadcast_source_data(
            &broadcast_source_pid,
            &BroadcastSource::default()
                .with_address([1, 2, 3, 4, 5, 6])
                .with_address_type(AddressType::Public)
                .with_advertising_sid(AdvertisingSetId(1)),
        );

        // Scanned broadcast source's BASE data.
        // TODO(b/308481381): replace this block sending data through PA train instead.
        #[rustfmt::skip]
        let base_data = vec![
            0x10, 0x20, 0x30, 0x02, // presentation delay, num of subgroups
            0x01, 0x03, 0x00, 0x00, 0x00, 0x00, // num of bis, codec id (big #1)
            0x00, // codec specific config len
            0x00, // metadata len,
            0x01, 0x00, // bis index, codec specific config len (big #1 / bis #1)
            0x01, 0x02, 0x00, 0x00, 0x00, 0x00, // num of bis, codec id (big #2)
            0x00, // codec specific config len
            0x00, // metadata len,
            0x01, 0x03, 0x02, 0x05,
            0x08, /* bis index, codec specific config len, codec frame blocks LTV
                    * (big #2 / bis #2) */
        ];

        scan_result_controller.add_scanned_result(Ok(ScanResult {
            id: broadcast_source_pid,
            connectable: true,
            name: PeerName::Unknown,
            advertised: vec![AdvertisingDatum::ServiceData(
                BASIC_AUDIO_ANNOUNCEMENT_SERVICE,
                base_data.clone(),
            )],
            advertising_sid: Some(1),
            periodic_advertising_interval: Some(0x0100),
        }));

        // Expect the stream to send out broadcast source found event since information
        // is complete.
        let Poll::Ready(Some(Ok(event))) = stream.poll_next_unpin(&mut noop_cx) else {
            panic!("should have received event");
        };
        assert_matches!(event, Event::FoundBroadcastSource{peer, source} => {
            assert_eq!(peer, broadcast_source_pid);
            assert_eq!(source.advertising_sid, Some(AdvertisingSetId(1)));
            assert_eq!(source.periodic_advertising_interval, Some(PeriodicAdvertisingInterval(0x0100)));
        });

        assert!(stream.poll_next_unpin(&mut noop_cx).is_pending());

        // Scanned the same broadcast source's BASE data.
        scan_result_controller.add_scanned_result(Ok(ScanResult {
            id: broadcast_source_pid,
            connectable: true,
            name: PeerName::Unknown,
            advertised: vec![AdvertisingDatum::ServiceData(
                BASIC_AUDIO_ANNOUNCEMENT_SERVICE,
                base_data.clone(),
            )],
            advertising_sid: Some(1),
            periodic_advertising_interval: Some(0x0100),
        }));

        // Shouldn't have gotten the event again since the information remained the
        // same.
        assert!(stream.poll_next_unpin(&mut noop_cx).is_pending());
    }

    #[test]
    fn central_scan_stream_terminates() {
        let (mut stream, scan_result_controller) = setup_stream();

        // Mimick scan error.
        scan_result_controller.add_scanned_result(Err(BtGattError::Gatt(GattError::InvalidPdu)));

        let mut noop_cx = futures::task::Context::from_waker(futures::task::noop_waker_ref());
        match stream.poll_next_unpin(&mut noop_cx) {
            Poll::Ready(Some(Err(e))) => assert_matches!(e, Error::CentralScanTerminated),
            _ => panic!("should have received central scan terminated error"),
        }

        // Entire stream should have terminated.
        assert_matches!(stream.poll_next_unpin(&mut noop_cx), Poll::Ready(None));
        assert_matches!(stream.is_terminated(), true);
    }

    #[test]
    fn poll_processes_multiple_results_eagerly() {
        let (mut stream, scan_result_controller) = setup_stream();
        let mut noop_cx = futures::task::Context::from_waker(futures::task::noop_waker_ref());

        let broadcast_source_pid = PeerId(1005);

        #[rustfmt::skip]
        let base_data = vec![
            0x10, 0x20, 0x30, 0x01, // presentation delay, num of subgroups
            0x01, 0x03, 0x00, 0x00, 0x00, 0x00, // num of bis, codec id
            0x00, // codec specific config len
            0x00, // metadata len,
            0x01, 0x00, // bis index, codec specific config len
        ];

        // 1. An irrelevant peer that should be ignored.
        scan_result_controller.add_scanned_result(Ok(ScanResult {
            id: PeerId(1),
            connectable: true,
            name: PeerName::Unknown,
            advertised: vec![],
            advertising_sid: Some(0),
            periodic_advertising_interval: None,
        }));
        // 2. A broadcast source, but incomplete data (only broadcast ID).
        scan_result_controller.add_scanned_result(Ok(ScanResult {
            id: broadcast_source_pid,
            connectable: true,
            name: PeerName::Unknown,
            advertised: vec![AdvertisingDatum::ServiceData(
                BROADCAST_AUDIO_ANNOUNCEMENT_SERVICE,
                vec![0x01, 0x02, 0x03],
            )],
            advertising_sid: Some(1),
            periodic_advertising_interval: None,
        }));
        // 3. The same broadcast source, with BASE data, which completes it.
        scan_result_controller.add_scanned_result(Ok(ScanResult {
            id: broadcast_source_pid,
            connectable: true,
            name: PeerName::Unknown,
            advertised: vec![AdvertisingDatum::ServiceData(
                BASIC_AUDIO_ANNOUNCEMENT_SERVICE,
                base_data.clone(),
            )],
            advertising_sid: Some(1),
            periodic_advertising_interval: None,
        }));

        // The stream should eagerly process all three items and emit the
        // FoundBroadcastSource event.
        let poll_result = stream.poll_next_unpin(&mut noop_cx);
        let Poll::Ready(Some(Ok(event))) = poll_result else {
            panic!("should have received event, but got {:?}", poll_result);
        };
        assert_matches!(event, Event::FoundBroadcastSource{peer, ..} => {
            assert_eq!(peer, broadcast_source_pid);
        });

        // The underlying stream is now empty, so the next poll should be pending.
        assert!(stream.poll_next_unpin(&mut noop_cx).is_pending());
    }
}
