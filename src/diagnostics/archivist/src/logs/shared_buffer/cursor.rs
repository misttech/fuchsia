// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::{ContainerId, Inner, SharedBuffer};
use crate::identity::ComponentIdentity;
use diagnostics_data::{LogError, LogsData};
use diagnostics_log_encoding::parse::ParseError;
use diagnostics_log_encoding::{Header, TRACING_FORMAT_LOG_RECORD_TYPE};
use diagnostics_message::error::MessageError;
use fidl_fuchsia_diagnostics::ComponentSelector;
use fuchsia_async::condition::{ConditionGuard, WakerEntry};
use futures::Stream;
use futures::stream::FusedStream;
use pin_project::pin_project;
use selectors::matches_selectors;
use std::cmp::Ordering;
use std::marker::PhantomData;
use std::ops::Range;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll, ready};
use zerocopy::FromBytes;

/// FilterCursor is a cursor that returns all logs optionally filtered by component selectors.
#[pin_project]
pub struct FilterCursor {
    buffer: Arc<SharedBuffer>,

    // Next message ID to read, or `u64::MAX` once the stream has terminated.
    message_id: u64,

    // For `StreamMode::Snapshot`, the message ID (`inner.next_msg_id`) at which the snapshot ends.
    end_msg_id: Option<u64>,

    selectors: Vec<ComponentSelector>,

    // Scratch buffer caching the most recently decompressed `CompressedChunk`.
    decompressed: DecompressedChunk,

    // Saved byte-offset position within the chunk currently being scanned (`decompressed` or
    // `inner.active_chunk`).
    chunk_cursor: ChunkCursor,

    #[pin]
    waker_entry: WakerEntry<Inner>,
}

impl FilterCursor {
    pub fn new(
        buffer: Arc<SharedBuffer>,
        message_id: u64,
        end_msg_id: Option<u64>,
        selectors: Vec<ComponentSelector>,
    ) -> Self {
        let waker_entry = buffer.inner.waker_entry();
        Self {
            buffer,
            message_id,
            end_msg_id,
            selectors,
            decompressed: DecompressedChunk::default(),
            chunk_cursor: ChunkCursor::default(),
            waker_entry,
        }
    }

    /// Polls for the next message.  We can't use the Stream trait because this is a lending
    /// iterator; it returns a reference to a message (and will hold a lock).  Depending on the use,
    /// this can be more efficient than using a Stream which will involve copying the message.
    pub fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Message<'_>>> {
        let this = self.project();
        let mut inner = this.buffer.inner.lock();
        let mut container_ids = ContainerIds::default();

        // If the cursor has fallen behind the oldest retained chunk, fast-forward to the tail and
        // record how many messages were missed.
        //
        // NOTE: If messages are dropped the dropped count won't account for filtering: it will
        // include messages that have been dropped that don't match the filter.  Fixing this is
        // difficult and not worth the effort.  Dropped messages should be rare and the common case
        // is that there is no filtering.
        let mut dropped = 0;
        let compressed_tail = inner.compressed_tail_message_id();
        if *this.message_id < compressed_tail {
            dropped = compressed_tail - *this.message_id;
            *this.message_id = compressed_tail;
            this.decompressed.reset();
        }

        loop {
            if this.end_msg_id.is_some_and(|end_msg_id| *this.message_id >= end_msg_id) {
                *this.message_id = u64::MAX;
                return Poll::Ready(None);
            }

            // Messages before `active_msg_ids.start` live in sealed chunks (`inner.chunks`);
            // messages in `active_msg_ids` live in `inner.active_chunk`.
            let (chunk_bytes, chunk_start, is_active) =
                if *this.message_id < inner.active_msg_ids.start {
                    this.decompressed.load(&inner, *this.message_id);
                    (&this.decompressed.bytes[..], this.decompressed.msg_ids.start, false)
                } else if *this.message_id < inner.active_msg_ids.end {
                    (&inner.active_chunk[..], inner.active_msg_ids.start, true)
                } else {
                    // Caught up to the end of all buffered messages.
                    break;
                };

            let (container_id, msg_range) =
                this.chunk_cursor.read_record(chunk_bytes, chunk_start, *this.message_id);
            *this.message_id += 1;

            if container_ids.matches(&inner, container_id, this.selectors) {
                let data = if is_active {
                    MessageData::ActiveChunk(msg_range)
                } else {
                    MessageData::Slice(&this.decompressed.bytes[msg_range])
                };
                return Poll::Ready(Some(Message { inner, container_id, data, dropped }));
            }
        }

