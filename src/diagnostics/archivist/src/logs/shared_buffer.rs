// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod cursor;

pub use cursor::{FilterCursor, FilterCursorStream};

use crate::identity::ComponentIdentity;
use crate::logs::repository::ARCHIVIST_MONIKER;
use crate::logs::stats::{LogStreamStats, SaturationCurve};
use derivative::Derivative;
use diagnostics_log_encoding::{Header, TRACING_FORMAT_LOG_RECORD_TYPE};
use fidl_fuchsia_diagnostics::{ComponentSelector, StreamMode};
use fuchsia_async as fasync;
use fuchsia_async::condition::{Condition, ConditionGuard};
use fuchsia_inspect::Node;
use log::debug;
use ring_buffer::{MAX_MESSAGE_SIZE, Reader, RingBuffer, ring_buffer_record_len};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::ops::{Deref, DerefMut, Range};
use std::sync::{Arc, Weak};
use std::time::Duration;
use zerocopy::FromBytes;

// Aim to keep 25% of the ring buffer free. This is expressed as a fraction: numerator / denominator.
const SPACE_THRESHOLD_NUMERATOR: usize = 1;
const SPACE_THRESHOLD_DENOMINATOR: usize = 4;

// The default amount of time that Archivist will sleep for to reduce how often it wakes up to
// handle log messages.
const DEFAULT_SLEEP_TIME: Duration = Duration::from_millis(200);

// Target uncompressed size of each LZ4-compressed chunk.
const CHUNK_SIZE: usize = 65536;

pub struct Buffer {
    // The underlying IOBuffer ring buffer reader used as the ingestion staging area.
    reader: Reader,

    // Target budget in bytes for the compressed log buffer (`logs_max_cached_original_bytes`).
    capacity: usize,
}

impl Deref for Buffer {
    type Target = Reader;

    fn deref(&self) -> &Self::Target {
        &self.reader
    }
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.reader
    }
}

pub fn create_ring_buffer(capacity: usize) -> Buffer {
    let page_size = zx::system_get_page_size() as usize;
    let iob_capacity = (capacity * SPACE_THRESHOLD_NUMERATOR / SPACE_THRESHOLD_DENOMINATOR)
        .max(MAX_MESSAGE_SIZE)
        .next_multiple_of(page_size);
    Buffer {
        reader: RingBuffer::create(iob_capacity),
        capacity: capacity.max(CHUNK_SIZE).max(MAX_MESSAGE_SIZE),
    }
}

pub type OnInactive = Box<dyn Fn(Arc<ComponentIdentity>) + Send + Sync>;

struct CompressedChunk {
    // Monotonic byte range `start..end` in `Inner::data` (guaranteed not to cross a multiple of
    // `Inner::data.len()`).
    range: Range<u64>,

    // Uncompressed byte length of this chunk. If equal to `range.end - range.start`, the chunk
    // was not compressible and is stored uncompressed in `Inner::data`.
    uncompressed_len: u32,

    // Contiguous range of message IDs stored in this chunk.
    msg_ids: Range<u64>,
}

pub struct SharedBuffer {
    inner: Condition<Inner>,

    // Callback for when a container is inactive (i.e. no logs and no IOBuffers).
    on_inactive: OnInactive,

    // The task responsible for draining the IOBuffer ring buffer into the compressed buffer and
    // waking cursors whenever new data arrives.
    _buffer_monitor_task: fasync::Task<()>,

    /// Metric for tracking buffer saturation.
    pub(crate) saturation_curve: Arc<SaturationCurve>,
}

struct InnerGuard<'a> {
    buffer: &'a Arc<SharedBuffer>,

    guard: ManuallyDrop<ConditionGuard<'a, Inner>>,

    // The list of components that should be reported as inactive once the lock is dropped.
    on_inactive: Vec<Arc<ComponentIdentity>>,

    // If true, wake all the wakers registered with the ConditionGuard.
    wake: bool,
}

impl Drop for InnerGuard<'_> {
    fn drop(&mut self) {
        if self.wake {
            for waker in self.guard.drain_wakers() {
                waker.wake();
            }
        }

        // SAFETY: This is the only place we drop the guard.
        unsafe {
            ManuallyDrop::drop(&mut self.guard);
        }
        for identity in self.on_inactive.drain(..) {
            (*self.buffer.on_inactive)(identity);
        }
    }
}

impl Deref for InnerGuard<'_> {
    type Target = Inner;

    fn deref(&self) -> &Self::Target {
        &self.guard
    }
}

impl DerefMut for InnerGuard<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.guard
    }
}

struct Inner {
    // The ring buffer.
    ring_buffer: Arc<RingBuffer>,

    // Registered containers.
    containers: Containers,

    // A copy of the tail index in the ring buffer.
    tail: u64,

    // Single pre-allocated circular buffer holding all sealed compressed chunks.
    data: Box<[u8]>,

    // Monotonic byte offset in `data` where the next sealed compressed chunk will be written.
    compressed_head: u64,

    // Metadata for sealed chunks in `data` in chronological order.
    chunks: VecDeque<CompressedChunk>,

    // Pre-allocated 64 KiB (`CHUNK_SIZE`) staging buffer for the uncompressed chunk currently being
    // built.
    active_chunk: Vec<u8>,

    // Contiguous range of message IDs currently stored in `active_chunk`.
    active_msg_ids: Range<u64>,

    // Monotonically increasing message ID assigned to the next record extracted into `active_chunk`.
    next_msg_id: u64,

    // The IOBuffer peers that we must watch.
    iob_peers: Slab<fasync::ReceiverRegistration<IobPacketReceiver>>,

    // True, when the buffer has been terminated.
    terminated: bool,
}

pub struct SharedBufferOptions {
    // To reduce how often Archivist wakes when new messages are written, Archivist will sleep for
    // this time. This will impact how quickly messages show up via the cursors.
    pub sleep_time: Duration,

    /// The size of the circular buffer for the saturation curve.
    pub saturation_curve_size: usize,
}

