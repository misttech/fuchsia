// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::blob_sink::{BlobSink, WriteAttempt};

/// Maximum number of Perfetto bytes carried by one FXT blob record.
///
/// The drain writes each Perfetto packet, framed as a field of a `Trace` message, as one
/// record, and splits only framed packets longer than this limit. Packets can be that long:
/// upstream traced_probes can put the ftrace events from several ring-buffer pages into one
/// `FtraceEventBundle` packet.
///
/// The value balances these limits:
/// - An FXT blob record holds at most `fxt::MAX_RECORD_SIZE_WORDS` words: the header word,
///   the inline blob name and the payload, each padded to 8 bytes (see
///   [`fxt::blob::build_blob_record()`]).
/// - Bigger blobs split a long packet into fewer records, so there is less header and name
///   overhead per byte.
/// - A record that does not fit in the rest of the current rolling buffer goes into the next
///   one, and the rest of the current one stays unused, so each buffer switch can waste up to
///   one record. With trace_manager's minimum default trace buffer, 1 MiB, each rolling
///   buffer is about 480 KiB, so a 4 KiB blob wastes under 1% of one. A per-provider buffer
///   size is also a whole number of MiB, so every provider buffer that can hold records is
///   at least that large.
pub(crate) const MAX_BLOB_PAYLOAD_SIZE: usize = 4096;

/// How long to wait after a streaming write finds the trace buffer full before trying again.
///
/// trace-engine asks trace_manager to save a full rolling buffer when the last writer releases
/// its trace context. Every attempt releases its context before the wait, so space frees up
/// once trace_manager has saved the buffer.
const STREAMING_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);

/// Waits between the forwarder's write attempts. Tests replace it with a fake that records
/// the waits instead of sleeping.
pub(crate) trait Clock {
    /// Blocks the calling thread for `duration`.
    fn sleep(&mut self, duration: zx::MonotonicDuration);
}

/// The monotonic clock.
pub(crate) struct MonotonicClock;

impl Clock for MonotonicClock {
    fn sleep(&mut self, duration: zx::MonotonicDuration) {
        duration.sleep();
    }
}

/// Why one blob could not be written.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BlobWriteError {
    /// The buffer is full, and the buffering mode doesn't wait for room (see
    /// `PacketForwarder::retry_on_full`).
    NoSpace,
    /// The blob does not fit in one record.
    TooLong,
    /// No trace context could be acquired.
    TracingStopped,
}

/// Forwards Perfetto packets to the Fuchsia trace buffer during one `TraceState::Stopping`
/// drain.
pub(crate) struct PacketForwarder<S, C> {
    sink: S,
    clock: C,
    /// Whether to wait for room when the trace buffer is full. Only streaming mode waits, for
    /// trace_manager to save the full rolling buffer. In oneshot mode the buffer never gains
    /// room. In circular mode the last writer to release its context frees the full rolling
    /// buffer for reuse, so room usually comes back quickly, but losing the record is
    /// acceptable there, so the drain doesn't wait.
    retry_on_full: bool,
    pub(crate) packets_written: usize,
    /// Packets that were not written in full.
    pub(crate) packets_dropped: usize,
    /// Bytes in the packets counted by `packets_dropped`.
    pub(crate) bytes_dropped: usize,
}

impl<S: BlobSink, C: Clock> PacketForwarder<S, C> {
    pub(crate) fn new(sink: S, clock: C, retry_on_full: bool) -> Self {
        Self {
            sink,
            clock,
            retry_on_full,
            packets_written: 0,
            packets_dropped: 0,
            bytes_dropped: 0,
        }
    }

    /// Writes `packet` as one or more blob records, and stops at the first blob that can't be
    /// written. The blobs written before that one stay in the trace buffer, so the packet is
    /// cut off. The drain never passes an empty packet: each one starts with a field tag and a
    /// length.
    pub(crate) fn forward_packet(&mut self, packet: &[u8]) {
        for chunk in packet.chunks(MAX_BLOB_PAYLOAD_SIZE) {
            if self.write_blob(chunk).is_err() {
                self.drop_packet(packet.len());
                return;
            }
        }
        self.packets_written += 1;
    }

    /// Counts a packet of `len` bytes that is not written.
    fn drop_packet(&mut self, len: usize) {
        self.packets_dropped += 1;
        self.bytes_dropped += len;
    }

