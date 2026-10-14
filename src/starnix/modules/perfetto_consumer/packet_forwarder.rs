// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::blob_sink::{BlobSink, WriteAttempt};
use perfetto_protos::perfetto::protos::read_buffers_response::Slice;
use starnix_logging::log_warn;

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
/// once trace_manager has saved the buffer. `DRAIN_TIMEOUT`, not this delay, decides when to
/// give up.
const STREAMING_RETRY_DELAY: zx::MonotonicDuration = zx::MonotonicDuration::from_millis(100);

/// The forwarder's clock: it reads the time to enforce the drain's time limits, and waits
/// between write attempts. Tests replace it with a fake.
pub(crate) trait Clock {
    /// Returns the current time.
    fn now(&self) -> zx::MonotonicInstant;
    /// Blocks the calling thread for `duration`.
    fn sleep(&mut self, duration: zx::MonotonicDuration);
}

/// The monotonic clock. trace_manager's stop timeout runs on the same clock.
pub(crate) struct MonotonicClock;

impl Clock for MonotonicClock {
    fn now(&self) -> zx::MonotonicInstant {
        zx::MonotonicInstant::get()
    }

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
    /// The buffer stayed full until the drain deadline passed.
    DeadlineExpired,
    /// No trace context could be acquired.
    TracingStopped,
}

/// Why a drain stopped forwarding Perfetto packets before it ran out of packets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StopReason {
    /// The drain deadline passed.
    DeadlineExpired,
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
    /// When the drain stops writing.
    deadline: zx::MonotonicInstant,
    /// Why the drain stopped writing, or `None` if it hasn't.
    pub(crate) stop_reason: Option<StopReason>,
    /// Bytes of the current packet that arrived after the drain stopped (see `skip_slice()`).
    skipped_packet_len: usize,
    pub(crate) packets_written: usize,
    /// Packets that were not written in full.
    pub(crate) packets_dropped: usize,
    /// Bytes in the packets counted by `packets_dropped`. Packets skipped after the drain
    /// stopped are counted before framing and PID rewriting, so this is approximate.
    pub(crate) bytes_dropped: usize,
}

impl<S: BlobSink, C: Clock> PacketForwarder<S, C> {
    pub(crate) fn new(
        sink: S,
        clock: C,
        retry_on_full: bool,
        deadline: zx::MonotonicInstant,
    ) -> Self {
        Self {
            sink,
            clock,
            retry_on_full,
            deadline,
            stop_reason: None,
            skipped_packet_len: 0,
            packets_written: 0,
            packets_dropped: 0,
            bytes_dropped: 0,
        }
    }

    /// Whether the drain has stopped writing. Once stopped, it never writes again.
    fn is_stopped(&self) -> bool {
        self.stop_reason.is_some()
    }

    /// Stops the drain if its deadline has passed, then returns whether the drain has stopped
    /// for any reason (see `stop_reason`), not only for the deadline.
    fn check_stopped(&mut self) -> bool {
        if !self.is_stopped() && self.clock.now() >= self.deadline {
            self.stop(StopReason::DeadlineExpired);
        }
        self.is_stopped()
    }