impl Default for SharedBufferOptions {
    fn default() -> Self {
        Self { sleep_time: DEFAULT_SLEEP_TIME, saturation_curve_size: 254 }
    }
}

impl SharedBuffer {
    /// Returns a new shared buffer.
    pub fn new(
        ring_buffer: Buffer,
        on_inactive: OnInactive,
        options: SharedBufferOptions,
        inspect_node: &Node,
    ) -> Arc<Self> {
        let Buffer { reader: ring_buffer, capacity } = ring_buffer;
        let saturation_curve =
            Arc::new(SaturationCurve::new(inspect_node, options.saturation_curve_size));
        Arc::new_cyclic(|weak: &Weak<Self>| Self {
            inner: Condition::new(Inner {
                ring_buffer: Arc::clone(&ring_buffer),
                containers: Containers::default(),
                tail: 0,
                data: vec![0; capacity].into_boxed_slice(),
                compressed_head: 0,
                chunks: VecDeque::new(),
                active_chunk: Vec::with_capacity(CHUNK_SIZE),
                active_msg_ids: 0..0,
                next_msg_id: 0,
                iob_peers: Slab::default(),
                terminated: false,
            }),
            on_inactive,
            saturation_curve,
            _buffer_monitor_task: fasync::Task::spawn(Self::buffer_monitor_task(
                Weak::clone(weak),
                ring_buffer,
                options.sleep_time,
            )),
        })
    }

    pub fn new_container_buffer(
        self: &Arc<Self>,
        identity: Arc<ComponentIdentity>,
        stats: Arc<LogStreamStats>,
    ) -> ContainerBuffer {
        ContainerBuffer {
            shared_buffer: Arc::clone(self),
            container_id: self.inner.lock().containers.new_container(identity, stats),
        }
    }

    pub fn flush(self: &Arc<Self>) {
        let mut inner = InnerGuard::new(self);
        let head = inner.ring_buffer.head();
        inner.drain_ring_buffer(head);
    }

    /// Returns the number of registered containers in use by the buffer.
    #[cfg(test)]
    pub fn container_count(&self) -> usize {
        self.inner.lock().containers.len()
    }

    /// Terminates the shared buffer, draining any remaining messages from the ring buffer and
    /// waking all cursors.
    pub fn terminate(self: &Arc<Self>) {
        let mut inner = InnerGuard::new(self);
        let head = inner.ring_buffer.head();
        inner.drain_ring_buffer(head);
        inner.terminated = true;
        inner.wake = true;
    }

    /// Returns a cursor that will return messages that match `selectors`. If no selectors are
    /// specified, *all* messages will be returned.
    pub fn cursor(
        self: &Arc<Self>,
        mode: StreamMode,
        selectors: Vec<ComponentSelector>,
    ) -> FilterCursor {
        InnerGuard::new(self).cursor(mode, selectors)
    }

    async fn buffer_monitor_task(this: Weak<Self>, mut ring_buffer: Reader, sleep_time: Duration) {
        let mut last_head = 0;
        loop {
            // Sleep to limit how often we wake up.
            fasync::Timer::new(sleep_time).await;
            let head = ring_buffer.wait(last_head).await;
            let Some(this) = this.upgrade() else { return };
            let mut inner = InnerGuard::new(&this);
            inner.drain_ring_buffer(head);
            last_head = inner.tail;
        }
    }
}

impl Inner {
    fn compressed_tail_message_id(&self) -> u64 {
        self.chunks.front().map(|c| c.msg_ids.start).unwrap_or(self.active_msg_ids.start)
    }

    fn chunk_bytes(&self, chunk: &CompressedChunk) -> &[u8] {
        let phys_start = (chunk.range.start % self.data.len() as u64) as usize;
        let len = (chunk.range.end - chunk.range.start) as usize;
        &self.data[phys_start..phys_start + len]
    }

    /// Compresses `self.active_chunk` into the circular `self.data` buffer and records a new
    /// `CompressedChunk` in `self.chunks`.
    fn seal_active_chunk(&mut self) {
        if self.active_chunk.is_empty() {
            return;
        }
        let cap = self.data.len();
        let uncompressed_len = self.active_chunk.len();
        let phys_start = (self.compressed_head % cap as u64) as usize;
        // We only want to store the compressed output if it is strictly smaller than
        // `uncompressed_len` (since `stored_len == uncompressed_len` indicates an uncompressed
        // chunk) and fits before the physical end of `self.data`.
        let max_dst = (cap - phys_start).min(uncompressed_len - 1);

        // Evict any oldest chunks that would be overwritten by writing up to `uncompressed_len`
        // bytes at `self.compressed_head`.
        while self.chunks.front().is_some_and(|oldest| {
            self.compressed_head + uncompressed_len as u64 - oldest.range.start > cap as u64
        }) {
            self.chunks.pop_front();
        }

        let stored_len = if let Ok(compressed_len) =
            lz4::compress_into(&self.active_chunk, &mut self.data[phys_start..phys_start + max_dst])
        {
            // Fast path: the compressed chunk fit in the remaining tail of `self.data` and was
            // strictly smaller than `uncompressed_len`.
            compressed_len
        } else if max_dst < uncompressed_len - 1 {
            // We are near the physical end of `self.data` (`cap - phys_start < uncompressed_len`)
            // and the compressed output did not fit in the remaining tail space. Since a chunk
            // cannot straddle the end of `self.data`, advance `self.compressed_head` to wrap to
            // physical index 0.
            self.compressed_head += (cap - phys_start) as u64;
            // Evict any oldest chunks that would be overwritten by writing up to `uncompressed_len`
            // bytes at physical index 0.
            while self.chunks.front().is_some_and(|oldest| {
                self.compressed_head + uncompressed_len as u64 - oldest.range.start > cap as u64
            }) {
                self.chunks.pop_front();
            }
            // Retry compressing at the start of `self.data`. If compression still fails, the data
            // is incompressible, so store it uncompressed at `0..uncompressed_len`.
            if let Ok(compressed_len) =
                lz4::compress_into(&self.active_chunk, &mut self.data[0..uncompressed_len - 1])
            {
                compressed_len
            } else {
                self.data[0..uncompressed_len].copy_from_slice(&self.active_chunk);
                uncompressed_len
            }
        } else {
            // `max_dst == uncompressed_len - 1`, meaning there was at least `uncompressed_len`
            // bytes of contiguous space at `phys_start`, so compression failed only because the
            // chunk did not compress to fewer than `uncompressed_len` bytes. Store the raw bytes
            // uncompressed; cursors detect this via
            // `chunk.range.end - chunk.range.start == chunk.uncompressed_len`.
            self.data[phys_start..phys_start + uncompressed_len]
                .copy_from_slice(&self.active_chunk);
            uncompressed_len
        };

        let range = self.compressed_head..self.compressed_head + stored_len as u64;
        self.chunks.push_back(CompressedChunk {
            range: range.clone(),
            uncompressed_len: uncompressed_len as u32,
            msg_ids: self.active_msg_ids.clone(),
        });
        self.compressed_head = range.end;
        self.active_chunk.clear();
        self.active_msg_ids = self.next_msg_id..self.next_msg_id;
    }