        if inner.terminated {
            *this.message_id = u64::MAX;
            return Poll::Ready(None);
        }

        inner.add_waker(this.waker_entry, cx.waker().clone());
        Poll::Pending
    }

    fn is_terminated(&self) -> bool {
        self.message_id == u64::MAX
    }
}

/// Scratch buffer caching a single decompressed `CompressedChunk`.
#[derive(Default)]
struct DecompressedChunk {
    bytes: Vec<u8>,
    msg_ids: Range<u64>,
}

impl DecompressedChunk {
    fn reset(&mut self) {
        self.msg_ids = 0..0;
    }

    /// Ensures the sealed chunk containing `message_id` is decompressed into `self.bytes`.
    fn load(&mut self, inner: &Inner, message_id: u64) {
        if self.msg_ids.contains(&message_id) {
            return;
        }
        let idx = inner
            .chunks
            .binary_search_by(|c| {
                if message_id < c.msg_ids.start {
                    Ordering::Greater
                } else if message_id >= c.msg_ids.end {
                    Ordering::Less
                } else {
                    Ordering::Equal
                }
            })
            .expect("chunk must exist for message_id < active_msg_ids.start");
        let chunk = &inner.chunks[idx];
        let raw_bytes = inner.chunk_bytes(chunk);
        let uncompressed_len = chunk.uncompressed_len as usize;
        self.bytes.resize(uncompressed_len, 0);
        if raw_bytes.len() < uncompressed_len {
            let written =
                lz4::decompress_into(raw_bytes, &mut self.bytes).expect("lz4 decompression failed");
            assert_eq!(written, uncompressed_len);
        } else {
            self.bytes.copy_from_slice(raw_bytes);
        }
        self.msg_ids = chunk.msg_ids.clone();
    }
}

/// Tracks the byte offset within the chunk currently being scanned to avoid O(N) rescanning from
/// the start of the chunk on each poll.
#[derive(Default)]
struct ChunkCursor {
    // Starting message ID of the chunk that `msg_id` and `offset` refer to.
    chunk_start_msg_id: u64,

    // Message ID corresponding to `offset` within the current chunk.
    msg_id: u64,

    // Byte offset of `msg_id` within the current chunk.
    offset: usize,
}

impl ChunkCursor {
    /// Advances to `target_msg_id` within `chunk` (whose first message has ID
    /// `chunk_start_msg_id`), reads that record, and returns its `(ContainerId, payload_range)`.
    fn read_record(
        &mut self,
        chunk: &[u8],
        chunk_start_msg_id: u64,
        target_msg_id: u64,
    ) -> (ContainerId, Range<usize>) {
        if self.chunk_start_msg_id != chunk_start_msg_id || self.msg_id > target_msg_id {
            self.chunk_start_msg_id = chunk_start_msg_id;
            self.msg_id = chunk_start_msg_id;
            self.offset = 0;
        }
        while self.msg_id < target_msg_id {
            let (_, msg_range) = Self::parse_record_at(chunk, self.offset);
            self.offset = msg_range.end;
            self.msg_id += 1;
        }
        let (container_id, msg_range) = Self::parse_record_at(chunk, self.offset);
        self.offset = msg_range.end;
        self.msg_id += 1;
        (container_id, msg_range)
    }

    fn parse_record_at(chunk: &[u8], offset: usize) -> (ContainerId, Range<usize>) {
        let container_id =
            ContainerId(u32::from_ne_bytes(chunk[offset..offset + 4].try_into().unwrap()));
        let msg_len =
            u32::from_ne_bytes(chunk[offset + 4..offset + 8].try_into().unwrap()) as usize;
        (container_id, (offset + 8)..(offset + 8 + msg_len))
    }
}

/// Small ring-buffer cache memoizing selector match results by `ContainerId`, as comparing
/// monikers against selectors is relatively expensive.
struct ContainerIds {
    ids: [ContainerId; 8],
    result: u8,
    pos: usize,
}