    /// Writes `packet` as one or more blob records, and stops at the first blob that can't be
    /// written. The blobs written before that one stay in the trace buffer, so the packet is
    /// cut off. Checks the deadline first, and once the drain has stopped (see `stop_reason`),
    /// drops `packet` without trying to write it. The drain never passes an empty packet: each
    /// one starts with a field tag and a length.
    fn forward_packet(&mut self, packet: &[u8]) {
        if self.check_stopped() {
            self.drop_packet(packet.len());
            return;
        }
        for chunk in packet.chunks(MAX_BLOB_PAYLOAD_SIZE) {
            if let Err(error) = self.write_blob(chunk) {
                match error {
                    BlobWriteError::NoSpace | BlobWriteError::TooLong => {}
                    BlobWriteError::DeadlineExpired => self.stop(StopReason::DeadlineExpired),
                    BlobWriteError::TracingStopped => self.stop(StopReason::TracingStopped),
                }
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

    /// Counts `len` bytes of a packet that arrived after the drain stopped, and that the drain
    /// therefore doesn't copy, frame or rewrite. `last_slice` ends the packet, which is then
    /// counted as dropped.
    fn skip_slice(&mut self, len: usize, last_slice: bool) {
        self.skipped_packet_len += len;
        if last_slice {
            let packet_len = std::mem::take(&mut self.skipped_packet_len);
            self.drop_packet(packet_len);
        }
    }

    /// Writes `chunk` as one blob record. With `retry_on_full`, retries until the drain deadline
    /// passes.
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
                    let now = self.clock.now();
                    if now >= self.deadline {
                        return Err(BlobWriteError::DeadlineExpired);
                    }
                    // TODO(b/304532640): Revisit polling for room at a fixed delay.
                    self.clock.sleep(STREAMING_RETRY_DELAY.min(self.deadline - now));
                }
            }
        }
    }

    /// Stops the drain for `reason`. Every later packet is dropped.
    fn stop(&mut self, reason: StopReason) {
        self.stop_reason = Some(reason);
        match reason {
            StopReason::DeadlineExpired => {
                log_warn!("Perfetto drain deadline passed; dropping the remaining Perfetto packets")
            }
            StopReason::TracingStopped => log_warn!(
                "Could not acquire a trace context during a Perfetto drain; dropping the \
                 remaining Perfetto packets"
            ),
        }
    }
}

