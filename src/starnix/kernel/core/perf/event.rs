// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use crate::task::Kernel;
use crate::vfs::OutputBuffer;
use fuchsia_inspect::Property;
use fuchsia_rcu::RcuDroppableArc;
use paged_ring_buffer::LocklessRingBuffer;
use starnix_logging::log_error;
use starnix_sync::{LockDepMutex, PerfTraceEventQueueListLock};
use starnix_types::PAGE_SIZE;
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use zerocopy::native_endian::{I32, U16, U32, U64};
use zerocopy::{Immutable, IntoBytes, Unaligned};
use zx::BootTimeline;

// The default ring buffer size (2MB).
const DEFAULT_RING_BUFFER_SIZE_BYTES: usize = 2097152;

// The event id for atrace events.
const FTRACE_PRINT_ID: U16 = U16::new(5);

// Used for inspect tracking.
const DROPPED_PAGES: &str = "dropped_pages";

#[repr(C)]
#[derive(Debug, Default, IntoBytes, Immutable, Unaligned)]
struct PrintEventHeader {
    common_type: U16,
    common_flags: u8,
    common_preempt_count: u8,
    common_pid: I32,
    ip: U64,
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, Unaligned)]
struct PrintEvent {
    header: PrintEventHeader,
}

impl PrintEvent {
    fn new(pid: i32) -> Self {
        Self {
            header: PrintEventHeader {
                common_type: FTRACE_PRINT_ID,
                common_pid: I32::new(pid),
                // Perfetto doesn't care about any other field.
                ..Default::default()
            },
        }
    }

    fn size(&self) -> usize {
        std::mem::size_of::<PrintEventHeader>()
    }
}

#[repr(C)]
#[derive(Debug, Default, IntoBytes, PartialEq, Immutable, Unaligned)]
struct TraceEventHeader {
    // u32 where:
    //   type_or_length: bottom 5 bits. If 0, `data` is read for length. Always set to 0 for now.
    //   time_delta: top 27 bits
    time_delta: U32,

    // If type_or_length is 0, holds the length of the trace message.
    // We always write length here for simplicity.
    data: U32,
}

impl TraceEventHeader {
    fn new(size: usize) -> Self {
        // The size reported in the event's header includes the size of `size` (a u32) and the size
        // of the event data.
        let size = (std::mem::size_of::<u32>() + size) as u32;
        Self { time_delta: U32::new(0), data: U32::new(size) }
    }

    fn set_time_delta(&mut self, nanos: u32) {
        // `Reservation::header_time_delta()` already fits in 27 bits: the ring buffer puts a
        // TIME_STAMP record before a record whose delta doesn't. Saturate anyway, so that a bug
        // can't spill into the `type_or_length` bits.
        let saturated_nanos = u64::from(nanos).min(LocklessRingBuffer::MAX_TIME_DELTA_NANOS) as u32;
        // Move into the high 27 bits reserving the lower 5 bits for the type_or_length value.
        self.time_delta = U32::new(saturated_nanos << LocklessRingBuffer::TYPE_OR_LENGTH_BITS);
    }
}

#[repr(C)]
#[derive(Debug, IntoBytes, Immutable, Unaligned)]
pub struct TraceEvent {
    /// Common metadata among all trace event types.
    header: TraceEventHeader, // u64

    /// The event data.
    ///
    /// Atrace events are reported as PrintFtraceEvents. When we support multiple types of events,
    /// this can be updated to be more generic.
    event: PrintEvent,
}

impl TraceEvent {
    pub fn new(pid: i32, data_len: usize) -> Self {
        let event = PrintEvent::new(pid);
        // +1 because we append a trailing '\n' to the data when we serialize.
        let header = TraceEventHeader::new(event.size() + data_len + 1);
        Self { header, event }
    }

    fn size(&self) -> usize {
        // The header's data size doesn't include the time_delta size.
        std::mem::size_of::<u32>() + self.header.data.get() as usize
    }
}

/// Stores all trace events.
pub struct TraceEventQueue {
    /// The trace events.
    ring_buffer: RcuDroppableArc<LocklessRingBuffer>,

    /// Async ID for read track grouping.
    pub async_id_read: fuchsia_trace::Id,

    /// Async ID for write track grouping.
    pub async_id_write: fuchsia_trace::Id,

    /// CPU ID for trace tracks.
    pub cpu_id: u32,
}