    fn append_message(&mut self, container_id: ContainerId, message: &[u8]) {
        let msg_len = message.len();
        let record_bytes = 8 + msg_len;

        // If appending this record would cause `active_chunk` to exceed `CHUNK_SIZE` (64 KiB),
        // compress and seal the current `active_chunk` into `self.data` first.
        if !self.active_chunk.is_empty() && self.active_chunk.len() + record_bytes > CHUNK_SIZE {
            self.seal_active_chunk();
        }

        let severity = (msg_len >= 8)
            .then(|| Header::read_from_bytes(&message[0..8]).unwrap().severity().into());
        self.active_chunk.extend_from_slice(&container_id.0.to_ne_bytes());
        self.active_chunk.extend_from_slice(&(msg_len as u32).to_ne_bytes());
        self.active_chunk.extend_from_slice(message);

        let msg_id = self.next_msg_id;
        self.next_msg_id += 1;
        self.active_msg_ids.end = self.next_msg_id;

        let container = self.containers.get_mut(container_id).unwrap();
        container.last_msg_id = Some(msg_id);
        if let Some(severity) = severity {
            container.stats.ingest_message(msg_len, severity);
        }
    }
}

impl<'a> InnerGuard<'a> {
    fn new(buffer: &'a Arc<SharedBuffer>) -> Self {
        Self {
            buffer,
            guard: ManuallyDrop::new(buffer.inner.lock()),
            on_inactive: Vec::new(),
            wake: false,
        }
    }

    fn finish_ingest(&mut self, old_compressed_tail: u64) {
        // If any old chunks were evicted while sealing chunks above, check whether any containers
        // had their last message evicted and are now inactive.
        let new_compressed_tail = self.compressed_tail_message_id();
        if new_compressed_tail > old_compressed_tail {
            let mut id = 0;
            while let Some(used_id) = self.containers.slab.next_used(id) {
                id = used_id + 1;
                let container = self.containers.slab.get_mut(used_id).unwrap();
                if container.last_msg_id.is_some_and(|msg_id| msg_id < new_compressed_tail) {
                    container.last_msg_id = None;
                    if !container.is_active() {
                        if container.should_free() {
                            self.containers.slab.free(used_id);
                        } else {
                            let identity = Arc::clone(&container.identity);
                            self.on_inactive.push(identity);
                        }
                    }
                }
            }
        }

        self.wake = true;
        let boot_time = zx::BootInstant::get().into_nanos();
        let count = self.next_msg_id - new_compressed_tail;
        self.buffer.saturation_curve.record(boot_time, count);
    }

    fn ingest(&mut self, msg: &[u8], container_id: ContainerId) {
        if msg.len() < std::mem::size_of::<Header>() {
            debug!("message too short ({})", msg.len());
            if let Some(container) = self.containers.get(container_id) {
                container.stats.increment_invalid(msg.len());
            }
            return;
        }

        let header = Header::read_from_bytes(&msg[..std::mem::size_of::<Header>()]).unwrap();

        // NOTE: Some tests send messages that are bigger than the header indicates. We ignore the
        // tail of any such messages.
        let msg_len = header.size_words() as usize * 8;

        // Check the correct type and size.
        if header.raw_type() != TRACING_FORMAT_LOG_RECORD_TYPE || msg.len() < msg_len {
            debug!("bad type or size ({}, {}, {})", header.raw_type(), msg_len, msg.len());
            if let Some(container) = self.containers.get(container_id) {
                container.stats.increment_invalid(msg.len());
            }
            return;
        }

        if self.containers.get(container_id).is_none() {
            return;
        }

        self.drain_ring_buffer(self.ring_buffer.head());
        let old_compressed_tail = self.compressed_tail_message_id();
        self.append_message(container_id, &msg[..msg_len]);
        self.finish_ingest(old_compressed_tail);
    }