/// Forwards the packets in `slices`, from one ReadBuffers reply, to `forwarder`.
///
/// A packet can span several slices, and several replies. `packet_data` holds the slices of the
/// current packet read so far, so it is empty between packets. When the last slice of a packet
/// arrives, the packet is framed as a field of a `Trace` message, passed through
/// `rewrite_pids`, and forwarded.
///
/// Once the forwarder has stopped it never writes again. So from then on the slices, and the
/// part of the current packet read before the stop, are only counted (see
/// `PacketForwarder::skip_slice()`): they are not copied, framed or passed to `rewrite_pids`.
pub(crate) fn forward_slices<S: BlobSink, C: Clock>(
    slices: &[Slice],
    packet_data: &mut Vec<u8>,
    forwarder: &mut PacketForwarder<S, C>,
    mut rewrite_pids: impl FnMut(Vec<u8>) -> Vec<u8>,
) {
    for slice in slices {
        let data = slice.data.as_deref().unwrap_or(&[]);
        let last_slice = slice.last_slice_for_packet.unwrap_or(false);
        if forwarder.check_stopped() {
            forwarder.skip_slice(packet_data.len() + data.len(), last_slice);
            packet_data.clear();
            continue;
        }
        packet_data.extend(data);
        if !last_slice {
            continue;
        }
        let mut blob_data = Vec::new();
        // Packet field number = 1, length delimited type = 2.
        blob_data.push(1 << 3 | 2);
        // Push a varint encoded length.
        // See https://protobuf.dev/programming-guides/encoding/
        const HIGH_BIT: u8 = 0x80;
        const LOW_SEVEN_BITS: usize = 0x7F;
        let mut value = packet_data.len();
        while value >= HIGH_BIT as usize {
            blob_data.push((value & LOW_SEVEN_BITS) as u8 | HIGH_BIT);
            value >>= 7;
        }
        blob_data.push(value as u8);
        // `append` moves all data out of the passed Vec, so `packet_data` will be empty after
        // this call.
        blob_data.append(packet_data);

        // At this point blob_data is a full Perfetto Trace protobuf. Parse the data and replace
        // the linux pids with their corresponding koid.
        let rewritten = rewrite_pids(blob_data);

        // A packet that can't be written in full is counted as dropped. Unless the drain has
        // stopped, we move on to the next one, since in oneshot mode a smaller packet may still
        // fit. If the packet spans several blobs, the ones written before the failure stay in
        // the trace buffer. trace_processor joins all Perfetto blob payloads into one proto
        // stream, so it then reads the packets after the cut-off one as the rest of it, which
        // corrupts the rest of the Perfetto data in the trace.
        // TODO(b/571694521): Stop the drain at a cut-off packet (fxr/1881111).
        forwarder.forward_packet(&rewritten);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DRAIN_TIMEOUT;
    use crate::testing::test_bytes;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

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

    /// A `Clock` that only moves when the forwarder sleeps or the test advances it. Clones
    /// share the same time.
    #[derive(Clone)]
    struct FakeClock {
        now: Rc<Cell<zx::MonotonicInstant>>,
        sleeps: Rc<RefCell<Vec<zx::MonotonicDuration>>>,
    }

    impl FakeClock {
        fn new() -> Self {
            Self {
                now: Rc::new(Cell::new(zx::MonotonicInstant::from_nanos(1_000_000_000))),
                sleeps: Rc::default(),
            }
        }

        fn advance(&self, duration: zx::MonotonicDuration) {
            self.now.set(self.now.get() + duration);
        }

        fn sleeps(&self) -> Vec<zx::MonotonicDuration> {
            self.sleeps.borrow().clone()
        }
    }

    impl Clock for FakeClock {
        fn now(&self) -> zx::MonotonicInstant {
            self.now.get()
        }

        fn sleep(&mut self, duration: zx::MonotonicDuration) {
            self.sleeps.borrow_mut().push(duration);
            self.advance(duration);
        }
    }

    /// Returns a forwarder whose drain deadline is `timeout` from now on `clock`.
    fn new_forwarder(
        sink: FakeSink,
        clock: &FakeClock,
        retry_on_full: bool,
        timeout: zx::MonotonicDuration,
    ) -> PacketForwarder<FakeSink, FakeClock> {
        let deadline = clock.now() + timeout;
        PacketForwarder::new(sink, clock.clone(), retry_on_full, deadline)
    }

    fn new_streaming_forwarder(
        sink: FakeSink,
        clock: &FakeClock,
        timeout: zx::MonotonicDuration,
    ) -> PacketForwarder<FakeSink, FakeClock> {
        new_forwarder(sink, clock, true, timeout)
    }

    #[fuchsia::test]
    fn forward_packet_splits_packet_into_chunks() {
        let data = test_bytes(2 * MAX_BLOB_PAYLOAD_SIZE + 10);
        let clock = FakeClock::new();
        let mut forwarder =
            new_streaming_forwarder(FakeSink::new(&[], WriteAttempt::Wrote), &clock, DRAIN_TIMEOUT);

        forwarder.forward_packet(&data);

        assert_eq!(forwarder.packets_written, 1, "forward_packet(len {})", data.len());
        assert_eq!(forwarder.stop_reason, None, "forward_packet(len {})", data.len());
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
            let clock = FakeClock::new();
            let mut forwarder = new_streaming_forwarder(
                FakeSink::new(&[], WriteAttempt::Wrote),
                &clock,
                DRAIN_TIMEOUT,
            );

            forwarder.forward_packet(&data);

            assert_eq!(forwarder.packets_written, 1, "forward_packet(len {len})");
            assert_eq!(forwarder.stop_reason, None, "forward_packet(len {len})");
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
        let clock = FakeClock::new();
        let sink = FakeSink::new(&[WriteAttempt::BufferFull; 2], WriteAttempt::Wrote);
        let mut forwarder = new_streaming_forwarder(sink, &clock, DRAIN_TIMEOUT);

        forwarder.forward_packet(&data);

        assert_eq!(forwarder.packets_written, 1, "packet after 2 full attempts");
        assert_eq!(forwarder.stop_reason, None, "packet after 2 full attempts");
        assert_eq!(clock.sleeps(), vec![STREAMING_RETRY_DELAY; 2], "packet after 2 full attempts");
        assert_eq!(forwarder.sink.written, vec![data], "packet after 2 full attempts");
    }

    #[fuchsia::test]
    fn forward_packet_stops_retrying_when_tracing_stops() {
        let data = test_bytes(10);
        let clock = FakeClock::new();
        let script = [WriteAttempt::BufferFull, WriteAttempt::TracingStopped];
        let sink = FakeSink::new(&script, WriteAttempt::Wrote);
        let mut forwarder = new_streaming_forwarder(sink, &clock, DRAIN_TIMEOUT);

        forwarder.forward_packet(&data);

        assert_eq!(
            forwarder.stop_reason,
            Some(StopReason::TracingStopped),
            "full, then tracing stopped"
        );
        assert_eq!(forwarder.packets_written, 0, "full, then tracing stopped");
        assert_eq!(forwarder.packets_dropped, 1, "full, then tracing stopped");
        assert_eq!(forwarder.bytes_dropped, data.len(), "full, then tracing stopped");
        assert_eq!(forwarder.sink.attempts, 2, "full, then tracing stopped: no more attempts");
        assert_eq!(clock.sleeps(), vec![STREAMING_RETRY_DELAY], "full, then tracing stopped");
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
            let clock = FakeClock::new();
            // Every attempt after the failed one would succeed.
            let sink = FakeSink::new(&[WriteAttempt::Wrote, result], WriteAttempt::Wrote);
            let mut forwarder = new_forwarder(sink, &clock, retry_on_full, DRAIN_TIMEOUT);
            let case = format!("second chunk got {result:?}, retry_on_full {retry_on_full}");

            forwarder.forward_packet(&data);

            assert_eq!(forwarder.packets_written, 0, "{case}");
            assert_eq!(forwarder.packets_dropped, 1, "{case}");
            assert_eq!(forwarder.bytes_dropped, data.len(), "{case}");
            assert_eq!(
                forwarder.sink.attempts, 2,
                "{case}: no retry, and no attempt for the third chunk"
            );
            assert!(clock.sleeps().is_empty(), "{case}: sleeps {:?}", clock.sleeps());
            assert_eq!(
                forwarder.sink.written,
                vec![data[..MAX_BLOB_PAYLOAD_SIZE].to_vec()],
                "{case}: only the first chunk is written"
            );
        }
    }

    #[fuchsia::test]
    fn forward_packet_drops_packet_and_continues_without_retry_on_full() {
        let clock = FakeClock::new();
        let sink = FakeSink::new(&[WriteAttempt::BufferFull], WriteAttempt::Wrote);
        let mut forwarder = new_forwarder(sink, &clock, false, DRAIN_TIMEOUT);

        forwarder.forward_packet(&test_bytes(100));
        // A later, smaller packet may still fit, so the drain keeps going.
        forwarder.forward_packet(&test_bytes(10));

        assert_eq!(forwarder.stop_reason, None, "no retry_on_full");
        assert_eq!(forwarder.packets_dropped, 1, "no retry_on_full");
        assert_eq!(forwarder.bytes_dropped, 100, "no retry_on_full");
        assert_eq!(forwarder.packets_written, 1, "no retry_on_full");
        assert!(clock.sleeps().is_empty(), "no retry_on_full: sleeps {:?}", clock.sleeps());
        assert_eq!(forwarder.sink.written, vec![test_bytes(10)], "no retry_on_full");
    }

    #[fuchsia::test]
    fn forward_packet_drops_packet_and_stops_at_deadline() {
        let timeout = STREAMING_RETRY_DELAY * 2 + STREAMING_RETRY_DELAY / 2;
        let data = test_bytes(2 * MAX_BLOB_PAYLOAD_SIZE);
        let clock = FakeClock::new();
        let sink = FakeSink::new(&[WriteAttempt::Wrote], WriteAttempt::BufferFull);
        let mut forwarder = new_streaming_forwarder(sink, &clock, timeout);

        forwarder.forward_packet(&test_bytes(10));
        forwarder.forward_packet(&data);
        let attempts = forwarder.sink.attempts;
        forwarder.forward_packet(&test_bytes(10));

        assert_eq!(
            forwarder.stop_reason,
            Some(StopReason::DeadlineExpired),
            "always-full buffer after 1 packet, timeout {timeout:?}"
        );
        assert_eq!(forwarder.packets_written, 1, "always-full buffer after 1 packet");
        assert_eq!(forwarder.packets_dropped, 2, "always-full buffer after 1 packet");
        assert_eq!(forwarder.bytes_dropped, data.len() + 10, "always-full buffer after 1 packet");
        // The last wait is cut short so the drain does not run past its deadline.
        assert_eq!(
            clock.sleeps(),
            vec![STREAMING_RETRY_DELAY, STREAMING_RETRY_DELAY, STREAMING_RETRY_DELAY / 2],
            "always-full buffer after 1 packet, timeout {timeout:?}"
        );
        assert_eq!(
            forwarder.sink.attempts, attempts,
            "no write attempts after the drain stopped, timeout {timeout:?}"
        );
    }

    #[fuchsia::test]
    fn forward_packet_stops_at_deadline_while_writes_succeed() {
        // The deadline also covers time spent outside of writes, such as reading and rewriting
        // packets.
        let clock = FakeClock::new();
        let mut forwarder =
            new_streaming_forwarder(FakeSink::new(&[], WriteAttempt::Wrote), &clock, DRAIN_TIMEOUT);

        forwarder.forward_packet(&test_bytes(10));
        clock.advance(DRAIN_TIMEOUT);
        forwarder.forward_packet(&test_bytes(10));

        assert_eq!(forwarder.stop_reason, Some(StopReason::DeadlineExpired), "packet at deadline");
        assert_eq!(forwarder.packets_written, 1, "packet at deadline");
        assert_eq!(forwarder.packets_dropped, 1, "packet at deadline");
        assert_eq!(forwarder.sink.attempts, 1, "packet at deadline: no write attempt");
    }

    #[fuchsia::test]
    fn forward_packet_stops_when_tracing_stopped() {
        let clock = FakeClock::new();
        let sink = FakeSink::new(&[], WriteAttempt::TracingStopped);
        let mut forwarder = new_streaming_forwarder(sink, &clock, DRAIN_TIMEOUT);

        forwarder.forward_packet(&test_bytes(10));
        forwarder.forward_packet(&test_bytes(10));

        assert_eq!(forwarder.stop_reason, Some(StopReason::TracingStopped), "TracingStopped");
        assert_eq!(forwarder.packets_dropped, 2, "TracingStopped");
        assert_eq!(forwarder.sink.attempts, 1, "TracingStopped: no attempts after the stop");
    }

    /// Returns a ReadBuffers slice that carries `data`, and ends its packet if `last_slice`.
    fn slice(data: &[u8], last_slice: bool) -> Slice {
        Slice { data: Some(data.to_vec()), last_slice_for_packet: Some(last_slice) }
    }

    /// Runs `forward_slices()` with a `rewrite_pids` that records each packet it is passed and
    /// returns it unchanged.
    fn forward_slices_recording(
        slices: &[Slice],
        packet_data: &mut Vec<u8>,
        forwarder: &mut PacketForwarder<FakeSink, FakeClock>,
        rewritten: &mut Vec<Vec<u8>>,
    ) {
        forward_slices(slices, packet_data, forwarder, |blob_data| {
            rewritten.push(blob_data.clone());
            blob_data
        });
    }

    #[fuchsia::test]
    fn forward_slices_frames_and_rewrites_each_packet() {
        let long_packet = test_bytes(300);
        let clock = FakeClock::new();
        let mut forwarder =
            new_streaming_forwarder(FakeSink::new(&[], WriteAttempt::Wrote), &clock, DRAIN_TIMEOUT);
        let mut packet_data = Vec::new();
        let mut rewritten = Vec::new();
        // Stands in for PID rewriting with a change that shows up in the written blobs.
        let mut invert = |blob_data: Vec<u8>| {
            rewritten.push(blob_data.clone());
            blob_data.iter().map(|byte| !byte).collect::<Vec<u8>>()
        };

        // The 300-byte packet spans two replies.
        forward_slices(
            &[slice(&[1, 2, 3], true), slice(&long_packet[..100], false)],
            &mut packet_data,
            &mut forwarder,
            &mut invert,
        );
        assert_eq!(packet_data, long_packet[..100], "partial packet after the first reply");
        forward_slices(
            &[slice(&long_packet[100..200], false), slice(&long_packet[200..], true)],
            &mut packet_data,
            &mut forwarder,
            &mut invert,
        );
        // Field 1, length delimited, then the length as a varint: 300 is 0xac 0x02.
        let mut framed_long_packet = vec![0x0a, 0xac, 0x02];
        framed_long_packet.extend(&long_packet);
        assert_eq!(rewritten, vec![vec![0x0a, 3, 1, 2, 3], framed_long_packet], "framed packets");
        assert!(packet_data.is_empty(), "packet data left after the last slice");
        assert_eq!(forwarder.packets_written, 2, "2 packets");
        assert_eq!(forwarder.stop_reason, None, "2 packets");
        let inverted: Vec<u8> = rewritten.concat().iter().map(|byte| !byte).collect();
        assert_eq!(forwarder.sink.written.concat(), inverted, "the rewritten packets are written");
    }

    #[fuchsia::test]
    fn forward_slices_skips_packets_after_deadline() {
        let clock = FakeClock::new();
        let mut forwarder =
            new_streaming_forwarder(FakeSink::new(&[], WriteAttempt::Wrote), &clock, DRAIN_TIMEOUT);
        let mut packet_data = Vec::new();
        let mut rewritten = Vec::new();

        // The deadline passes after the first slice of a packet has been read.
        forward_slices_recording(
            &[slice(&test_bytes(100), false)],
            &mut packet_data,
            &mut forwarder,
            &mut rewritten,
        );
        clock.advance(DRAIN_TIMEOUT);
        forward_slices_recording(
            &[slice(&test_bytes(50), true), slice(&test_bytes(10), false)],
            &mut packet_data,
            &mut forwarder,
            &mut rewritten,
        );
        forward_slices_recording(
            &[slice(&test_bytes(20), true)],
            &mut packet_data,
            &mut forwarder,
            &mut rewritten,
        );

        assert_eq!(forwarder.stop_reason, Some(StopReason::DeadlineExpired), "after the deadline");
        assert_eq!(rewritten.len(), 0, "packets framed and rewritten after the deadline");
        assert!(packet_data.is_empty(), "packet data kept after the deadline");
        assert_eq!(forwarder.packets_dropped, 2, "packets that ended after the deadline");
        assert_eq!(forwarder.bytes_dropped, 100 + 50 + 10 + 20, "unframed bytes of 2 packets");
        assert_eq!(forwarder.sink.attempts, 0, "write attempts after the deadline");
    }

    #[fuchsia::test]
    fn forward_slices_skips_rest_of_reply_once_forwarder_stops() {
        let clock = FakeClock::new();
        let mut forwarder =
            new_streaming_forwarder(FakeSink::new(&[], WriteAttempt::Wrote), &clock, DRAIN_TIMEOUT);
        let mut packet_data = Vec::new();
        let mut rewrites = 0;
        let slices = [
            slice(&test_bytes(10), true),
            slice(&test_bytes(20), false),
            slice(&test_bytes(30), true),
            slice(&test_bytes(40), true),
        ];

        // Rewriting the first packet takes the drain past its deadline, so `forward_packet()`
        // drops that packet and stops the forwarder.
        forward_slices(&slices, &mut packet_data, &mut forwarder, |blob_data| {
            rewrites += 1;
            clock.advance(DRAIN_TIMEOUT);
            blob_data
        });

        assert_eq!(forwarder.stop_reason, Some(StopReason::DeadlineExpired), "slow rewrite");
        assert_eq!(rewrites, 1, "only the packet before the deadline is rewritten");
        assert!(packet_data.is_empty(), "packet data kept after the deadline");
        assert_eq!(forwarder.packets_dropped, 3, "all 3 packets");
        // The first packet was framed: 2 bytes of tag and length.
        assert_eq!(forwarder.bytes_dropped, 12 + 50 + 40, "bytes of the dropped packets");
        assert_eq!(forwarder.sink.attempts, 0, "write attempts after the deadline");
    }
}