const STATIC_READ_TRACK_NAMES: [&str; 8] = [
    "Tracefs read 0",
    "Tracefs read 1",
    "Tracefs read 2",
    "Tracefs read 3",
    "Tracefs read 4",
    "Tracefs read 5",
    "Tracefs read 6",
    "Tracefs read 7",
];

const STATIC_WRITE_TRACK_NAMES: [&str; 8] = [
    "Tracefs write 0",
    "Tracefs write 1",
    "Tracefs write 2",
    "Tracefs write 3",
    "Tracefs write 4",
    "Tracefs write 5",
    "Tracefs write 6",
    "Tracefs write 7",
];

impl<'a> TraceEventQueue {
    pub(crate) fn new(cpu_id: u32) -> Result<Self, Errno> {
        Self::new_with_size(cpu_id, DEFAULT_RING_BUFFER_SIZE_BYTES)
    }

    pub(crate) fn new_with_size(cpu_id: u32, size_bytes: usize) -> Result<Self, Errno> {
        let async_id_read = fuchsia_trace::Id::new();
        let async_id_write = fuchsia_trace::Id::new();
        let ring_buffer = Arc::new(
            LocklessRingBuffer::new(size_bytes, true, async_id_write)
                .map_err(|_| errno!(ENOMEM))?,
        );
        ring_buffer.disable()?;

        Ok(Self {
            ring_buffer: RcuDroppableArc::new(ring_buffer),
            async_id_read,
            async_id_write,
            cpu_id,
        })
    }

    pub fn read_track_name(&self) -> std::borrow::Cow<'static, str> {
        if let Some(&name) = STATIC_READ_TRACK_NAMES.get(self.cpu_id as usize) {
            std::borrow::Cow::Borrowed(name)
        } else {
            std::borrow::Cow::Owned(format!("Tracefs read {}", self.cpu_id))
        }
    }

    pub fn write_track_name(&self) -> std::borrow::Cow<'static, str> {
        if let Some(&name) = STATIC_WRITE_TRACK_NAMES.get(self.cpu_id as usize) {
            std::borrow::Cow::Borrowed(name)
        } else {
            std::borrow::Cow::Owned(format!("Tracefs write {}", self.cpu_id))
        }
    }

    fn enable(&self) -> Result<zx::BootInstant, Errno> {
        self.ring_buffer.read().enable()
    }

    /// Disables the event queue and resets it to empty.
    /// The number of dropped pages are recorded for reading via tracefs.
    fn disable(&self) -> Result<u64, Errno> {
        self.ring_buffer.read().disable()
    }

    /// Reads a page worth of events. Currently only reads pages that are full.
    ///
    /// From https://docs.kernel.org/trace/ring-buffer-design.html, when memory is mapped, a reader
    /// page can be swapped with the header page to avoid copying memory.
    pub fn read(&self, buf: &mut dyn OutputBuffer) -> Result<usize, Errno> {
        self.ring_buffer.read().read(&mut RingOutputBuffer(buf))
    }

    /// Write `event` into `ring_buffer`.
    /// If `event` does not fit in the current page, move on to the next.
    ///
    /// Should eventually allow for a writer to preempt another writer.
    /// See https://docs.kernel.org/trace/ring-buffer-design.html.
    /// Returns the delta duration between this event and the previous event written.
    pub fn push_event(
        &self,
        event: TraceEvent,
        data: &[u8],
    ) -> Result<zx::Duration<BootTimeline>, Errno> {
        self.push_event_with_clock(event, data, zx::BootInstant::get)
    }

    /// Like `push_event()`, but reads the current time from `current_boot_time`, so that tests can
    /// control timestamps.
    fn push_event_with_clock(
        &self,
        mut event: TraceEvent,
        data: &[u8],
        current_boot_time: impl Fn() -> zx::BootInstant,
    ) -> Result<zx::Duration<BootTimeline>, Errno> {
        let size = event.size();
        let ring_buffer = self.ring_buffer.read();

        let (res, _timestamp, delta) = match ring_buffer.reserve_with_clock(size, current_boot_time)
        {
            Ok(res) => res,
            Err(e) if e == errno!(EINVAL) => {
                log_error!("Invalid reservation size: {}", size);
                return Err(errno!(EINVAL));
            }
            Err(e) if e == errno!(ENOSPC) => {
                log_error!("Ring buffer full, dropping event of size: {}", size);
                return Err(errno!(ENOSPC));
            }
            Err(e) => return Err(e),
        };

        event.header.set_time_delta(res.header_time_delta());

        let bytes = event.as_bytes();
        res.write_at(0, bytes);
        res.write_at(bytes.len(), data);
        res.write_at(bytes.len() + data.len(), b"\n");

        ring_buffer.commit(res);

        Ok(delta)
    }
}