    /// Writes `chunk` as one blob record. With `retry_on_full`, retries for as long as the
    /// buffer stays full.
    fn write_blob(&mut self, chunk: &[u8]) -> Result<(), BlobWriteError> {
        loop {
            match self.sink.try_write_blob(chunk) {
                WriteAttempt::Wrote => return Ok(()),
                WriteAttempt::TracingStopped => return Err(BlobWriteError::TracingStopped),
                WriteAttempt::TooLong => return Err(BlobWriteError::TooLong),
                WriteAttempt::BufferFull if !self.retry_on_full => {
                    return Err(BlobWriteError::NoSpace);
                }
                WriteAttempt::BufferFull => {
                    // TODO(b/304532640): Revisit polling for room at a fixed delay.
                    self.clock.sleep(STREAMING_RETRY_DELAY);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::test_bytes;
    use std::collections::VecDeque;

    /// A `BlobSink` that returns scripted write results.
    struct FakeSink {
        /// Results for the next attempts. Once empty, every attempt returns `default_result`.
        scripted_results: VecDeque<WriteAttempt>,
        default_result: WriteAttempt,
        attempts: usize,
        written: Vec<Vec<u8>>,
    }

    impl FakeSink {
        fn new(scripted_results: &[WriteAttempt], default_result: WriteAttempt) -> Self {
            Self {
                scripted_results: scripted_results.iter().copied().collect(),
                default_result,
                attempts: 0,
                written: Vec::new(),
            }
        }
    }

    impl BlobSink for FakeSink {
        fn try_write_blob(&mut self, blob_data: &[u8]) -> WriteAttempt {
            self.attempts += 1;
            let result = self.scripted_results.pop_front().unwrap_or(self.default_result);
            if result == WriteAttempt::Wrote {
                self.written.push(blob_data.to_vec());
            }
            result
        }
    }

    /// A `Clock` that records sleeps instead of sleeping.
    #[derive(Default)]
    struct FakeClock {
        sleeps: Vec<zx::MonotonicDuration>,
    }

    impl Clock for FakeClock {
        fn sleep(&mut self, duration: zx::MonotonicDuration) {
            self.sleeps.push(duration);
        }
    }

    fn new_forwarder(sink: FakeSink, retry_on_full: bool) -> PacketForwarder<FakeSink, FakeClock> {
        PacketForwarder::new(sink, FakeClock::default(), retry_on_full)
    }

    #[fuchsia::test]
    fn forward_packet_splits_packet_into_chunks() {
        let data = test_bytes(2 * MAX_BLOB_PAYLOAD_SIZE + 10);
        let mut forwarder = new_forwarder(FakeSink::new(&[], WriteAttempt::Wrote), true);

        forwarder.forward_packet(&data);

        assert_eq!(forwarder.packets_written, 1, "forward_packet(len {})", data.len());
        let lens: Vec<usize> = forwarder.sink.written.iter().map(Vec::len).collect();
        assert_eq!(
            lens,
            vec![MAX_BLOB_PAYLOAD_SIZE, MAX_BLOB_PAYLOAD_SIZE, 10],
            "forward_packet(len {}) blob lengths",
            data.len()
        );
        assert_eq!(
            forwarder.sink.written.concat(),
            data,
            "forward_packet(len {}) bytes",
            data.len()
        );
    }

    #[fuchsia::test]
    fn forward_packet_chunk_boundaries() {
        let cases: [(usize, &[usize]); 3] = [
            (MAX_BLOB_PAYLOAD_SIZE, &[MAX_BLOB_PAYLOAD_SIZE]),
            (MAX_BLOB_PAYLOAD_SIZE + 1, &[MAX_BLOB_PAYLOAD_SIZE, 1]),
            (2 * MAX_BLOB_PAYLOAD_SIZE, &[MAX_BLOB_PAYLOAD_SIZE; 2]),
        ];
        for (len, expected_lens) in cases {
            let data = test_bytes(len);
            let mut forwarder = new_forwarder(FakeSink::new(&[], WriteAttempt::Wrote), true);

            forwarder.forward_packet(&data);

            assert_eq!(forwarder.packets_written, 1, "forward_packet(len {len})");
            assert_eq!(
                forwarder.sink.attempts,
                expected_lens.len(),
                "forward_packet(len {len}) attempts"
            );
            let lens: Vec<usize> = forwarder.sink.written.iter().map(Vec::len).collect();
            assert_eq!(lens, expected_lens, "forward_packet(len {len}) blob lengths");
            assert_eq!(forwarder.sink.written.concat(), data, "forward_packet(len {len}) bytes");
        }
    }

    #[fuchsia::test]
    fn forward_packet_waits_for_streaming_buffer_space() {
        let data = test_bytes(10);
        let sink = FakeSink::new(&[WriteAttempt::BufferFull; 2], WriteAttempt::Wrote);
        let mut forwarder = new_forwarder(sink, true);

        forwarder.forward_packet(&data);

        assert_eq!(forwarder.packets_written, 1, "packet after 2 full attempts");
        assert_eq!(
            forwarder.clock.sleeps,
            vec![STREAMING_RETRY_DELAY; 2],
            "packet after 2 full attempts"
        );
        assert_eq!(forwarder.sink.written, vec![data], "packet after 2 full attempts");
    }

    #[fuchsia::test]
    fn forward_packet_stops_retrying_when_tracing_stops() {
        let data = test_bytes(10);
        let script = [WriteAttempt::BufferFull, WriteAttempt::TracingStopped];
        let mut forwarder = new_forwarder(FakeSink::new(&script, WriteAttempt::Wrote), true);

        forwarder.forward_packet(&data);

        assert_eq!(forwarder.packets_written, 0, "full, then tracing stopped");
        assert_eq!(forwarder.packets_dropped, 1, "full, then tracing stopped");
        assert_eq!(forwarder.bytes_dropped, data.len(), "full, then tracing stopped");
        assert_eq!(forwarder.sink.attempts, 2, "full, then tracing stopped: no more attempts");
        assert_eq!(
            forwarder.clock.sleeps,
            vec![STREAMING_RETRY_DELAY],
            "full, then tracing stopped"
        );
        assert!(forwarder.sink.written.is_empty(), "full, then tracing stopped");
    }

    #[fuchsia::test]
    fn forward_packet_stops_at_a_blob_that_cannot_be_written() {
        // Only a full buffer with `retry_on_full` is retried. `TraceBufferSink` returns `TooLong`
        // only for a blob that does not fit in one record, which `forward_packet()` never
        // passes, so the fake stands in for it here to check that `write_blob()` gives up on
        // such a blob instead of retrying it.
        let cases = [
            (WriteAttempt::BufferFull, false),
            (WriteAttempt::TracingStopped, true),
            (WriteAttempt::TooLong, true),
        ];
        for (result, retry_on_full) in cases {
            let data = test_bytes(2 * MAX_BLOB_PAYLOAD_SIZE + 10);
            // Every attempt after the failed one would succeed.
            let sink = FakeSink::new(&[WriteAttempt::Wrote, result], WriteAttempt::Wrote);
            let mut forwarder = new_forwarder(sink, retry_on_full);
            let case = format!("second chunk got {result:?}, retry_on_full {retry_on_full}");

            forwarder.forward_packet(&data);

            assert_eq!(forwarder.packets_written, 0, "{case}");
            assert_eq!(forwarder.packets_dropped, 1, "{case}");
            assert_eq!(forwarder.bytes_dropped, data.len(), "{case}");
            assert_eq!(
                forwarder.sink.attempts, 2,
                "{case}: no retry, and no attempt for the third chunk"
            );
            assert!(
                forwarder.clock.sleeps.is_empty(),
                "{case}: sleeps {:?}",
                forwarder.clock.sleeps
            );
            assert_eq!(
                forwarder.sink.written,
                vec![data[..MAX_BLOB_PAYLOAD_SIZE].to_vec()],
                "{case}: only the first chunk is written"
            );
        }
    }

    #[fuchsia::test]
    fn forward_packet_drops_packet_and_continues_without_retry_on_full() {
        let sink = FakeSink::new(&[WriteAttempt::BufferFull], WriteAttempt::Wrote);
        let mut forwarder = new_forwarder(sink, false);

        forwarder.forward_packet(&test_bytes(100));
        // A later, smaller packet may still fit, so the drain keeps going.
        forwarder.forward_packet(&test_bytes(10));

        assert_eq!(forwarder.packets_dropped, 1, "no retry_on_full");
        assert_eq!(forwarder.bytes_dropped, 100, "no retry_on_full");
        assert_eq!(forwarder.packets_written, 1, "no retry_on_full");
        assert!(forwarder.clock.sleeps.is_empty(), "sleeps {:?}", forwarder.clock.sleeps);
        assert_eq!(forwarder.sink.written, vec![test_bytes(10)], "no retry_on_full");
    }
}