impl Default for ContainerIds {
    fn default() -> Self {
        Self { ids: [ContainerId(0xffff); 8], result: 0, pos: 0 }
    }
}

impl ContainerIds {
    fn matches(&mut self, inner: &Inner, id: ContainerId, selectors: &[ComponentSelector]) -> bool {
        let Some(container) = inner.containers.get(id) else {
            return false;
        };
        if selectors.is_empty() {
            return true;
        }
        if let Some(pos) = self.ids.iter().position(|i| *i == id) {
            self.result & (1 << pos) != 0
        } else {
            let matched = matches_selectors(&container.identity.moniker, selectors);
            self.ids[self.pos] = id;
            if matched {
                self.result |= 1 << self.pos;
            } else {
                self.result &= !(1 << self.pos);
            }
            self.pos = (self.pos + 1) % self.ids.len();
            matched
        }
    }
}

enum MessageData<'a> {
    Slice(&'a [u8]),
    ActiveChunk(Range<usize>),
}

pub struct Message<'a> {
    inner: ConditionGuard<'a, Inner>,
    container_id: ContainerId,
    data: MessageData<'a>,
    pub dropped: u64,
}

impl Message<'_> {
    /// Returns the component and FXT bytes.  The FXT record is validated to be the correct length
    /// and type.
    pub fn parse(&self) -> Result<(&Arc<ComponentIdentity>, Header, &[u8]), MessageError> {
        let data = match &self.data {
            MessageData::Slice(slice) => *slice,
            MessageData::ActiveChunk(range) => &self.inner.active_chunk[range.clone()],
        };
        let (mut header, _) = Header::read_from_prefix(data)
            .map_err(|_| MessageError::from(ParseError::InvalidHeader))?;
        let msg_len = header.size_words() as usize * 8;
        if msg_len > data.len() || msg_len < 16 {
            return Err(ParseError::ValueOutOfValidRange.into());
        }
        if header.raw_type() != TRACING_FORMAT_LOG_RECORD_TYPE {
            return Err(ParseError::InvalidRecordType.into());
        }
        header.set_tag(self.container_id.0);
        let container = self.inner.containers.get(self.container_id).unwrap();
        Ok((&container.identity, header, &data[..msg_len]))
    }
}

impl TryFrom<Message<'_>> for LogsData {
    type Error = MessageError;

    fn try_from(value: Message<'_>) -> Result<Self, Self::Error> {
        let (container, _header, data) = value.parse()?;
        let mut data = diagnostics_message::from_structured(container.as_ref().into(), data)?;
        if value.dropped > 0 {
            data.metadata
                .errors
                .get_or_insert(vec![])
                .push(LogError::RolledOutLogs { count: value.dropped });
        }
        Ok(data)
    }
}

/// Like FilterCursor but returns a stream of LogsData.
#[pin_project]
pub struct FilterCursorStream<T> {
    #[pin]
    cursor: FilterCursor,
    phantom: PhantomData<T>,
}

impl<T> Stream for FilterCursorStream<T>
where
    T: for<'a> TryFrom<Message<'a>>,
{
    type Item = T;

    fn poll_next(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        let mut this = self.project();
        loop {
            match ready!(this.cursor.as_mut().poll_next(cx)) {
                Some(item) => {
                    if let Ok(data) = T::try_from(item) {
                        return Poll::Ready(Some(data));
                    }
                    // The message is bad, just ignore it.
                }
                None => return Poll::Ready(None),
            }
        }
    }
}

impl<T> FusedStream for FilterCursorStream<T>
where
    T: for<'a> TryFrom<Message<'a>>,
{
    fn is_terminated(&self) -> bool {
        self.cursor.is_terminated()
    }
}