pub struct TraceEventQueueList {
    pub queues: Vec<Arc<TraceEventQueue>>,
    // Serializes `enable()`, `disable()`, and `set_buffer_size_kb()`, so that the queues,
    // `tracing_enabled`, and `configured_buffer_size_kb` change together, as Linux
    // `trace_types_lock` does. Readers of `tracing_enabled` and `configured_buffer_size_kb`
    // and writers to the queues do not take it.
    state_mutex: LockDepMutex<(), PerfTraceEventQueueListLock>,
    tracing_enabled: Arc<AtomicBool>,
    configured_buffer_size_kb: AtomicUsize,
    tracefs_node: fuchsia_inspect::Node,
    buffer_size_kb_property: fuchsia_inspect::UintProperty,
    total_capacity_bytes_property: fuchsia_inspect::UintProperty,
}

impl TraceEventQueueList {
    pub fn from(kernel: &Kernel) -> Arc<Self> {
        kernel.expando.get_or_init(|| {
            let num_cpus = zx::system_get_num_cpus() as usize;
            Self::new(num_cpus, kernel.inspect_node.create_child("tracefs"))
        })
    }

    #[cfg(test)]
    pub(crate) fn new_for_testing(
        num_cpus: usize,
        inspect_parent: &fuchsia_inspect::Node,
    ) -> Arc<Self> {
        Arc::new(Self::new(num_cpus, inspect_parent.create_child("tracefs")))
    }

    fn new(num_cpus: usize, tracefs_node: fuchsia_inspect::Node) -> Self {
        let default_size_kb = DEFAULT_RING_BUFFER_SIZE_BYTES / 1024;
        let buffer_size_kb_property =
            tracefs_node.create_uint("buffer_size_kb", default_size_kb as u64);
        let total_capacity_bytes_property = tracefs_node.create_uint(
            "total_capacity_bytes",
            (DEFAULT_RING_BUFFER_SIZE_BYTES * num_cpus) as u64,
        );
        let queues = (0..num_cpus)
            .map(|cpu| Arc::new(TraceEventQueue::new(cpu as u32).expect("create queue")))
            .collect();
        Self {
            queues,
            state_mutex: Default::default(),
            tracing_enabled: Arc::new(AtomicBool::new(false)),
            configured_buffer_size_kb: AtomicUsize::new(default_size_kb),
            tracefs_node,
            buffer_size_kb_property,
            total_capacity_bytes_property,
        }
    }

    pub fn is_enabled(&self) -> bool {
        self.tracing_enabled.load(Ordering::Acquire)
    }

    pub fn buffer_size_kb(&self) -> usize {
        self.configured_buffer_size_kb.load(Ordering::Acquire)
    }

    /// Replaces every queue's ring buffer with an empty one of at least `size_kb` KiB, rounded up
    /// to whole pages and to `LocklessRingBuffer`'s 3-page minimum, and returns the size used.
    /// The new buffers keep the current `tracing_on` state.
    ///
    /// Unlike Linux `ring_buffer_resize()`, which adds or removes pages in place, this discards
    /// every unread event, including events from writers still holding the old buffer. The old
    /// buffer's dropped-page count is discarded too, so `DROPPED_PAGES` misses it.
    // TODO(https://fxbug.dev/574072344): Count the events and pages a resize discards.
    pub fn set_buffer_size_kb(&self, size_kb: usize) -> Result<usize, Errno> {
        if size_kb == 0 {
            return error!(EINVAL);
        }
        let _lock = self.state_mutex.lock();
        let enabled = self.is_enabled();

        let page_size_kb = (*PAGE_SIZE as usize) / 1024;
        let requested_pages = size_kb.div_ceil(page_size_kb);
        // LocklessRingBuffer requires at least 3 pages (read page, head page, and swap page).
        let pages = std::cmp::max(3, requested_pages);
        let actual_size_kb = pages.checked_mul(page_size_kb).ok_or_else(|| errno!(ENOMEM))?;
        let target_bytes = actual_size_kb.checked_mul(1024).ok_or_else(|| errno!(ENOMEM))?;
        let total_capacity_bytes =
            target_bytes.checked_mul(self.queues.len()).ok_or_else(|| errno!(ENOMEM))?;

        // Transactional allocation: pre-allocate all per-CPU ring buffers first.
        let mut new_buffers = Vec::with_capacity(self.queues.len());
        for queue in &self.queues {
            let ring_buffer = Arc::new(
                LocklessRingBuffer::new(target_bytes, true, queue.async_id_write)
                    .map_err(|_| errno!(ENOMEM))?,
            );
            if !enabled {
                ring_buffer.disable()?;
            }
            new_buffers.push(ring_buffer);
        }

        // All succeeded; swap in the new buffers.
        for (queue, new_buffer) in self.queues.iter().zip(new_buffers) {
            queue.ring_buffer.update(new_buffer);
        }

        self.configured_buffer_size_kb.store(actual_size_kb, Ordering::Release);
        self.buffer_size_kb_property.set(actual_size_kb as u64);
        self.total_capacity_bytes_property.set(total_capacity_bytes as u64);

        Ok(actual_size_kb)
    }