    /// Drains all records in `self.tail..head` from the ring buffer into `self.active_chunk`
    /// (sealing chunks into `self.data` as needed) and updates container stats and liveness.
    fn drain_ring_buffer(&mut self, head: u64) {
        if self.tail >= head {
            return;
        }
        let old_compressed_tail = self.compressed_tail_message_id();
        while self.tail < head {
            let index = self.tail;
            // Read the message length first so we can seal `active_chunk` before copying if needed.
            //
            // SAFETY: `index..head` is within the written range of the ring buffer
            // (`self.tail..head`), so there is no concurrent write access to this range. Only the
            // kernel and Archivist can write to the ring buffer and so we trust both not to corrupt
            // it.
            let msg_len = unsafe { self.ring_buffer.first_message_in(index..head) }
                .expect("Unable to read message from ring buffer")
                .1
                .len();
            let record_len = ring_buffer_record_len(msg_len);
            let record_bytes = 8 + msg_len;

            // If appending this record would cause `active_chunk` to exceed `CHUNK_SIZE` (64 KiB),
            // compress and seal the current `active_chunk` into `self.data` first.
            if !self.active_chunk.is_empty() && self.active_chunk.len() + record_bytes > CHUNK_SIZE
            {
                self.seal_active_chunk();
            }

            // Copy `[container_id, msg_len, message]` from the ring buffer into `active_chunk`.
            let (container_id, severity) = {
                let Inner { ring_buffer, active_chunk, .. } = &mut **self;
                // SAFETY: `index..head` is within `self.tail..head`.
                let (tag, message) = unsafe { ring_buffer.first_message_in(index..head) }
                    .expect("Unable to read message from ring buffer");
                let container_id = ContainerId(tag as u32);
                let severity = (msg_len >= 8)
                    .then(|| Header::read_from_bytes(&message[0..8]).unwrap().severity().into());
                active_chunk.extend_from_slice(&container_id.0.to_ne_bytes());
                active_chunk.extend_from_slice(&(msg_len as u32).to_ne_bytes());
                active_chunk.extend_from_slice(message);
                (container_id, severity)
            };

            self.ring_buffer.increment_tail(record_len);
            self.tail += record_len as u64;

            let msg_id = self.next_msg_id;
            self.next_msg_id += 1;
            self.active_msg_ids.end = self.next_msg_id;

            let container = self.containers.get_mut(container_id).unwrap();
            container.last_msg_id = Some(msg_id);
            if let Some(severity) = severity {
                container.stats.ingest_message(msg_len, severity);
            }
        }

        self.finish_ingest(old_compressed_tail);
    }

    /// Returns a cursor.
    fn cursor(&mut self, mode: StreamMode, selectors: Vec<ComponentSelector>) -> FilterCursor {
        let (message_id, end_msg_id) = match mode {
            StreamMode::Snapshot => {
                let head = self.ring_buffer.head();
                self.drain_ring_buffer(head);
                (self.compressed_tail_message_id(), Some(self.next_msg_id))
            }
            StreamMode::SnapshotThenSubscribe => (self.compressed_tail_message_id(), None),
            StreamMode::Subscribe => {
                let head = self.ring_buffer.head();
                self.drain_ring_buffer(head);
                (self.next_msg_id, None)
            }
        };
        FilterCursor::new(Arc::clone(self.buffer), message_id, end_msg_id, selectors)
    }
}

#[derive(Default)]
struct Containers {
    slab: Slab<ContainerInfo>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct ContainerId(u32);

impl Containers {
    #[cfg(test)]
    fn len(&self) -> usize {
        self.slab.len()
    }

    fn get(&self, id: ContainerId) -> Option<&ContainerInfo> {
        self.slab.get(id.0)
    }

    fn get_mut(&mut self, id: ContainerId) -> Option<&mut ContainerInfo> {
        self.slab.get_mut(id.0)
    }

    fn new_container(
        &mut self,
        identity: Arc<ComponentIdentity>,
        stats: Arc<LogStreamStats>,
    ) -> ContainerId {
        ContainerId(self.slab.insert(|_| ContainerInfo::new(identity, stats)))
    }

    fn free(&mut self, id: ContainerId) {
        self.slab.free(id.0);
    }
}

#[derive(Derivative)]
#[derivative(Debug)]
struct ContainerInfo {
    // The identity of the container.
    identity: Arc<ComponentIdentity>,

    // Message ID of the most recently extracted message for this container, if it is still present
    // in the compressed buffer.
    last_msg_id: Option<u64>,

    // Whether the container is terminated.
    terminated: bool,

    // Inspect instrumentation.
    #[derivative(Debug = "ignore")]
    stats: Arc<LogStreamStats>,

    // The number of client IOBuffers.
    iob_count: usize,
}

impl ContainerInfo {
    fn new(identity: Arc<ComponentIdentity>, stats: Arc<LogStreamStats>) -> Self {
        Self { identity, last_msg_id: None, terminated: false, stats, iob_count: 0 }
    }

    fn should_free(&self) -> bool {
        self.terminated && !self.is_active()
    }

    // Returns true if the container is considered active which is the case if it has IOBuffers or
    // buffered messages.
    //
    // NOTE: Whenever an IOBuffer is closed, `drain_ring_buffer` must be called to ensure
    // `last_msg_id` is up to date.
    fn is_active(&self) -> bool {
        self.iob_count > 0
            || self.last_msg_id.is_some()
            || ARCHIVIST_MONIKER.get().is_some_and(|m| *self.identity == *m)
    }
}

pub struct ContainerBuffer {
    shared_buffer: Arc<SharedBuffer>,
    container_id: ContainerId,
}

impl ContainerBuffer {
    /// Returns the tag used by IOBuffers used for this component.
    pub fn iob_tag(&self) -> u64 {
        self.container_id.0 as u64
    }

    /// Ingests a new message.
    ///
    /// If the message is invalid, it is dropped.
    pub fn push_back(&self, msg: &[u8]) {
        InnerGuard::new(&self.shared_buffer).ingest(msg, self.container_id);
    }

    /// Returns the identity of the container if it still exists.
    pub fn identity(&self) -> Option<Arc<ComponentIdentity>> {
        self.shared_buffer
            .inner
            .lock()
            .containers
            .get(self.container_id)
            .map(|c| Arc::clone(&c.identity))
    }

    /// Returns an IOBuffer for the container.
    pub fn iob(&self) -> zx::Iob {
        let mut inner = self.shared_buffer.inner.lock();
        inner.containers.get_mut(self.container_id).unwrap().iob_count += 1;
        let (ep0, ep1) = inner.ring_buffer.new_iob_writer(self.container_id.0 as u64).unwrap();
        let buffer = Arc::downgrade(&self.shared_buffer);
        let container_id = self.container_id;
        inner.iob_peers.insert(|peer_id| {
            let reg = fasync::EHandle::local().register_receiver(IobPacketReceiver {
                buffer,
                container_id,
                iob: ep1,
                peer_id,
            });
            reg.iob
                .wait_async(
                    reg.port(),
                    reg.key(),
                    zx::Signals::IOB_PEER_CLOSED,
                    zx::WaitAsyncOpts::empty(),
                )
                .unwrap();
            reg
        });

        ep0
    }