impl<T> From<FilterCursor> for FilterCursorStream<T> {
    fn from(cursor: FilterCursor) -> Self {
        Self { cursor, phantom: PhantomData }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::logs::shared_buffer::{
        InnerGuard, SharedBuffer, SharedBufferOptions, create_ring_buffer,
    };
    use crate::logs::stats::LogStreamStats;
    use crate::logs::testing::make_message;
    use assert_matches::assert_matches;
    use fidl_fuchsia_diagnostics::StreamMode;
    use fuchsia_inspect::Node;
    use futures::{StreamExt, poll};
    use selectors::{FastError, parse_component_selector};
    use std::future::poll_fn;
    use std::pin::pin;
    use std::time::Duration;

    fn test_stats() -> Arc<LogStreamStats> {
        Arc::new(LogStreamStats::new(&Node::default(), &ComponentIdentity::unknown()))
    }

    #[fuchsia::test]
    async fn cursor_basic() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );
        let container = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));
        container.push_back(msg.bytes());

        let cursor = buffer.cursor(StreamMode::Snapshot, vec![]);
        let mut stream = pin!(FilterCursorStream::<LogsData>::from(cursor));

        let item = stream.next().await.unwrap();
        assert_eq!(item.msg().unwrap(), "a");
        assert!(stream.next().await.is_none());
    }

    #[fuchsia::test]
    async fn cursor_filter() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );
        let container_a = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        let container_b = buffer.new_container_buffer(Arc::new(vec!["b"].into()), test_stats());

        container_a.push_back(make_message("msg_a", None, zx::BootInstant::from_nanos(1)).bytes());
        container_b.push_back(make_message("msg_b", None, zx::BootInstant::from_nanos(2)).bytes());

        let selector = parse_component_selector::<FastError>("a").unwrap();
        let cursor = buffer.cursor(StreamMode::Snapshot, vec![selector]);
        let mut stream = pin!(FilterCursorStream::<LogsData>::from(cursor));

        let item = stream.next().await.unwrap();
        assert_eq!(item.msg().unwrap(), "msg_a");
        assert!(stream.next().await.is_none());
    }

    #[fuchsia::test]
    async fn cursor_subscribe() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );
        let container = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());

        let cursor = buffer.cursor(StreamMode::Subscribe, vec![]);
        let mut stream = pin!(FilterCursorStream::<LogsData>::from(cursor));

        assert!(poll!(stream.next()).is_pending());

        container.push_back(make_message("msg", None, zx::BootInstant::from_nanos(1)).bytes());

        let item = stream.next().await.unwrap();
        assert_eq!(item.msg().unwrap(), "msg");
    }

    #[fuchsia::test]
    async fn cursor_snapshot_then_subscribe() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );
        let container = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());

        container.push_back(make_message("msg1", None, zx::BootInstant::from_nanos(1)).bytes());

        let cursor = buffer.cursor(StreamMode::SnapshotThenSubscribe, vec![]);
        let mut stream = pin!(FilterCursorStream::<LogsData>::from(cursor));

        let item = stream.next().await.unwrap();
        assert_eq!(item.msg().unwrap(), "msg1");

        assert!(poll!(stream.next()).is_pending());

        container.push_back(make_message("msg2", None, zx::BootInstant::from_nanos(2)).bytes());

        let item = stream.next().await.unwrap();
        assert_eq!(item.msg().unwrap(), "msg2");
    }

    #[fuchsia::test]
    async fn cursor_dropped_logs() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            SharedBufferOptions { sleep_time: Duration::ZERO, ..Default::default() },
            &Node::default(),
        );
        let container_a = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        let container_b = buffer.new_container_buffer(Arc::new(vec!["b"].into()), test_stats());

        let msg = make_message("msg", None, zx::BootInstant::from_nanos(1));

        container_a.push_back(msg.bytes()); // 1
        container_a.push_back(msg.bytes()); // 2
        container_a.push_back(msg.bytes()); // 3

        let cursor = buffer.cursor(StreamMode::SnapshotThenSubscribe, vec![]);
        let mut stream = pin!(FilterCursorStream::<LogsData>::from(cursor));

        // Consume one
        let item = stream.next().await.unwrap();
        assert_eq!(item.msg().unwrap(), "msg");

        let has_messages = async |component| {
            let mut cursor = pin!(buffer.cursor(
                StreamMode::Snapshot,
                vec![parse_component_selector::<FastError>(component).unwrap()],
            ));
            poll_fn(|cx| cursor.as_mut().poll_next(cx).map(|i| i.is_some())).await
        };

        // Force roll out of remaining A messages
        while has_messages("a").await {
            container_b.push_back(msg.bytes());
            // Force the buffer to pop old messages.
            {
                let mut inner = InnerGuard::new(&buffer);
                inner.drain_ring_buffer(inner.ring_buffer.head());
            }
        }

        // Next item should indicate the dropped messages from the rolled-out chunk (which
        // includes the two unread messages from container A plus the container B messages in
        // that chunk).
        let item = stream.next().await.unwrap();
        assert_eq!(item.metadata.errors.as_ref().unwrap().len(), 1);
        assert_matches!(
            item.metadata.errors.as_ref().unwrap()[0],
            LogError::RolledOutLogs { count } if count >= 2
        );
    }

    #[fuchsia::test]
    async fn terminate_terminates_cursor() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            SharedBufferOptions { sleep_time: Duration::from_secs(10), ..Default::default() },
            &Node::default(),
        );
        let cursor = pin!(FilterCursorStream::<LogsData>::from(
            buffer.cursor(StreamMode::SnapshotThenSubscribe, vec![])
        ));
        let container = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        container.push_back(make_message("msg", None, zx::BootInstant::from_nanos(1)).bytes());
        buffer.terminate();
        assert_eq!(cursor.count().await, 1);
    }

    #[fuchsia::test]
    fn chunk_cursor_read_and_skip() {
        let mut chunk = Vec::new();
        let records = [
            (ContainerId(1), b"first".as_slice()),
            (ContainerId(2), b"second_record".as_slice()),
            (ContainerId(3), b"third".as_slice()),
            (ContainerId(4), b"fourth_record_payload".as_slice()),
        ];
        for (id, payload) in records {
            chunk.extend_from_slice(&id.0.to_ne_bytes());
            chunk.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
            chunk.extend_from_slice(payload);
        }

        let mut cursor = ChunkCursor::default();

        // Read first record at the start of chunk (msg_id 10).
        let (id, range) = cursor.read_record(&chunk, 10, 10);
        assert_eq!(id, ContainerId(1));
        assert_eq!(&chunk[range], b"first");

        // Skip msg_id 11 ("second_record") and read msg_id 12 ("third").
        let (id, range) = cursor.read_record(&chunk, 10, 12);
        assert_eq!(id, ContainerId(3));
        assert_eq!(&chunk[range], b"third");

        // Read the next sequential record (msg_id 13) without rescanning.
        let (id, range) = cursor.read_record(&chunk, 10, 13);
        assert_eq!(id, ContainerId(4));
        assert_eq!(&chunk[range], b"fourth_record_payload");

        // Seeking backward within the same chunk resets to offset 0 and scans forward.
        let (id, range) = cursor.read_record(&chunk, 10, 11);
        assert_eq!(id, ContainerId(2));
        assert_eq!(&chunk[range], b"second_record");

        // Switching to a new chunk (`chunk_start_msg_id` changes) resets offset to 0.
        let (id, range) = cursor.read_record(&chunk, 20, 20);
        assert_eq!(id, ContainerId(1));
        assert_eq!(&chunk[range], b"first");
    }

    // `SharedBuffer::new` spawns a task, which requires an active executor (provided by
    // `#[fuchsia::test]` when the test function is `async`).
    #[fuchsia::test]
    #[allow(clippy::unused_async)]
    async fn container_ids_memoization_and_wraparound() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );

        // Create 12 containers (> 8 cache slots): even indices match `match_*`, odd indices don't.
        let mut containers = Vec::new();
        for i in 0..12 {
            let name = if i % 2 == 0 { format!("match_{i}") } else { format!("other_{i}") };
            containers.push(
                buffer.new_container_buffer(Arc::new(vec![name.as_str()].into()), test_stats()),
            );
        }

        let selectors = vec![parse_component_selector::<FastError>("match_*").unwrap()];
        {
            let inner = buffer.inner.lock();
            let mut cache = ContainerIds::default();

            // Non-existent ContainerId returns false.
            assert!(!cache.matches(&inner, ContainerId(9999), &selectors));

            // Empty selectors always match an existing container.
            assert!(cache.matches(&inner, containers[1].container_id, &[]));

            // Populate all 12 containers to wrap around the 8-entry ring buffer and overwrite bits.
            for (i, c) in containers.iter().enumerate() {
                assert_eq!(cache.matches(&inner, c.container_id, &selectors), i % 2 == 0);
            }

            // Re-query the last 8 (cached hits) and first 4 (evicted misses) to verify consistency.
            for (i, c) in containers.iter().enumerate().rev() {
                assert_eq!(cache.matches(&inner, c.container_id, &selectors), i % 2 == 0);
            }
        }
        buffer.terminate();
    }

    // `SharedBuffer::new` spawns a task, which requires an active executor (provided by
    // `#[fuchsia::test]` when the test function is `async`).
    #[fuchsia::test]
    #[allow(clippy::unused_async)]
    async fn decompressed_chunk_compressed_and_uncompressed() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );
        let container = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());

        // Seal a compressible chunk (msg_id 0..2).
        let msg = make_message(&"a".repeat(200), None, zx::BootInstant::from_nanos(1));
        container.push_back(msg.bytes());
        container.push_back(msg.bytes());
        buffer.inner.lock().seal_active_chunk();

        // Seal an incompressible chunk (msg_id 2..3) by seeding `active_chunk` with high-entropy
        // bytes via a short message that cannot compress smaller than its original length.
        let short_msg = make_message("x", None, zx::BootInstant::from_nanos(2));
        {
            let mut inner = buffer.inner.lock();
            inner.append_message(container.container_id, short_msg.bytes());

            // Overwrite the message payload with pseudo-random bytes so LZ4 cannot compress it.
            let mut state = 0x1234_5678_9abc_def0u64;
            for b in &mut inner.active_chunk[8..] {
                state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
                *b = (state >> 32) as u8;
            }
            inner.seal_active_chunk();
        }

        {
            let inner = buffer.inner.lock();
            assert_eq!(inner.chunks.len(), 2);

            // First chunk is compressed (`stored_len < uncompressed_len`).
            assert!(
                (inner.chunks[0].range.end - inner.chunks[0].range.start)
                    < inner.chunks[0].uncompressed_len as u64
            );

            // Second chunk is stored uncompressed (`stored_len == uncompressed_len`).
            assert_eq!(
                inner.chunks[1].range.end - inner.chunks[1].range.start,
                inner.chunks[1].uncompressed_len as u64
            );

            let mut decompressed = DecompressedChunk::default();
            decompressed.load(&inner, 0);
            assert_eq!(decompressed.msg_ids, 0..2);
            assert_eq!(decompressed.bytes.len(), inner.chunks[0].uncompressed_len as usize);

            // Loading another message ID in the same chunk is a cached no-op.
            decompressed.load(&inner, 1);
            assert_eq!(decompressed.msg_ids, 0..2);

            // Loading msg_id 2 switches to the uncompressed chunk.
            decompressed.load(&inner, 2);
            assert_eq!(decompressed.msg_ids, 2..3);
            assert_eq!(decompressed.bytes.len(), inner.chunks[1].uncompressed_len as usize);

            decompressed.reset();
            assert_eq!(decompressed.msg_ids, 0..0);
        }
        buffer.terminate();
    }

    #[fuchsia::test]
    async fn cursor_reads_across_active_to_sealed_chunk_transition() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );
        let container = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());

        // Push 2 messages into `active_chunk` before creating a Subscribe cursor.
        container.push_back(make_message("m0", None, zx::BootInstant::from_nanos(1)).bytes());
        container.push_back(make_message("m1", None, zx::BootInstant::from_nanos(2)).bytes());

        // Subscribe cursor starts mid-chunk at message_id 2.
        let mut stream = pin!(FilterCursorStream::<LogsData>::from(
            buffer.cursor(StreamMode::Subscribe, vec![])
        ));
        assert!(poll!(stream.next()).is_pending());

        // Append m2 and m3 to the same `active_chunk` and read only m2 while it's still active.
        container.push_back(make_message("m2", None, zx::BootInstant::from_nanos(3)).bytes());
        container.push_back(make_message("m3", None, zx::BootInstant::from_nanos(4)).bytes());
        assert_eq!(stream.next().await.unwrap().msg().unwrap(), "m2");

        // Seal `active_chunk` into a `CompressedChunk` before reading m3, then push m4 into the
        // new `active_chunk`.
        buffer.inner.lock().seal_active_chunk();
        container.push_back(make_message("m4", None, zx::BootInstant::from_nanos(5)).bytes());

        assert_eq!(stream.next().await.unwrap().msg().unwrap(), "m3");
        assert_eq!(stream.next().await.unwrap().msg().unwrap(), "m4");
        assert!(poll!(stream.next()).is_pending());
    }
}