    /// Enables every queue. A queue that is already enabled is left alone, as Linux
    /// `rb_simple_write()` does when `tracing_on` is set to its current state. Unlike Linux, which
    /// keeps the buffer readable after `tracing_on` is cleared, `disable()` discards each queue's
    /// contents, so enabling a disabled queue starts from empty.
    pub fn enable(&self) -> Result<(), Errno> {
        let _lock = self.state_mutex.lock();
        let mut first_error = None;
        for queue in &self.queues {
            if let Err(e) = queue.enable() {
                if first_error.is_none() {
                    first_error = Some(e);
                }
            }
        }
        if let Some(e) = first_error {
            return Err(e);
        }
        self.tracing_enabled.store(true, Ordering::Release);
        Ok(())
    }

    pub fn disable(&self) -> Result<(), Errno> {
        let _lock = self.state_mutex.lock();
        // Set disabled to stop new access to the queue, then clean up each one.
        self.tracing_enabled.store(false, Ordering::Release);
        let mut first_error = None;
        let mut total_dropped = 0;
        for queue in &self.queues {
            match queue.disable() {
                Ok(dropped) => total_dropped += dropped,
                Err(e) => {
                    if first_error.is_none() {
                        first_error = Some(e);
                    }
                }
            }
        }

        self.tracefs_node.record_uint(DROPPED_PAGES, total_dropped);

        if let Some(e) = first_error {
            return Err(e);
        }
        Ok(())
    }
}

/// Adapts a starnix_core `OutputBuffer` to the `paged_ring_buffer::OutputBuffer` that
/// `LocklessRingBuffer::read()` takes.
struct RingOutputBuffer<'a>(&'a mut dyn OutputBuffer);

impl paged_ring_buffer::OutputBuffer for RingOutputBuffer<'_> {
    fn available(&self) -> usize {
        self.0.available()
    }
    fn write_each(
        &mut self,
        callback: &mut paged_ring_buffer::OutputBufferCallback<'_>,
    ) -> Result<usize, Errno> {
        self.0.write_each(callback)
    }
}

#[cfg(test)]
mod tests {
    use super::{
        DEFAULT_RING_BUFFER_SIZE_BYTES, LocklessRingBuffer, PrintEvent, TraceEvent,
        TraceEventQueue, TraceEventQueueList,
    };
    use crate::vfs::OutputBuffer;
    use crate::vfs::buffers::VecOutputBuffer;
    use diagnostics_assertions::assert_data_tree;
    use std::sync::Arc;

    use starnix_types::PAGE_SIZE;
    use starnix_uapi::error;
    use zerocopy::IntoBytes;

    #[fuchsia::test]
    fn trace_event_queue_empty_errors() {
        let queue = TraceEventQueue::new(0).unwrap();

        let mut buffer = VecOutputBuffer::new(*PAGE_SIZE as usize);
        assert_eq!(queue.read(&mut buffer), error!(EAGAIN));

        let data = b"B|1234|slice_name";
        let event = TraceEvent::new(1234, data.len());
        assert_eq!(queue.push_event(event, data), error!(ENOMEM));
    }