    /// Marks the buffer as terminated which will force all cursors to end. The component's data
    /// will remain in the buffer until the messages are rolled out. This will *not* close
    /// IOBuffers.
    pub fn terminate(&self) {
        let mut inner = InnerGuard::new(&self.shared_buffer);

        // See the comment on `is_active()` for why this is required.
        let head = inner.ring_buffer.head();
        inner.drain_ring_buffer(head);

        if let Some(container) = inner.containers.get_mut(self.container_id) {
            container.terminated = true;
            if container.should_free() {
                inner.containers.free(self.container_id);
            }
            inner.wake = true;
        }
    }

    /// Returns true if the container has messages or IOBuffers.
    pub fn is_active(&self) -> bool {
        self.shared_buffer
            .inner
            .lock()
            .containers
            .get(self.container_id)
            .is_some_and(|c| c.is_active())
    }
}

impl Drop for ContainerBuffer {
    fn drop(&mut self) {
        self.terminate();
    }
}

struct IobPacketReceiver {
    buffer: Weak<SharedBuffer>,
    container_id: ContainerId,
    iob: zx::Iob,
    peer_id: u32,
}

impl fasync::PacketReceiver for IobPacketReceiver {
    fn receive_packet(&self, _packet: zx::Packet) {
        let Some(buffer) = self.buffer.upgrade() else { return };
        let mut inner = InnerGuard::new(&buffer);

        // See the comment on `is_active()` for why this is required.
        let head = inner.ring_buffer.head();
        inner.drain_ring_buffer(head);

        inner.iob_peers.free(self.peer_id);
        if let Some(container) = inner.containers.get_mut(self.container_id) {
            container.iob_count -= 1;
            if container.iob_count == 0 && !container.is_active() {
                if container.should_free() {
                    inner.containers.free(self.container_id);
                } else {
                    let identity = Arc::clone(&container.identity);
                    inner.on_inactive.push(identity);
                }
            }
        }
    }
}

/// Implements a simple Slab allocator.
struct Slab<T> {
    slab: Vec<Slot<T>>,
    free_index: usize,
}

impl<T> Default for Slab<T> {
    fn default() -> Self {
        Self { slab: Vec::new(), free_index: usize::MAX }
    }
}

enum Slot<T> {
    Used(T),
    Free(usize),
}

impl<T> Slab<T> {
    /// Returns the number of used entries. This is not performant.
    #[cfg(test)]
    fn len(&self) -> usize {
        self.slab.iter().filter(|c| matches!(c, Slot::Used(_))).count()
    }

    fn free(&mut self, index: u32) -> T {
        let index = index as usize;
        let value = match std::mem::replace(&mut self.slab[index], Slot::Free(self.free_index)) {
            Slot::Free(_) => panic!("Slot already free"),
            Slot::Used(value) => value,
        };
        self.free_index = index;
        value
    }

    fn get(&self, id: u32) -> Option<&T> {
        self.slab.get(id as usize).and_then(|s| match s {
            Slot::Used(s) => Some(s),
            _ => None,
        })
    }

    fn get_mut(&mut self, id: u32) -> Option<&mut T> {
        self.slab.get_mut(id as usize).and_then(|s| match s {
            Slot::Used(s) => Some(s),
            _ => None,
        })
    }

    fn insert(&mut self, value: impl FnOnce(u32) -> T) -> u32 {
        let free_index = self.free_index;
        if free_index != usize::MAX {
            self.free_index = match std::mem::replace(
                &mut self.slab[free_index],
                Slot::Used(value(free_index as u32)),
            ) {
                Slot::Free(next) => next,
                _ => unreachable!(),
            };
            free_index as u32
        } else {
            assert!(self.slab.len() < u32::MAX as usize);
            self.slab.push(Slot::Used(value(self.slab.len() as u32)));
            (self.slab.len() - 1) as u32
        }
    }

    /// Returns the next used slot starting from `id` (inclusive).
    fn next_used(&self, id: u32) -> Option<u32> {
        let mut id = id as usize;
        while id < self.slab.len() {
            if matches!(self.slab[id], Slot::Used(_)) {
                return Some(id as u32);
            }
            id += 1;
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{SharedBuffer, SharedBufferOptions, Slab, create_ring_buffer};
    use crate::identity::ComponentIdentity;
    use crate::logs::stats::LogStreamStats;
    use crate::logs::testing::make_message;
    use assert_matches::assert_matches;
    use diagnostics_assertions::{AnyProperty, assert_data_tree};
    use fidl_fuchsia_diagnostics::StreamMode;
    use fuchsia_async as fasync;
    use fuchsia_async::TimeoutExt;
    use fuchsia_inspect::{Inspector, Node};
    use futures::FutureExt;
    use futures::channel::mpsc;
    use futures::future::OptionFuture;
    use futures::stream::{FuturesUnordered, StreamExt as _};
    use moniker::ExtendedMoniker;
    use ring_buffer::MAX_MESSAGE_SIZE;
    use selectors::SelectorExt;
    use std::future::poll_fn;
    use std::iter::repeat_with;
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::task::Poll;
    use std::time::Duration;
    use zerocopy::FromBytes;

    fn test_stats() -> Arc<LogStreamStats> {
        Arc::new(LogStreamStats::new(
            &fuchsia_inspect::Node::default(),
            &ComponentIdentity::unknown(),
        ))
    }

    fn container_cursor(
        buffer: &Arc<SharedBuffer>,
        container: &super::ContainerBuffer,
        mode: StreamMode,
    ) -> Option<impl futures::Stream<Item = Box<[u8]>>> {
        let selectors = vec![container.identity()?.moniker.clone().into_component_selector()];
        let mut cursor = Box::pin(buffer.cursor(mode, selectors));
        Some(futures::stream::poll_fn(move |cx| {
            cursor.as_mut().poll_next(cx).map(|m| m.map(|m| m.parse().unwrap().2.into()))
        }))
    }

    async fn yield_to_executor() {
        let mut first_time = true;
        poll_fn(|cx| {
            if first_time {
                cx.waker().wake_by_ref();
                first_time = false;
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        })
        .await;
    }

    #[fuchsia::test]
    async fn push_one_message() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );
        let container_buffer =
            buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));
        container_buffer.push_back(msg.bytes());

