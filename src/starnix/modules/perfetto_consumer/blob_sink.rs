// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fxt::blob::{BlobType, build_blob_record};
use starnix_logging::NAME_PERFETTO_BLOB;

/// The result of a single attempt to write one blob record to the Fuchsia trace buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum WriteAttempt {
    /// The record was copied into the trace buffer.
    Wrote,
    /// The trace buffer had no room for the record.
    BufferFull,
    /// No trace context could be acquired.
    TracingStopped,
    /// `blob_data` does not fit in one record.
    TooLong,
}

/// Writes blob records for a drain. Tests replace it with a fake.
pub(crate) trait BlobSink {
    /// Makes one attempt to write `blob_data` as a single Perfetto blob record, without
    /// waiting.
    fn try_write_blob(&mut self, blob_data: &[u8]) -> WriteAttempt;
}

/// Writes blob records to the Fuchsia trace buffer of the current trace session.
pub(crate) struct TraceBufferSink;

impl BlobSink for TraceBufferSink {
    fn try_write_blob(&mut self, blob_data: &[u8]) -> WriteAttempt {
        let Some(record_words) =
            build_blob_record(NAME_PERFETTO_BLOB, BlobType::Perfetto, blob_data)
        else {
            return WriteAttempt::TooLong;
        };
        let Some(context) = fuchsia_trace::Context::acquire() else {
            return WriteAttempt::TracingStopped;
        };
        if context.copy_record(&record_words).is_some() {
            return WriteAttempt::Wrote;
        }
        // Release the context before the caller waits. If this was the last writer holding a
        // context, the release services any pending switch of rolling buffers: in streaming
        // mode it asks trace_manager to save the full rolling buffer, and in circular mode it
        // frees that buffer for reuse. Prolonged contexts don't count as writers, so the
        // drain's prolonged context does not block this.
        drop(context);
        WriteAttempt::BufferFull
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet_forwarder::MAX_BLOB_PAYLOAD_SIZE;
    use crate::testing::{TRACE_BUFFER_SIZE, TraceSession, lock_trace_engine, test_bytes};

    #[fuchsia::test]
    fn trace_buffer_sink_reports_each_write_attempt() {
        let mut sink = TraceBufferSink;
        // Too long for one record even without the header and name.
        let too_long = test_bytes(fxt::MAX_RECORD_SIZE_WORDS * 8);
        {
            let _lock = lock_trace_engine();
            assert!(fuchsia_trace::Context::acquire().is_none(), "a trace session is running");
            // The length is checked before a context is acquired.
            assert_eq!(sink.try_write_blob(&too_long), WriteAttempt::TooLong, "too long blob");
            assert_eq!(
                sink.try_write_blob(&test_bytes(10)),
                WriteAttempt::TracingStopped,
                "no trace session"
            );
        }

        // `TraceSession::start()` takes `TRACE_ENGINE_LOCK` itself, so it is released above.
        let _session = TraceSession::start();
        assert_eq!(sink.try_write_blob(&test_bytes(10)), WriteAttempt::Wrote, "oneshot session");
        // A oneshot buffer only fills up, so it is full after at most this many full blobs.
        let max_attempts = TRACE_BUFFER_SIZE / MAX_BLOB_PAYLOAD_SIZE + 1;
        let full_blob = test_bytes(MAX_BLOB_PAYLOAD_SIZE);
        assert!(
            (0..max_attempts).any(|_| sink.try_write_blob(&full_blob) == WriteAttempt::BufferFull),
            "{TRACE_BUFFER_SIZE}-byte oneshot buffer still had room after {max_attempts} full blobs"
        );
        assert_eq!(sink.try_write_blob(&full_blob), WriteAttempt::BufferFull, "full buffer");
    }
}