    #[fuchsia::test]
    fn read_empty_queue() {
        let queue = TraceEventQueue::new(0).expect("create queue");
        let mut buffer = VecOutputBuffer::new(*PAGE_SIZE as usize);
        assert_eq!(queue.read(&mut buffer), error!(EAGAIN));
    }

    #[fuchsia::test]
    fn enable_disable_queue() {
        let queue = TraceEventQueue::new(0).expect("create queue");
        assert!(!queue.ring_buffer.read().is_enabled());

        // Enable tracing.
        assert!(queue.enable().is_ok());

        // Confirm we can push an event.
        let data = b"B|1234|slice_name";
        let event = TraceEvent::new(1234, data.len());
        let result = queue.push_event(event, data);

        assert!(result.is_ok());
        assert_eq!(result.as_ref().unwrap().into_nanos(), 0);

        // Disable tracing and check that the queue's state has been reset.
        assert!(queue.disable().is_ok());
        assert!(!queue.ring_buffer.read().is_enabled());
    }

    #[fuchsia::test]
    fn buffer_size_kb_configuration() {
        let inspector = fuchsia_inspect::Inspector::default();
        let list = TraceEventQueueList::new_for_testing(2, inspector.root());
        assert_eq!(list.buffer_size_kb(), DEFAULT_RING_BUFFER_SIZE_BYTES / 1024);

        // Resize when disabled succeeds.
        let new_size_kb = list.set_buffer_size_kb(4096).expect("set buffer size");
        assert_eq!(new_size_kb, 4096);
        assert_eq!(list.buffer_size_kb(), 4096);
        for queue in &list.queues {
            assert_eq!(queue.ring_buffer.read().size_bytes(), 4096 * 1024);
            assert!(!queue.ring_buffer.read().is_enabled());
        }

        // Small size is clamped to minimum 3 pages (12 KB on 4KB page system).
        let clamped_size_kb = list.set_buffer_size_kb(4).expect("clamp buffer size");
        assert_eq!(clamped_size_kb, 12);
        assert_eq!(list.buffer_size_kb(), 12);
        for queue in &list.queues {
            assert_eq!(queue.ring_buffer.read().size_bytes(), 12 * 1024);
            assert!(!queue.ring_buffer.read().is_enabled());
        }

        // Zero returns EINVAL.
        assert_eq!(list.set_buffer_size_kb(0), error!(EINVAL));

        // When enabled, resizing succeeds and keeps the new buffers enabled.
        list.enable().expect("enable tracing");
        let enabled_size_kb = list.set_buffer_size_kb(2048).expect("set buffer size while enabled");
        assert_eq!(enabled_size_kb, 2048);
        assert_eq!(list.buffer_size_kb(), 2048);
        for queue in &list.queues {
            assert_eq!(queue.ring_buffer.read().size_bytes(), 2048 * 1024);
            assert!(queue.ring_buffer.read().is_enabled());
        }
        list.disable().expect("disable tracing");
    }

    #[fuchsia::test]
    async fn buffer_size_kb_inspect_updates_in_place() {
        let inspector = fuchsia_inspect::Inspector::default();
        let list = TraceEventQueueList::new_for_testing(2, inspector.root());
        assert_data_tree!(inspector, root: {
            tracefs: {
                buffer_size_kb: (DEFAULT_RING_BUFFER_SIZE_BYTES / 1024) as u64,
                total_capacity_bytes: (DEFAULT_RING_BUFFER_SIZE_BYTES * 2) as u64,
            }
        });

        // Each resize must update the existing properties rather than add new ones, so the tree
        // must still have exactly one of each.
        list.set_buffer_size_kb(4096).expect("set buffer size");
        list.set_buffer_size_kb(1024).expect("set buffer size");
        assert_data_tree!(inspector, root: {
            tracefs: {
                buffer_size_kb: 1024u64,
                total_capacity_bytes: (1024 * 1024 * 2) as u64,
            }
        });
    }

    #[fuchsia::test]
    fn create_trace_event() {
        // Create an event.
        let event = TraceEvent::new(1234, b"B|1234|slice_name".len());
        let event_size = event.size();
        assert_eq!(event_size, 42);
    }