        // Make sure the cursor can find it.
        let cursor = container_cursor(&buffer, &container_buffer, StreamMode::Snapshot).unwrap();
        assert_eq!(cursor.map(|item| assert_eq!(&*item, msg.bytes())).count().await, 1);
    }

    #[fuchsia::test]
    async fn message_too_short() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );

        let container_buffer =
            buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        container_buffer.push_back(&[0]);

        assert_eq!(
            container_cursor(&buffer, &container_buffer, StreamMode::Snapshot)
                .unwrap()
                .count()
                .await,
            0
        );
    }

    #[fuchsia::test]
    async fn bad_type() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );
        let container_buffer =
            buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        container_buffer.push_back(&[0x77; 16]);

        assert_eq!(
            container_cursor(&buffer, &container_buffer, StreamMode::Snapshot)
                .unwrap()
                .count()
                .await,
            0
        );
    }

    #[fuchsia::test]
    async fn message_truncated() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );
        let container_buffer =
            buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));
        container_buffer.push_back(&msg.bytes()[..msg.bytes().len() - 1]);

        assert_eq!(
            container_cursor(&buffer, &container_buffer, StreamMode::Snapshot)
                .unwrap()
                .count()
                .await,
            0
        );
    }

    #[fuchsia::test]
    async fn buffer_wrapping() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            SharedBufferOptions { sleep_time: Duration::ZERO, ..Default::default() },
            &fuchsia_inspect::Node::default(),
        );
        let container_buffer =
            buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());

        // Keep writing messages until we wrap.
        let mut i = 0;
        loop {
            let msg = make_message(&format!("{i}"), None, zx::BootInstant::from_nanos(i));
            container_buffer.push_back(msg.bytes());
            i += 1;

            // Yield to the executor to allow messages to be rolled out.
            yield_to_executor().await;

            let inner = buffer.inner.lock();
            if inner.compressed_head > inner.data.len() as u64 {
                break;
            }
        }

        // Read back all the messages.
        let mut cursor =
            pin!(container_cursor(&buffer, &container_buffer, StreamMode::Snapshot).unwrap());

        let mut j;
        let item = cursor.next().await;
        assert_matches!(
            item,
            Some(item) => {
                j = i64::read_from_bytes(&item[8..16]).unwrap();
                let msg = make_message(&format!("{j}"),
                                       None,
                                       zx::BootInstant::from_nanos(j));
                assert_eq!(&*item, msg.bytes());
            }
        );

        j += 1;
        while j != i {
            assert_matches!(
                cursor.next().await,
                Some(item) => {
                    assert_eq!(&*item, make_message(&format!("{j}"),
                                                         None,
                                                         zx::BootInstant::from_nanos(j)).bytes());
                }
            );
            j += 1;
        }

        assert!(cursor.next().await.is_none());
    }

    #[fuchsia::test]
    async fn on_inactive() {
        let identity = Arc::new(vec!["a"].into());
        let on_inactive = Arc::new(AtomicU64::new(0));
        let buffer = {
            let on_inactive = Arc::clone(&on_inactive);
            let identity = Arc::clone(&identity);
            Arc::new(SharedBuffer::new(
                create_ring_buffer(MAX_MESSAGE_SIZE),
                Box::new(move |i| {
                    assert_eq!(i, identity);
                    on_inactive.fetch_add(1, Ordering::Relaxed);
                }),
                SharedBufferOptions { sleep_time: Duration::ZERO, ..Default::default() },
                &fuchsia_inspect::Node::default(),
            ))
        };
        let container_a = buffer.new_container_buffer(identity, test_stats());
        let container_b = buffer.new_container_buffer(Arc::new(vec!["b"].into()), test_stats());

        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));
        container_a.push_back(msg.bytes());

        // Repeatedly write messages to b until a is rolled out.
        while container_cursor(&buffer, &container_a, StreamMode::Snapshot).unwrap().count().await
            == 1
        {
            container_b.push_back(msg.bytes());

            // Yield to the executor to allow messages to be rolled out.
            yield_to_executor().await;
        }

        assert_eq!(on_inactive.load(Ordering::Relaxed), 1);
    }

    #[fuchsia::test]
    async fn terminate_drops_container() {
        // Silence a clippy warning; SharedBuffer needs an executor.
        async {}.await;

        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            SharedBufferOptions { sleep_time: Duration::ZERO, ..Default::default() },
            &fuchsia_inspect::Node::default(),
        );

        // terminate when buffer has no logs.
        let container_a = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        assert_eq!(buffer.container_count(), 1);
        container_a.terminate();

        assert_eq!(buffer.container_count(), 0);

        // terminate when buffer has logs.
        let container_a = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));
        container_a.push_back(msg.bytes());
        assert_eq!(buffer.container_count(), 1);
        container_a.terminate();

        // The container should still be there because it has logs.
        assert_eq!(buffer.container_count(), 1);

        // Roll out the logs.
        let container_b = buffer.new_container_buffer(Arc::new(vec!["b"].into()), test_stats());
        assert_eq!(buffer.container_count(), 2);

        // Repeatedly write messages to b until a's message is dropped and then the container will
        // be dropped.
        while buffer.container_count() != 1 {
            container_b.push_back(msg.bytes());

            // Yield to the executor to allow messages to be rolled out.
            yield_to_executor().await;
        }

        assert!(container_cursor(&buffer, &container_a, StreamMode::Subscribe).is_none());
    }

    #[fuchsia::test]
    async fn cursor_subscribe() {
        for mode in [StreamMode::Subscribe, StreamMode::SnapshotThenSubscribe] {
            let buffer = SharedBuffer::new(
                create_ring_buffer(MAX_MESSAGE_SIZE),
                Box::new(|_| {}),
                Default::default(),
                &fuchsia_inspect::Node::default(),
            );
            let container =
                Arc::new(buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats()));
            let msg = make_message("a", None, zx::BootInstant::from_nanos(1));
            container.push_back(msg.bytes());

            let (sender, mut receiver) = mpsc::unbounded();

            // Run the cursor in a separate task so that we can test it gets woken correctly.
            {
                let container = Arc::clone(&container);
                fasync::Task::spawn(async move {
                    let mut cursor =
                        pin!(container_cursor(&container.shared_buffer, &container, mode).unwrap());
                    while let Some(item) = cursor.next().await {
                        sender.unbounded_send(item).unwrap();
                    }
                })
                .detach();
            }

            // The existing message should only be returned with SnapshotThenSubscribe
            if mode == StreamMode::SnapshotThenSubscribe {
                assert_matches!(
                    receiver.next().await,
                    Some(item) if item.as_ref() == msg.bytes()
                );
            }

            // No message should arrive. We can only use a timeout here.
            assert!(
                OptionFuture::from(Some(receiver.next()))
                    .on_timeout(Duration::from_millis(500), || None)
                    .await
                    .is_none()
            );

            container.push_back(msg.bytes());

            // The message should arrive now.
            assert_matches!(
                receiver.next().await,
                Some(item) if item.as_ref() == msg.bytes()
            );
        }
    }

    #[fuchsia::test]
    async fn drained_post_termination_cursors() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );
        let container =
            Arc::new(buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats()));
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));

        let mut cursor_a =
            pin!(container_cursor(&buffer, &container, StreamMode::Subscribe).unwrap());
        let mut cursor_b =
            pin!(container_cursor(&buffer, &container, StreamMode::SnapshotThenSubscribe).unwrap());

        container.push_back(msg.bytes());
        container.push_back(msg.bytes());
        container.push_back(msg.bytes());
        container.push_back(msg.bytes());
        container.push_back(msg.bytes());

        let mut cursor_c =
            pin!(container_cursor(&buffer, &container, StreamMode::Snapshot).unwrap());
        assert!(cursor_a.next().await.is_some());
        assert!(cursor_b.next().await.is_some());
        assert!(cursor_c.next().await.is_some());

        buffer.terminate();

        // All cursors should return the 4 remaining messages.
        assert_eq!(cursor_a.count().await, 4);
        assert_eq!(cursor_b.count().await, 4);
        assert_eq!(cursor_c.count().await, 4);
    }

    #[fuchsia::test]
    async fn empty_post_termination_cursors() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );
        let container =
            Arc::new(buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats()));

        let cursor_a = container_cursor(&buffer, &container, StreamMode::Subscribe).unwrap();
        let cursor_b =
            container_cursor(&buffer, &container, StreamMode::SnapshotThenSubscribe).unwrap();
        let cursor_c = container_cursor(&buffer, &container, StreamMode::Snapshot).unwrap();

        buffer.terminate();

        assert_eq!(cursor_a.count().await, 0);
        assert_eq!(cursor_b.count().await, 0);
        assert_eq!(cursor_c.count().await, 0);
    }

    #[fuchsia::test]
    async fn recycled_container_slot() {
        let buffer = Arc::new(SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            Box::new(|_| {}),
            SharedBufferOptions { sleep_time: Duration::ZERO, ..Default::default() },
            &fuchsia_inspect::Node::default(),
        ));
        let container_a =
            Arc::new(buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats()));
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));
        container_a.push_back(msg.bytes());

        let mut cursor = pin!(
            container_cursor(&buffer, &container_a, StreamMode::SnapshotThenSubscribe).unwrap()
        );
        assert_matches!(cursor.next().await, Some(_));

        // Roll out all the messages.
        let container_b =
            Arc::new(buffer.new_container_buffer(Arc::new(vec!["b"].into()), test_stats()));
        while container_cursor(&buffer, &container_a, StreamMode::Snapshot).unwrap().count().await
            > 0
        {
            container_b.push_back(msg.bytes());

            // Yield to the executor to allow messages to be rolled out.
            yield_to_executor().await;
        }

        container_a.terminate();

        // This should create a new container that uses a new slot and shouldn't interfere with
        // container_a.
        let container_c =
            Arc::new(buffer.new_container_buffer(Arc::new(vec!["c"].into()), test_stats()));
        container_c.push_back(msg.bytes());
        container_c.push_back(msg.bytes());
    }

    #[fuchsia::test]
    async fn iob_increments_logstats() {
        let inspector = Inspector::default();
        let identity =
            Arc::new(ComponentIdentity::new(ExtendedMoniker::parse_str("./test").unwrap(), ""));
        let stats = Arc::new(LogStreamStats::new(inspector.root(), &identity));
        let buffer = SharedBuffer::new(
            create_ring_buffer(65536),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );
        let container_a = Arc::new(buffer.new_container_buffer(Arc::new(vec!["a"].into()), stats));
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));

        let iob = container_a.iob();

        let cursor_a = container_cursor(&buffer, &container_a, StreamMode::Subscribe).unwrap();

        // Use FuturesUnordered so that we can make sure that the cursor is woken when a message is
        // received (FuturesUnordered uses separate wakers for all the futures it manages).
        let mut futures = FuturesUnordered::new();
        futures.push(async move {
            let mut cursor_a = pin!(cursor_a);
            cursor_a.next().await
        });
        let mut next = futures.next();
        assert!(futures::poll!(&mut next).is_pending());

        iob.write(Default::default(), 0, msg.bytes()).unwrap();

        let cursor_b = pin!(container_cursor(&buffer, &container_a, StreamMode::Snapshot).unwrap());

        assert_eq!(cursor_b.map(|item| assert_eq!(&*item, msg.bytes())).count().await, 1);

        // If cursor_a wasn't woken, this will hang.
        next.await;

        // Validate logstats (must happen after the ring buffer was drained).
        assert_data_tree!(
            inspector,
            root: contains {
                test: {
                    url: "",
                    last_timestamp: AnyProperty,
                    invalid: {
                        number: 0u64,
                        bytes: 0u64,
                    },
                    total: {
                        number: 1u64,
                        bytes: 88u64,
                    },
                    trace: {
                        number: 0u64,
                        bytes: 0u64,
                    },
                    debug: {
                        number: 1u64,
                        bytes: 88u64,
                    },
                    info: {
                        number: 0u64,
                        bytes: 0u64,
                    },
                    warn: {
                        number: 0u64,
                        bytes: 0u64,
                    },
                    error: {
                        number: 0u64,
                        bytes: 0u64,
                    },
                    fatal: {
                        number: 0u64,
                        bytes: 0u64,
                    },
                }
            }
        );
    }

    #[fuchsia::test]
    async fn iob_on_inactive() {
        let on_inactive = Arc::new(AtomicU64::new(0));
        let a_identity = Arc::new(vec!["a"].into());
        let buffer = SharedBuffer::new(
            create_ring_buffer(MAX_MESSAGE_SIZE),
            {
                let on_inactive = Arc::clone(&on_inactive);
                let a_identity = Arc::clone(&a_identity);
                Box::new(move |id| {
                    assert_eq!(id, a_identity);
                    on_inactive.fetch_add(1, Ordering::Relaxed);
                })
            },
            SharedBufferOptions { sleep_time: Duration::ZERO, ..Default::default() },
            &fuchsia_inspect::Node::default(),
        );
        let container_a = Arc::new(buffer.new_container_buffer(a_identity, test_stats()));
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));

        let iob = container_a.iob();
        iob.write(Default::default(), 0, msg.bytes()).unwrap();

        let cursor = pin!(container_cursor(&buffer, &container_a, StreamMode::Snapshot).unwrap());

        assert_eq!(cursor.map(|item| assert_eq!(&*item, msg.bytes())).count().await, 1);

        // Now roll out a's messages.
        let container_b = buffer.new_container_buffer(Arc::new(vec!["b"].into()), test_stats());
        while container_cursor(&buffer, &container_a, StreamMode::Snapshot).unwrap().count().await
            == 1
        {
            container_b.push_back(msg.bytes());

            // Yield to the executor to allow messages to be rolled out.
            yield_to_executor().await;
        }

        assert_eq!(on_inactive.load(Ordering::Relaxed), 0);

        // Close the IOBuffer.
        std::mem::drop(iob);

        while on_inactive.load(Ordering::Relaxed) != 1 {
            yield_to_executor().await;
        }
    }

    // `SharedBuffer::new` spawns a task, which requires an active executor (provided by
    // `#[fuchsia::test]` when the test function is `async`).
    #[fuchsia::test]
    #[allow(clippy::unused_async)]
    async fn flush() {
        let a_identity = Arc::new(vec!["a"].into());
        let buffer = SharedBuffer::new(
            create_ring_buffer(1024 * 1024),
            Box::new(|_| {}),
            Default::default(),
            &fuchsia_inspect::Node::default(),
        );
        let container_a = Arc::new(buffer.new_container_buffer(a_identity, test_stats()));
        let msg = make_message("a", None, zx::BootInstant::from_nanos(1));

        let iob = container_a.iob();

        let cursor = pin!(container_cursor(&buffer, &container_a, StreamMode::Subscribe).unwrap());

        const COUNT: usize = 1000;
        for _ in 0..COUNT {
            iob.write(Default::default(), 0, msg.bytes()).unwrap();
        }

        buffer.flush();

        let messages: Option<Vec<_>> = cursor.take(COUNT).collect().now_or_never();
        assert_eq!(messages.map(|m| m.len()), Some(COUNT));

        // Make sure we can still terminate the buffer.
        buffer.terminate();
    }

    #[fuchsia::test]
    fn test_slab_next_used() {
        let mut slab = Slab::default();
        let mut ids: Vec<_> = repeat_with(|| slab.insert(|_| ())).take(4).collect();
        ids.sort();
        slab.free(ids[0]);
        slab.free(ids[2]);
        assert_eq!(slab.next_used(0), Some(ids[1]));
        assert_eq!(slab.next_used(ids[1]), Some(ids[1]));
        assert_eq!(slab.next_used(ids[1] + 1), Some(ids[3]));
        assert_eq!(slab.next_used(ids[3] + 1), None);
    }

    #[fuchsia::test]
    async fn small_capacity_incompressible_chunk() {
        let buffer = SharedBuffer::new(
            create_ring_buffer(0),
            Box::new(|_| {}),
            Default::default(),
            &Node::default(),
        );
        let container = buffer.new_container_buffer(Arc::new(vec!["a"].into()), test_stats());

        // Push > 64 KiB (`CHUNK_SIZE`) of incompressible messages to force sealing an
        // incompressible chunk when the requested capacity was smaller than `CHUNK_SIZE`.
        let mut rng_state = 0x1234_5678_9abc_def0u64;
        for ts in 0..70 {
            let text: String = (0..1000)
                .map(|_| {
                    rng_state = rng_state.wrapping_mul(6364136223846793005).wrapping_add(1);
                    (b'!' + ((rng_state >> 32) % 94) as u8) as char
                })
                .collect();
            let msg = make_message(&text, None, zx::BootInstant::from_nanos(ts));
            container.push_back(msg.bytes());
        }

        let cursor = pin!(container_cursor(&buffer, &container, StreamMode::Snapshot).unwrap());
        let messages: Vec<_> = cursor.collect().await;
        assert!(!messages.is_empty());

        buffer.terminate();
    }
}