    // This can be removed when we support reading incomplete pages.
    #[fuchsia::test]
    fn single_trace_event_fails_read() {
        let queue = TraceEventQueue::new(0).expect("create queue");
        queue.enable().expect("enable queue");
        // Create an event.
        let data = b"B|1234|slice_name";
        let event = TraceEvent::new(1234, data.len());

        // Push the event into the queue.
        let result = queue.push_event(event, data);
        assert!(result.is_ok());
        assert_eq!(result.ok().expect("delta").into_nanos(), 0);

        let mut buffer = VecOutputBuffer::new(*PAGE_SIZE as usize);
        assert_eq!(queue.read(&mut buffer), error!(EAGAIN));
    }

    const TYPE_OR_LENGTH_MASK: u32 = (1 << LocklessRingBuffer::TYPE_OR_LENGTH_BITS) - 1;
    // The `type_or_length` of a data record whose length is in the u32 after the header.
    const TYPE_OR_LENGTH_DATA: u32 = 0;

    /// A data record from an ftrace page.
    struct PageRecord {
        /// The absolute time from the `TIME_STAMP` record right before this record, if any.
        time_stamp: Option<u64>,
        /// The record header's time delta field.
        time_delta: u32,
        /// The length word, which counts itself and the body.
        length: u32,
        /// The bytes after the length word.
        body: Vec<u8>,
    }

    /// Splits an ftrace page into its data records. Panics on any other record type, on a
    /// `TIME_STAMP` record that no data record follows, or on a record that runs past the page's
    /// data size.
    fn page_records(page: &[u8]) -> Vec<PageRecord> {
        let data_size = u64::from_le_bytes(page[8..16].try_into().unwrap()) as usize;
        let end = LocklessRingBuffer::PAGE_HEADER_SIZE + data_size;
        assert!(end <= page.len(), "data size {data_size} runs past the page");
        let word = |at: usize| {
            assert!(at + 4 <= end, "read at {at} runs past the page data ({end})");
            u32::from_le_bytes(page[at..at + 4].try_into().unwrap())
        };
        let mut records = vec![];
        let mut time_stamp = None;
        let mut offset = LocklessRingBuffer::PAGE_HEADER_SIZE;
        while offset < end {
            let header = word(offset);
            let time_delta = header >> LocklessRingBuffer::TYPE_OR_LENGTH_BITS;
            match header & TYPE_OR_LENGTH_MASK {
                LocklessRingBuffer::RINGBUF_TYPE_TIME_STAMP => {
                    assert!(time_stamp.is_none(), "two TIME_STAMP records in a row at {offset}");
                    let high = word(offset + 4);
                    time_stamp = Some(
                        (u64::from(high) << LocklessRingBuffer::TIME_DELTA_BITS)
                            | u64::from(time_delta),
                    );
                    offset += LocklessRingBuffer::TIME_STAMP_RECORD_SIZE;
                }
                TYPE_OR_LENGTH_DATA => {
                    let length = word(offset + 4);
                    let record_end = offset + 4 + length as usize;
                    assert!(record_end <= end, "record at {offset} runs past the page data");
                    let body = page[offset + 8..record_end].to_vec();
                    records.push(PageRecord {
                        time_stamp: time_stamp.take(),
                        time_delta,
                        length,
                        body,
                    });
                    offset = record_end;
                }
                other => panic!("unexpected record type {other} at offset {offset}"),
            }
        }
        assert!(time_stamp.is_none(), "no data record follows the last TIME_STAMP record");
        records
    }

    #[fuchsia::test]
    fn page_overflow() {
        let queue = TraceEventQueue::new(0).expect("create queue");
        let queue_start_timestamp = queue.enable().expect("enable queue").into_nanos();

        let pid = 1234;
        let data = b"B|1234|loooooooooooooooooooooooooooooooooooooooooooooooooooooooooo\
        ooooooooooooooooooooooooooooooooooooooooooooooooooooooooongevent";
        let expected_event = TraceEvent::new(pid, data.len());
        assert_eq!(expected_event.size(), 155);
        let page_capacity = *PAGE_SIZE as usize - LocklessRingBuffer::PAGE_HEADER_SIZE;
        let events_per_page = page_capacity / expected_event.size();
        // Page 0 holds `events_per_page` events even with a TIME_STAMP record among them.
        assert!(
            events_per_page * expected_event.size() + LocklessRingBuffer::TIME_STAMP_RECORD_SIZE
                <= page_capacity
        );

        // Events are STEP_NANOS apart, except that event GAP_EVENT is more than
        // MAX_TIME_DELTA_NANOS after the one before it. Push one more event than Page 0 holds, so
        // that Page 0 is finalized and can be read.
        const STEP_NANOS: i64 = 1000;
        const GAP_EVENT: usize = 5;
        let gap_nanos = LocklessRingBuffer::MAX_TIME_DELTA_NANOS as i64 + 1;
        let times: Vec<i64> = (0..=events_per_page)
            .scan(queue_start_timestamp, |time, i| {
                *time += if i == GAP_EVENT { gap_nanos } else { STEP_NANOS };
                Some(*time)
            })
            .collect();
        let deltas: Vec<i64> = times
            .iter()
            .map(|&time| {
                let event = TraceEvent::new(pid, data.len());
                queue
                    .push_event_with_clock(event, data, || zx::BootInstant::from_nanos(time))
                    .expect("push event")
                    .into_nanos()
            })
            .collect();

        let mut buffer = VecOutputBuffer::new(*PAGE_SIZE as usize);
        assert_eq!(queue.read(&mut buffer), Ok(*PAGE_SIZE as usize));
        assert_eq!(buffer.bytes_written() as u64, *PAGE_SIZE);
        let page = buffer.data();
        // The page header holds the time of the first event on the page.
        let page_timestamp = u64::from_le_bytes(page[0..8].try_into().unwrap());
        assert_eq!(page_timestamp, times[0] as u64);
        // Page 0 holds the first `events_per_page` events and GAP_EVENT's TIME_STAMP record. The
        // next event doesn't fit.
        let data_size = u64::from_le_bytes(page[8..16].try_into().unwrap()) as usize;
        assert_eq!(
            data_size,
            events_per_page * expected_event.size() + LocklessRingBuffer::TIME_STAMP_RECORD_SIZE
        );

        // Each event is one ftrace record, in push order. push_event() returns the delta from the
        // previous event, and the record header holds it, except for the first event on the page,
        // whose time the page header carries, and GAP_EVENT, whose time a TIME_STAMP record
        // carries. Their header deltas are 0.
        let records = page_records(page);
        assert_eq!(records.len(), events_per_page);
        let mut expected_body = PrintEvent::new(pid).as_bytes().to_vec();
        expected_body.extend_from_slice(data);
        expected_body.push(b'\n');
        for (i, record) in records.iter().enumerate() {
            assert_eq!(record.length as usize, expected_event.size() - 4, "event {i}");
            assert_eq!(record.body, expected_body, "event {i}");
            let (expected_delta, expected_time_delta, expected_time_stamp) = match i {
                0 => (0, 0, None),
                GAP_EVENT => (gap_nanos, 0, Some(times[i] as u64)),
                _ => (STEP_NANOS, STEP_NANOS as u32, None),
            };
            assert_eq!(deltas[i], expected_delta, "event {i}");
            assert_eq!(record.time_delta, expected_time_delta, "event {i}");
            assert_eq!(record.time_stamp, expected_time_stamp, "event {i}");
        }
        // The next event is the first on Page 1.
        assert_eq!(deltas[events_per_page], 0);

        // Page 1 is not full, so it can't be read yet.
        let mut buffer = VecOutputBuffer::new(*PAGE_SIZE as usize);
        assert_eq!(queue.read(&mut buffer), error!(EAGAIN));
    }

    // Races list-level enable() against disable() and checks that the queues and
    // `tracing_enabled` agree afterwards. Without `state_mutex`, enable() can enable every queue,
    // then disable() can clear `tracing_enabled` and disable every queue, and then enable() can
    // set `tracing_enabled`, leaving it set with every queue disabled. That needs disable() to
    // run in the short window between enable()'s queue loop and its store, so this test catches
    // a regression only on some runs.
    #[fuchsia::test]
    fn concurrent_enable_disable_keeps_list_consistent() {
        let list = TraceEventQueueList::new_for_testing(4, &fuchsia_inspect::Node::default());
        for _ in 0..200 {
            let barrier = Arc::new(std::sync::Barrier::new(2));
            let enabler = {
                let list = Arc::clone(&list);
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    list.enable().unwrap();
                })
            };
            barrier.wait();
            list.disable().unwrap();
            enabler.join().unwrap();

            let enabled = list.is_enabled();
            for queue in &list.queues {
                assert_eq!(queue.ring_buffer.read().is_enabled(), enabled);
            }
        }
    }
}
