// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Defines the buffer traits needed by the TCP implementation. The traits
//! in this module provide a common interface for platform-specific buffers
//! used by TCP.

use netstack3_base::{Payload, SendPayload, SeqNum, WindowSize};

use alloc::vec;
use alloc::vec::Vec;
use core::cmp;
use core::fmt::Debug;
use core::num::NonZeroUsize;
use core::ops::Range;
use either::Either;
use packet::InnerPacketBuilder;

use crate::internal::base::BufferSizes;
use crate::internal::state::Takeable;

/// Common super trait for both sending and receiving buffer.
pub trait Buffer: Takeable + Debug + Sized {
    /// Returns the capacity range `(min, max)` for this buffer type.
    fn capacity_range() -> (usize, usize);

    /// Returns information about the number of bytes in the buffer.
    ///
    /// Returns a [`BufferLimits`] instance with information about the number of
    /// bytes in the buffer.
    fn limits(&self) -> BufferLimits;

    /// Gets the target size of the buffer, in bytes.
    ///
    /// The target capacity of the buffer is distinct from the actual capacity
    /// (returned by [`Buffer::capacity`]) in that the target capacity should
    /// remain fixed unless requested otherwise, while the actual capacity can
    /// vary with usage.
    ///
    /// For fixed-size buffers this should return the same result as calling
    /// `self.capacity()`. For buffer types that support resizing, the
    /// returned value can be different but should not change unless a resize
    /// was requested.
    fn target_capacity(&self) -> usize;

    /// Requests that the buffer be resized to hold the given number of bytes.
    ///
    /// Calling this method suggests to the buffer that it should alter its size.
    /// Implementations are free to impose constraints or ignore requests
    /// entirely.
    fn request_capacity(&mut self, size: usize);
}

/// A buffer supporting TCP receiving operations.
pub trait ReceiveBuffer: Buffer {
    /// Writes `data` into the buffer at `offset`.
    ///
    /// Returns the number of bytes written.
    fn write_at<P: Payload>(&mut self, offset: usize, data: &P) -> usize;

    /// Marks `count` bytes available for the application to read.
    ///
    /// # Panics
    ///
    /// Panics if the caller attempts to make more bytes readable than the
    /// buffer has capacity for. That is, this method panics if
    /// `self.len() + count > self.cap()`
    fn make_readable(&mut self, count: usize);
}

/// A buffer supporting TCP sending operations.
pub trait SendBuffer: Buffer {
    /// The payload type given to `peek_with`.
    type Payload<'a>: InnerPacketBuilder + Payload + Debug + 'a;

    /// Removes `count` bytes from the beginning of the buffer as already read.
    ///
    /// # Panics
    ///
    /// Panics if more bytes are marked as read than are available, i.e.,
    /// `count > self.len`.
    fn mark_read(&mut self, count: usize);

    /// Calls `f` with contiguous sequences of readable bytes in the buffer
    /// without advancing the reading pointer.
    ///
    /// # Panics
    ///
    /// Panics if more bytes are peeked than are available, i.e.,
    /// `offset > self.len`
    fn peek_with<'a, F, R>(&'a mut self, offset: usize, f: F) -> R
    where
        F: FnOnce(Self::Payload<'a>) -> R;
}

/// Information about the number of bytes in a [`Buffer`].
pub struct BufferLimits {
    /// The total number of bytes that the buffer can hold.
    pub capacity: usize,

    /// The number of readable bytes that the buffer currently holds.
    pub len: usize,
}

/// A circular buffer implementation.
///
/// A [`RingBuffer`] holds a logically contiguous ring of memory in three
/// regions:
///
/// - *readable*: memory is available for reading and not for writing,
/// - *writable*: memory that is available for writing and not for reading,
/// - *reserved*: memory that was read from and is no longer available
///   for reading or for writing.
///
/// Zero or more of these regions can be empty, and a region of memory can
/// transition from one to another in a few different ways:
///
/// *Readable* memory, once read, becomes writable unless a shrink operation is
/// in progress, in which case it becomes reserved.
///
/// *Writable* memory, once marked as such, becomes readable.
///
/// *Reserved* memory will never become readable or writable, and is non-empty
/// only while a shrink is in progress. Once the reserved segment is large
/// enough it will be removed to complete the shrinking.
#[cfg_attr(any(test, feature = "testutils"), derive(Clone, PartialEq, Eq))]
pub struct RingBuffer {
    storage: Vec<u8>,
    /// The index where the reader starts to read.
    ///
    /// Maintains the invariant that `head < storage.len()` by wrapping
    /// around to 0 as needed.
    head: usize,
    /// The amount of readable data in `storage`.
    ///
    /// Anything between [head, head+len) is readable. This will never exceed
    /// `storage.len()`.
    len: usize,
    /// The shrink operation currently in progress. If `Some`, this holds the
    /// target number of bytes to be trimmed and the current size of the
    /// reserved region.
    shrink: Option<PendingShrink>,
}

impl Debug for RingBuffer {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let Self { storage, head, len, shrink } = self;
        f.debug_struct("RingBuffer")
            .field("storage (len, cap)", &(storage.len(), storage.capacity()))
            .field("head", head)
            .field("len", len)
            .field("shrink", shrink)
            .finish()
    }
}

#[derive(Debug)]
#[cfg_attr(any(test, feature = "testutils"), derive(Copy, Clone, Eq, PartialEq))]
struct PendingShrink {
    /// The target number of reserved bytes.
    target: NonZeroUsize,
    /// The current number of bytes held in the reserved region. This will
    /// always be at most equal to `target`.
    current: usize,
}

impl Default for RingBuffer {
    fn default() -> Self {
        Self::new(WindowSize::DEFAULT.into())
    }
}

impl RingBuffer {
    /// Creates a new `RingBuffer`.
    pub fn new(capacity: usize) -> Self {
        Self { storage: vec![0; capacity], head: 0, len: 0, shrink: None }
    }

    /// Calls `f` on the contiguous sequences from `start` up to `len` bytes.
    fn with_readable<'a, F, R>(storage: &'a Vec<u8>, start: usize, len: usize, f: F) -> R
    where
        F: for<'b> FnOnce(&'b [&'a [u8]]) -> R,
    {
        // Don't read past the end of storage.
        let end = start + len;
        if end > storage.len() {
            let first_part = &storage[start..storage.len()];
            let second_part = &storage[0..len - first_part.len()];
            f(&[first_part, second_part][..])
        } else {
            let all_bytes = &storage[start..end];
            f(&[all_bytes][..])
        }
    }

    /// Shrinks `self.storage` if it is larger than the current requested
    /// capacity.
    ///
    /// Tries to shrink `self.storage` if it is larger than `self.capacity` by
    /// removing values at the end of the writable region. The number of bytes
    /// removed will be no more than the provided `max_shrink_by` value.
    ///
    /// If `self.storage.len() == self.capacity`, this is a no-op.
    fn maybe_shrink(&mut self, max_shrink_by: usize) {
        let Self { storage, head, len: _, shrink } = self;
        let PendingShrink { target, current } = match shrink {
            Some(x) => x,
            None => return,
        };
        let target = target.get();

        // Grab as many bytes as possible, up to the requested limit.
        *current = core::cmp::min(target, *current + max_shrink_by);

        if target == *current {
            // The reserved region is big enough, now finish the shrink.

            // Allocate an entirely new buffer instead of slicing in place since
            // we don't want the buffer to hold on to a bunch of extra memory
            // "just in case" for later allocations.
            let mut new_storage = Vec::new();
            new_storage.reserve_exact(storage.len() - target);

            if let Some(writable_end) = (*head).checked_sub(target) {
                // The reserved region is in the middle somewhere; slice it out.
                new_storage.extend_from_slice(&storage[..writable_end]);
                new_storage.extend_from_slice(&storage[(*head)..]);
                *head = writable_end;
            } else {
                // The reserved region wraps around the end; just copy from the
                // middle.
                let unreserved_len = storage.len() - target;
                new_storage.extend_from_slice(&storage[*head..(*head + unreserved_len)]);
                *head = 0;
            }
            *storage = new_storage;
            *shrink = None;
            return;
        }
    }

    /// Calls `f` with contiguous sequences of readable bytes in the buffer and
    /// discards the amount of bytes returned by `f`.
    ///
    /// # Panics
    ///
    /// Panics if the closure wants to discard more bytes than possible, i.e.,
    /// the value returned by `f` is greater than `self.len()`.
    pub fn read_with<F>(&mut self, f: F) -> usize
    where
        F: for<'a, 'b> FnOnce(&'b [&'a [u8]]) -> usize,
    {
        let Self { storage, head, len, shrink: _ } = self;
        if storage.len() == 0 {
            return f(&[&[]]);
        }
        let nread = RingBuffer::with_readable(storage, *head, *len, f);
        assert!(nread <= *len);
        *len -= nread;
        *head = (*head + nread) % storage.len();
        self.maybe_shrink(nread);
        nread
    }

    /// Returns the writable regions of the [`RingBuffer`].
    pub fn writable_regions(&mut self) -> impl IntoIterator<Item = &mut [u8]> {
        let BufferLimits { capacity, len } = self.limits();
        let available = capacity - len;
        let Self { storage, head, len, shrink: _ } = self;

        let mut write_start = *head + *len;
        if write_start >= storage.len() {
            write_start -= storage.len()
        }
        let write_end = write_start + available;
        if write_end <= storage.len() {
            Either::Left([&mut self.storage[write_start..write_end]].into_iter())
        } else {
            let (b1, b2) = self.storage[..].split_at_mut(write_start);
            let b2_len = b2.len();
            Either::Right([b2, &mut b1[..(available - b2_len)]].into_iter())
        }
    }

    /// Sets the target size for the [`RingBuffer`].
    ///
    /// Calling this must not cause the buffer to drop any data. If the new
    /// size can be accommodated immediately, it will be applied. Otherwise the
    /// buffer will be resized opportunistically during future operatins.
    pub fn set_target_size(&mut self, new_capacity: usize) {
        let Self { ref mut shrink, head, len: _, storage } = self;

        if let Some(extend_by) = new_capacity.checked_sub(storage.len()) {
            // This is a grow operation. Make sure to take into account any
            // shrink previously in progress.
            let old_shrink = shrink.take();
            if extend_by != 0 {
                let reserved_len = old_shrink.map_or(0, |r| r.current);
                // This is going to require resizing the storage so just
                // do that explicitly instead of trying to be clever with
                // methods on Vec.
                let mut new_storage = Vec::new();
                new_storage.reserve_exact(new_capacity);

                if *head <= reserved_len {
                    new_storage
                        .extend_from_slice(&storage[*head..][..(storage.len() - reserved_len)])
                } else {
                    new_storage.extend_from_slice(&storage[*head..]);
                    new_storage.extend_from_slice(&storage[..(*head - reserved_len)]);
                }
                new_storage.resize(new_capacity, 0);
                *storage = new_storage;
                *head = 0;
            }
        } else {
            // Start a shrink operation.

            // Unwrapping here is safe because `new_capacity <= preserve_len` is
            // not 0, or we'd be in the branch above.
            let target = NonZeroUsize::new(storage.len() - new_capacity).unwrap();
            match shrink.take() {
                None => *shrink = Some(PendingShrink { target, current: 0 }),
                Some(PendingShrink { target: _, current }) => {
                    // The old target doesn't matter, but the number of reserved
                    // bytes does. Keep that, and check to see whether it's
                    // sufficient to finish the requested shrink immediately.
                    let current = core::cmp::min(current, target.get());
                    *shrink = Some(PendingShrink { target, current });
                    self.maybe_shrink(0)
                }
            }
        }
    }
}

impl Buffer for RingBuffer {
    fn capacity_range() -> (usize, usize) {
        // Arbitrarily chosen to satisfy tests so we have some semblance of
        // clamping capacity in tests.
        (16, 16 << 20)
    }

    fn limits(&self) -> BufferLimits {
        let Self { storage, shrink, len, head: _ } = self;
        let capacity = storage.len() - shrink.as_ref().map_or(0, |r| r.current);
        BufferLimits { len: *len, capacity }
    }

    fn target_capacity(&self) -> usize {
        let Self { storage, shrink, len: _, head: _ } = self;
        storage.len() - shrink.as_ref().map_or(0, |r| r.target.get())
    }

    fn request_capacity(&mut self, size: usize) {
        self.set_target_size(size)
    }
}

impl ReceiveBuffer for RingBuffer {
    fn write_at<P: Payload>(&mut self, offset: usize, data: &P) -> usize {
        let BufferLimits { capacity, len } = self.limits();
        let available = capacity - len;
        let Self { storage, head, len, shrink: _ } = self;
        if storage.len() == 0 {
            return 0;
        }

        if offset > available {
            return 0;
        }
        let start_at = (*head + *len + offset) % storage.len();
        let to_write = cmp::min(data.len(), available);
        // Write the first part of the payload.
        let first_len = cmp::min(to_write, storage.len() - start_at);
        data.partial_copy(0, &mut storage[start_at..start_at + first_len]);
        // If we have more to write, wrap around and start from the beginning
        // of the storage.
        if to_write > first_len {
            data.partial_copy(first_len, &mut storage[0..to_write - first_len]);
        }
        to_write
    }

    fn make_readable(&mut self, count: usize) {
        let BufferLimits { capacity, len } = self.limits();
        debug_assert!(count <= capacity - len);
        self.len += count;
    }
}

impl SendBuffer for RingBuffer {
    type Payload<'a> = SendPayload<'a>;

    fn mark_read(&mut self, count: usize) {
        let Self { storage, head, len, shrink: _ } = self;
        assert!(count <= *len);
        *len -= count;
        *head = (*head + count) % storage.len();
        self.maybe_shrink(count);
    }

    fn peek_with<'a, F, R>(&'a mut self, offset: usize, f: F) -> R
    where
        F: FnOnce(SendPayload<'a>) -> R,
    {
        let Self { storage, head, len, shrink: _ } = self;
        if storage.len() == 0 {
            return f(SendPayload::Contiguous(&[]));
        }
        assert!(offset <= *len);
        RingBuffer::with_readable(
            storage,
            (*head + offset) % storage.len(),
            *len - offset,
            |readable| match readable.len() {
                1 => f(SendPayload::Contiguous(readable[0])),
                2 => f(SendPayload::Straddle(readable[0], readable[1])),
                x => unreachable!(
                    "the ring buffer cannot have more than 2 fragments, got {} fragments ({:?})",
                    x, readable
                ),
            },
        )
    }
}

/// Assembler for out-of-order segment data.
#[derive(Debug)]
#[cfg_attr(test, derive(PartialEq, Eq))]
pub(super) struct Assembler {
    // `nxt` is the next sequence number to be expected. It should be before
    // any sequnce number of the out-of-order sequence numbers we keep track
    // of below.
    nxt: SeqNum,
    // Holds all the sequence number ranges which we have already received.
    // These ranges are sorted and should have a gap of at least 1 byte
    // between any consecutive two. These ranges should only be after `nxt`.
    outstanding: Vec<Range<SeqNum>>,
}

impl Assembler {
    /// Creates a new assembler.
    pub(super) fn new(nxt: SeqNum) -> Self {
        Self { outstanding: Vec::new(), nxt }
    }

    /// Returns the next sequence number expected to be received.
    pub(super) fn nxt(&self) -> SeqNum {
        self.nxt
    }

    /// Returns whether there are out-of-order segments waiting to be
    /// acknowledged.
    pub(super) fn has_out_of_order(&self) -> bool {
        !self.outstanding.is_empty()
    }

    /// Inserts a received segment.
    ///
    /// The newly added segment will be merged with as many existing ones as
    /// possible and `nxt` will be advanced to the highest ACK number possible.
    ///
    /// Returns number of bytes that should be available for the application
    /// to consume.
    ///
    /// # Panics
    ///
    /// Panics if `start` is after `end` or if `start` is before `self.nxt`.
    pub(super) fn insert(&mut self, Range { start, end }: Range<SeqNum>) -> usize {
        assert!(!start.after(end));
        assert!(!start.before(self.nxt));
        if start == end {
            return 0;
        }
        self.insert_inner(start..end);

        let Self { outstanding, nxt } = self;
        if outstanding[0].start == *nxt {
            let advanced = outstanding.remove(0);
            *nxt = advanced.end;
            // The following unwrap is safe because it is invalid to have
            // have a range where `end` is before `start`.
            usize::try_from(advanced.end - advanced.start).unwrap()
        } else {
            0
        }
    }

    fn insert_inner(&mut self, Range { mut start, mut end }: Range<SeqNum>) {
        let Self { outstanding, nxt: _ } = self;

        if start == end {
            return;
        }

        if outstanding.is_empty() {
            outstanding.push(Range { start, end });
            return;
        }

        // Search for the first segment whose `start` is greater.
        let first_after = {
            let mut cur = 0;
            while cur < outstanding.len() {
                if start.before(outstanding[cur].start) {
                    break;
                }
                cur += 1;
            }
            cur
        };

        let mut merge_right = 0;
        for range in &outstanding[first_after..outstanding.len()] {
            if end.before(range.start) {
                break;
            }
            merge_right += 1;
            if end.before(range.end) {
                end = range.end;
                break;
            }
        }

        let mut merge_left = 0;
        for range in (&outstanding[0..first_after]).iter().rev() {
            if start.after(range.end) {
                break;
            }
            // There is no guarantee that `end.after(range.end)`, not doing
            // the following may shrink existing coverage. For example:
            // range.start = 0, range.end = 10, start = 0, end = 1, will result
            // in only 0..1 being tracked in the resulting assembler. We didn't
            // do the symmetrical thing above when merging to the right because
            // the search guarantees that `start.before(range.start)`, thus the
            // problem doesn't exist there. The asymmetry rose from the fact
            // that we used `start` to perform the search.
            if end.before(range.end) {
                end = range.end;
            }
            merge_left += 1;
            if start.after(range.start) {
                start = range.start;
                break;
            }
        }

        if merge_left == 0 && merge_right == 0 {
            // If the new segment cannot merge with any of its neighbors, we
            // add a new entry for it.
            outstanding.insert(first_after, Range { start, end });
        } else {
            // Otherwise, we put the new segment at the left edge of the merge
            // window and remove all other existing segments.
            let left_edge = first_after - merge_left;
            let right_edge = first_after + merge_right;
            outstanding[left_edge] = Range { start, end };
            for i in right_edge..outstanding.len() {
                outstanding[i - merge_left - merge_right + 1] = outstanding[i].clone();
            }
            outstanding.truncate(outstanding.len() - merge_left - merge_right + 1);
        }
    }
}

/// A conversion trait that converts the object that Bindings give us into a
/// pair of receive and send buffers.
pub trait IntoBuffers<R: ReceiveBuffer, S: SendBuffer> {
    /// Converts to receive and send buffers.
    fn into_buffers(self, buffer_sizes: BufferSizes) -> (R, S);
}

#[cfg(any(test, feature = "testutils"))]
impl<R: Default + ReceiveBuffer, S: Default + SendBuffer> IntoBuffers<R, S> for () {
    fn into_buffers(self, buffer_sizes: BufferSizes) -> (R, S) {
        // Ignore buffer sizes since this is a test-only impl.
        let BufferSizes { send: _, receive: _ } = buffer_sizes;
        Default::default()
    }
}

#[cfg(any(test, feature = "testutils"))]
pub(crate) mod testutil {
    use super::*;

    use alloc::sync::Arc;

    use netstack3_base::sync::Mutex;

    use crate::internal::socket::accept_queue::ListenerNotifier;

    impl RingBuffer {
        /// Enqueues as much of `data` as possible to the end of the buffer.
        ///
        /// Returns the number of bytes actually queued.
        pub(crate) fn enqueue_data(&mut self, data: &[u8]) -> usize {
            let nwritten = self.write_at(0, &data);
            self.make_readable(nwritten);
            nwritten
        }
    }

    impl Buffer for Arc<Mutex<RingBuffer>> {
        fn capacity_range() -> (usize, usize) {
            RingBuffer::capacity_range()
        }

        fn limits(&self) -> BufferLimits {
            self.lock().limits()
        }

        fn target_capacity(&self) -> usize {
            self.lock().target_capacity()
        }

        fn request_capacity(&mut self, size: usize) {
            self.lock().set_target_size(size)
        }
    }

    impl ReceiveBuffer for Arc<Mutex<RingBuffer>> {
        fn write_at<P: Payload>(&mut self, offset: usize, data: &P) -> usize {
            self.lock().write_at(offset, data)
        }

        fn make_readable(&mut self, count: usize) {
            self.lock().make_readable(count)
        }
    }

    /// An implementation of [`SendBuffer`] for tests.
    #[derive(Debug, Default)]
    pub struct TestSendBuffer {
        fake_stream: Arc<Mutex<Vec<u8>>>,
        ring: RingBuffer,
    }

    impl TestSendBuffer {
        /// Creates a new `TestSendBuffer` with a backing shared vec and a
        /// helper ring buffer.
        pub fn new(fake_stream: Arc<Mutex<Vec<u8>>>, ring: RingBuffer) -> TestSendBuffer {
            Self { fake_stream, ring }
        }
    }

    impl Buffer for TestSendBuffer {
        fn capacity_range() -> (usize, usize) {
            let (min, max) = RingBuffer::capacity_range();
            (min * 2, max * 2)
        }

        fn limits(&self) -> BufferLimits {
            let Self { fake_stream, ring } = self;
            let BufferLimits { capacity: ring_capacity, len: ring_len } = ring.limits();
            let guard = fake_stream.lock();
            let len = ring_len + guard.len();
            let capacity = ring_capacity + guard.capacity();
            BufferLimits { len, capacity }
        }

        fn target_capacity(&self) -> usize {
            let Self { fake_stream: _, ring } = self;
            ring.target_capacity()
        }

        fn request_capacity(&mut self, size: usize) {
            let Self { fake_stream: _, ring } = self;
            ring.set_target_size(size)
        }
    }

    impl SendBuffer for TestSendBuffer {
        type Payload<'a> = SendPayload<'a>;

        fn mark_read(&mut self, count: usize) {
            let Self { fake_stream: _, ring } = self;
            ring.mark_read(count)
        }

        fn peek_with<'a, F, R>(&'a mut self, offset: usize, f: F) -> R
        where
            F: FnOnce(SendPayload<'a>) -> R,
        {
            let Self { fake_stream, ring } = self;
            let mut guard = fake_stream.lock();
            if !guard.is_empty() {
                // Pull from the fake stream into the ring if there is capacity.
                let BufferLimits { capacity, len } = ring.limits();
                let len = (capacity - len).min(guard.len());
                let rest = guard.split_off(len);
                let first = core::mem::replace(&mut *guard, rest);
                assert_eq!(ring.enqueue_data(&first[..]), len);
            }
            ring.peek_with(offset, f)
        }
    }

    fn arc_mutex_eq<T: PartialEq>(a: &Arc<Mutex<T>>, b: &Arc<Mutex<T>>) -> bool {
        if Arc::ptr_eq(a, b) {
            return true;
        }
        (&*a.lock()) == (&*b.lock())
    }

    /// A fake implementation of client-side TCP buffers.
    #[derive(Clone, Debug, Default)]
    pub struct ClientBuffers {
        /// Receive buffer shared with core TCP implementation.
        pub receive: Arc<Mutex<RingBuffer>>,
        /// Send buffer shared with core TCP implementation.
        pub send: Arc<Mutex<Vec<u8>>>,
    }

    impl PartialEq for ClientBuffers {
        fn eq(&self, ClientBuffers { receive: other_receive, send: other_send }: &Self) -> bool {
            let Self { receive, send } = self;
            arc_mutex_eq(receive, other_receive) && arc_mutex_eq(send, other_send)
        }
    }

    impl Eq for ClientBuffers {}

    impl ClientBuffers {
        /// Creates new a `ClientBuffers` with `buffer_sizes`.
        pub fn new(buffer_sizes: BufferSizes) -> Self {
            let BufferSizes { send, receive } = buffer_sizes;
            Self {
                receive: Arc::new(Mutex::new(RingBuffer::new(receive))),
                send: Arc::new(Mutex::new(Vec::with_capacity(send))),
            }
        }
    }

    /// A fake implementation of bindings buffers for TCP.
    #[derive(Debug, Clone, Eq, PartialEq)]
    #[allow(missing_docs)]
    pub enum ProvidedBuffers {
        Buffers(WriteBackClientBuffers),
        NoBuffers,
    }

    impl Default for ProvidedBuffers {
        fn default() -> Self {
            Self::NoBuffers
        }
    }

    impl From<WriteBackClientBuffers> for ProvidedBuffers {
        fn from(buffers: WriteBackClientBuffers) -> Self {
            ProvidedBuffers::Buffers(buffers)
        }
    }

    impl From<ProvidedBuffers> for WriteBackClientBuffers {
        fn from(extra: ProvidedBuffers) -> Self {
            match extra {
                ProvidedBuffers::Buffers(buffers) => buffers,
                ProvidedBuffers::NoBuffers => Default::default(),
            }
        }
    }

    impl From<ProvidedBuffers> for () {
        fn from(_: ProvidedBuffers) -> Self {
            ()
        }
    }

    impl From<()> for ProvidedBuffers {
        fn from(_: ()) -> Self {
            Default::default()
        }
    }

    /// The variant of [`ProvidedBuffers`] that provides observing the data
    /// sent/received to TCP sockets.
    #[derive(Debug, Default, Clone)]
    pub struct WriteBackClientBuffers(pub Arc<Mutex<Option<ClientBuffers>>>);

    impl PartialEq for WriteBackClientBuffers {
        fn eq(&self, Self(other): &Self) -> bool {
            let Self(this) = self;
            arc_mutex_eq(this, other)
        }
    }

    impl Eq for WriteBackClientBuffers {}

    impl IntoBuffers<Arc<Mutex<RingBuffer>>, TestSendBuffer> for ProvidedBuffers {
        fn into_buffers(
            self,
            buffer_sizes: BufferSizes,
        ) -> (Arc<Mutex<RingBuffer>>, TestSendBuffer) {
            let buffers = ClientBuffers::new(buffer_sizes);
            if let ProvidedBuffers::Buffers(b) = self {
                *b.0.as_ref().lock() = Some(buffers.clone());
            }
            let ClientBuffers { receive, send } = buffers;
            (receive, TestSendBuffer::new(send, Default::default()))
        }
    }

    impl ListenerNotifier for ProvidedBuffers {
        fn new_incoming_connections(&mut self, _: usize) {}
    }
}

#[cfg(test)]
mod test {
    use assert_matches::assert_matches;
    use packet::{
        Buf, FragmentedBytesMut, InnerPacketBuilder as _, PacketBuilder, PacketConstraints,
        SerializeError, SerializeTarget, Serializer,
    };
    use proptest::proptest;
    use proptest::strategy::{Just, Strategy};
    use proptest::test_runner::Config;
    use proptest_support::failed_seeds;
    use test_case::test_case;

    use super::*;
    use netstack3_base::{PayloadLen, WindowSize};

    const TEST_BYTES: &'static [u8] = "Hello World!".as_bytes();

    proptest! {
        #![proptest_config(Config {
            // Add all failed seeds here.
            failure_persistence: failed_seeds!(
                "cc f621ca7d3a2b108e0dc41f7169ad028f4329b79e90e73d5f68042519a9f63999",
                "cc c449aebed201b4ec4f137f3c224f20325f4cfee0b7fd596d9285176b6d811aa9"
            ),
            ..Config::default()
        })]

        #[test]
        fn assembler_insertion(insertions in proptest::collection::vec(assembler::insertions(), 200)) {
            let mut assembler = Assembler::new(SeqNum::new(0));
            let mut num_insertions_performed = 0;
            let mut min_seq = SeqNum::new(WindowSize::MAX.into());
            let mut max_seq = SeqNum::new(0);
            for Range { start, end } in insertions {
                if min_seq.after(start) {
                    min_seq = start;
                }
                if max_seq.before(end) {
                    max_seq = end;
                }
                // assert that it's impossible to have more entries than the
                // number of insertions performed.
                assert!(assembler.outstanding.len() <= num_insertions_performed);
                assembler.insert_inner(start..end);
                num_insertions_performed += 1;

                // assert that the ranges are sorted and don't overlap with
                // each other.
                for i in 1..assembler.outstanding.len() {
                    assert!(assembler.outstanding[i-1].end.before(assembler.outstanding[i].start));
                }
            }
            assert_eq!(assembler.outstanding.first().unwrap().start, min_seq);
            assert_eq!(assembler.outstanding.last().unwrap().end, max_seq);
        }

        #[test]
        fn ring_buffer_make_readable((mut rb, avail) in ring_buffer::with_written()) {
            let old_storage = rb.storage.clone();
            let old_head = rb.head;
            let old_len = rb.limits().len;
            let old_shrink = rb.shrink;
            rb.make_readable(avail);
            // Assert that length is updated but everything else is unchanged.
            let RingBuffer { storage, head, len, shrink } = rb;
            assert_eq!(len, old_len + avail);
            assert_eq!(head, old_head);
            assert_eq!(storage, old_storage);
            assert_eq!(shrink, old_shrink);
        }

        #[test]
        fn ring_buffer_write_at((mut rb, offset, data) in ring_buffer::with_offset_data()) {
            let old_head = rb.head;
            let old_len = rb.limits().len;
            assert_eq!(rb.write_at(offset, &&data[..]), data.len());
            assert_eq!(rb.head, old_head);
            assert_eq!(rb.limits().len, old_len);
            for i in 0..data.len() {
                let masked = (rb.head + rb.len + offset + i) % rb.storage.len();
                // Make sure that data are written.
                assert_eq!(rb.storage[masked], data[i]);
                rb.storage[masked] = 0;
            }
            // And the other parts of the storage are untouched.
            assert_eq!(rb.storage, vec![0; rb.storage.len()])
        }

        #[test]
        fn ring_buffer_read_with((mut rb, expected, consume) in ring_buffer::with_read_data()) {
            assert_eq!(rb.limits().len, expected.len());
            let nread = rb.read_with(|readable| {
                assert!(readable.len() == 1 || readable.len() == 2);
                let got = readable.concat();
                assert_eq!(got, expected);
                consume
            });
            assert_eq!(nread, consume);
            assert_eq!(rb.limits().len, expected.len() - consume);
        }

        #[test]
        fn ring_buffer_mark_read((mut rb, readable) in ring_buffer::with_readable()) {
            const BYTE_TO_WRITE: u8 = 0x42;
            let written = rb.writable_regions().into_iter().fold(0, |acc, slice| {
                slice.fill(BYTE_TO_WRITE);
                acc + slice.len()
            });
            let old_storage = rb.storage.clone();
            let old_head = rb.head;
            let old_len = rb.limits().len;
            let old_shrink = rb.shrink;

            rb.mark_read(readable);
            // Depending on whether a shrink was performed, a bunch of things
            // might have changed. Either way, the length should always be
            // reduced, and the written bytes should be preserved.
            let new_writable = rb.writable_regions().into_iter().fold(Vec::new(), |mut acc, slice| {
                acc.extend_from_slice(slice);
                acc
            });
            for (i, x) in new_writable.iter().enumerate().take(written) {
                assert_eq!(*x, BYTE_TO_WRITE, "i={}, rb={:?}", i, rb);
            }
            assert!(new_writable.len() >= written);

            let RingBuffer { storage, head, len, shrink } = rb;
            assert_eq!(len, old_len - readable);
            let shrank = old_shrink.is_some() && shrink.is_none();
            if !shrank {
                assert_eq!(head, (old_head + readable) % old_storage.len());
                assert_eq!(storage, old_storage);
            }

        }

        #[test]
        fn ring_buffer_peek_with((mut rb, expected, offset) in ring_buffer::with_read_data()) {
            assert_eq!(rb.limits().len, expected.len());
            let () = rb.peek_with(offset, |readable| {
                assert_eq!(readable.to_vec(), &expected[offset..]);
            });
            assert_eq!(rb.limits().len, expected.len());
        }

        #[test]
        fn ring_buffer_writable_regions(mut rb in ring_buffer::arb_ring_buffer()) {
            const BYTE_TO_WRITE: u8 = 0x42;
            let writable_len = rb.writable_regions().into_iter().fold(0, |acc, slice| {
                slice.fill(BYTE_TO_WRITE);
                acc + slice.len()
            });
            let BufferLimits {len, capacity} = rb.limits();
            assert_eq!(writable_len + len, capacity);
            for i in 0..capacity {
                let expected = if i < len {
                    0
                } else {
                    BYTE_TO_WRITE
                };
                let idx = (rb.head + i) % rb.storage.len();
                assert_eq!(rb.storage[idx], expected);
            }
        }

        #[test]
        fn send_payload_len((payload, _idx) in send_payload::with_index()) {
            assert_eq!(payload.len(), TEST_BYTES.len())
        }

        #[test]
        fn send_payload_slice((payload, idx) in send_payload::with_index()) {
            let idx_u32 = u32::try_from(idx).unwrap();
            let end = u32::try_from(TEST_BYTES.len()).unwrap();
            assert_eq!(payload.clone().slice(0..idx_u32).to_vec(), &TEST_BYTES[..idx]);
            assert_eq!(payload.clone().slice(idx_u32..end).to_vec(), &TEST_BYTES[idx..]);
        }

        #[test]
        fn send_payload_partial_copy((payload, offset, len) in send_payload::with_offset_and_length()) {
            let mut buffer = [0; TEST_BYTES.len()];
            payload.partial_copy(offset, &mut buffer[0..len]);
            assert_eq!(&buffer[0..len], &TEST_BYTES[offset..offset + len]);
        }

        #[test]
        fn set_target_size((mut rb, new_cap) in ring_buffer::with_new_target_size()) {
            const BYTE_TO_WRITE: u8 = 0x42;
            let written = rb.writable_regions().into_iter().fold(0, |acc, slice| {
                slice.fill(BYTE_TO_WRITE);
                acc + slice.len()
            });

            let old_len = rb.limits().len;
            rb.set_target_size(new_cap);

            assert_eq!(rb.limits().len, old_len);
            let new_writable = rb.writable_regions().into_iter().fold(Vec::new(), |mut acc, slice| {
                acc.extend_from_slice(slice);
                acc
            });
            let BufferLimits {len, capacity} = rb.limits();
            assert_eq!(new_writable.len() + len, capacity);
            assert!(new_writable.len() >= written);
            for (i, x) in new_writable.iter().enumerate() {
                let expected = (i < written).then_some(BYTE_TO_WRITE).unwrap_or(0);
                assert_eq!(*x, expected, "i={}, rb={:?}", i, rb);
            }
        }
    }

    #[derive(Debug)]
    struct OuterBuilder(PacketConstraints);

    impl OuterBuilder {
        const HEADER_BYTE: u8 = b'H';
        const FOOTER_BYTE: u8 = b'H';
    }

    impl PacketBuilder for OuterBuilder {
        fn constraints(&self) -> PacketConstraints {
            let Self(constraints) = self;
            constraints.clone()
        }

        fn serialize(&self, target: &mut SerializeTarget<'_>, _body: FragmentedBytesMut<'_, '_>) {
            target.header.fill(Self::HEADER_BYTE);
            target.footer.fill(Self::FOOTER_BYTE);
        }
    }

    const EXAMPLE_DATA: [u8; 10] = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9];
    #[test_case(SendPayload::Contiguous(&EXAMPLE_DATA); "contiguous")]
    #[test_case(SendPayload::Straddle(&EXAMPLE_DATA[0..5], &EXAMPLE_DATA[5..]); "split")]
    #[test_case(SendPayload::Straddle(&[], &EXAMPLE_DATA); "split empty front")]
    #[test_case(SendPayload::Straddle(&EXAMPLE_DATA, &[]); "split empty back")]
    fn send_payload_serializer_data(payload: SendPayload<'static>) {
        const HEADER_LEN: usize = 5;
        const FOOTER_LEN: usize = 6;
        let outer = OuterBuilder(PacketConstraints::new(HEADER_LEN, FOOTER_LEN, 0, usize::MAX));

        assert_eq!(
            payload
                .into_serializer()
                .encapsulate(outer)
                .serialize_vec_outer()
                .expect("should serialize")
                .unwrap_b(),
            Buf::new(
                [OuterBuilder::HEADER_BYTE; HEADER_LEN]
                    .into_iter()
                    .chain(EXAMPLE_DATA)
                    .chain([OuterBuilder::FOOTER_BYTE; FOOTER_LEN])
                    .collect(),
                ..
            )
        );
    }

    #[test]
    fn send_payload_serializer_body_too_large() {
        let outer = OuterBuilder(PacketConstraints::new(0, 0, 0, EXAMPLE_DATA.len() - 1));
        let payload = SendPayload::Contiguous(&EXAMPLE_DATA);

        assert_matches!(
            payload.into_serializer().encapsulate(outer).serialize_vec_outer(),
            Err((SerializeError::SizeLimitExceeded, _))
        );
    }

    #[test]
    fn send_payload_serializer_body_needs_padding() {
        const PADDING: usize = 3;
        let outer =
            OuterBuilder(PacketConstraints::new(0, 0, EXAMPLE_DATA.len() + PADDING, usize::MAX));
        let payload = SendPayload::Contiguous(&EXAMPLE_DATA);

        // The body gets padded with zeroes.
        assert_eq!(
            payload
                .into_serializer()
                .encapsulate(outer)
                .serialize_vec_outer()
                .expect("can serialize")
                .unwrap_b(),
            Buf::new(EXAMPLE_DATA.into_iter().chain([0; PADDING]).collect(), ..)
        );
    }

    #[test_case([Range { start: 0, end: 0 }]
        => Assembler { outstanding: vec![], nxt: SeqNum::new(0) })]
    #[test_case([Range { start: 0, end: 10 }]
        => Assembler { outstanding: vec![], nxt: SeqNum::new(10) })]
    #[test_case([Range{ start: 10, end: 15 }, Range { start: 5, end: 10 }]
        => Assembler { outstanding: vec![Range { start: SeqNum::new(5), end: SeqNum::new(15) }], nxt: SeqNum::new(0)})]
    #[test_case([Range{ start: 10, end: 15 }, Range { start: 0, end: 5 }, Range { start: 5, end: 10 }]
        => Assembler { outstanding: vec![], nxt: SeqNum::new(15) })]
    #[test_case([Range{ start: 10, end: 15 }, Range { start: 5, end: 10 }, Range { start: 0, end: 5 }]
        => Assembler { outstanding: vec![], nxt: SeqNum::new(15) })]
    fn assembler_examples(ops: impl IntoIterator<Item = Range<u32>>) -> Assembler {
        let mut assembler = Assembler::new(SeqNum::new(0));
        for Range { start, end } in ops.into_iter() {
            let _advanced = assembler.insert(SeqNum::new(start)..SeqNum::new(end));
        }
        assembler
    }

    #[test]
    // Regression test for https://fxbug.dev/42061342.
    fn ring_buffer_wrap_around() {
        const CAPACITY: usize = 16;
        let mut rb = RingBuffer::new(CAPACITY);

        // Write more than half the buffer.
        const BUF_SIZE: usize = 10;
        assert_eq!(rb.enqueue_data(&[0xAA; BUF_SIZE]), BUF_SIZE);
        rb.peek_with(0, |payload| assert_eq!(payload, SendPayload::Contiguous(&[0xAA; BUF_SIZE])));
        rb.mark_read(BUF_SIZE);

        // Write around the end of the buffer.
        assert_eq!(rb.enqueue_data(&[0xBB; BUF_SIZE]), BUF_SIZE);
        rb.peek_with(0, |payload| {
            assert_eq!(
                payload,
                SendPayload::Straddle(
                    &[0xBB; (CAPACITY - BUF_SIZE)],
                    &[0xBB; (BUF_SIZE * 2 - CAPACITY)]
                )
            )
        });
        // Mark everything read, which should advance `head` around to the
        // beginning of the buffer.
        rb.mark_read(BUF_SIZE);

        // Now make a contiguous sequence of bytes readable.
        assert_eq!(rb.enqueue_data(&[0xCC; BUF_SIZE]), BUF_SIZE);
        rb.peek_with(0, |payload| assert_eq!(payload, SendPayload::Contiguous(&[0xCC; BUF_SIZE])));

        // Check that the unwritten bytes are left untouched. If `head` was
        // advanced improperly, this will crash.
        let read = rb.read_with(|segments| {
            assert_eq!(segments, [[0xCC; BUF_SIZE]]);
            BUF_SIZE
        });
        assert_eq!(read, BUF_SIZE);
    }

    #[test]
    fn ring_buffer_example() {
        let mut rb = RingBuffer::new(16);
        assert_eq!(rb.write_at(5, &"World".as_bytes()), 5);
        assert_eq!(rb.write_at(0, &"Hello".as_bytes()), 5);
        rb.make_readable(10);
        assert_eq!(
            rb.read_with(|readable| {
                assert_eq!(readable, &["HelloWorld".as_bytes()]);
                5
            }),
            5
        );
        assert_eq!(
            rb.read_with(|readable| {
                assert_eq!(readable, &["World".as_bytes()]);
                readable[0].len()
            }),
            5
        );
        assert_eq!(rb.write_at(0, &"HelloWorld".as_bytes()), 10);
        rb.make_readable(10);
        assert_eq!(
            rb.read_with(|readable| {
                assert_eq!(readable, &["HelloW".as_bytes(), "orld".as_bytes()]);
                6
            }),
            6
        );
        assert_eq!(rb.limits().len, 4);
        assert_eq!(
            rb.read_with(|readable| {
                assert_eq!(readable, &["orld".as_bytes()]);
                4
            }),
            4
        );
        assert_eq!(rb.limits().len, 0);

        assert_eq!(rb.enqueue_data("Hello".as_bytes()), 5);
        assert_eq!(rb.limits().len, 5);

        let () = rb.peek_with(3, |readable| {
            assert_eq!(readable.to_vec(), "lo".as_bytes());
        });

        rb.mark_read(2);

        let () = rb.peek_with(0, |readable| {
            assert_eq!(readable.to_vec(), "llo".as_bytes());
        });
    }

    mod assembler {
        use super::*;
        pub(super) fn insertions() -> impl Strategy<Value = Range<SeqNum>> {
            (0..u32::from(WindowSize::MAX)).prop_flat_map(|start| {
                (start + 1..=u32::from(WindowSize::MAX)).prop_flat_map(move |end| {
                    Just(Range { start: SeqNum::new(start), end: SeqNum::new(end) })
                })
            })
        }
    }

    mod ring_buffer {
        use super::*;
        // Use a small capacity so that we have a higher chance to exercise
        // wrapping around logic.
        const MAX_CAP: usize = 32;

        fn arb_ring_buffer_args(
        ) -> impl Strategy<Value = (usize, usize, usize, Option<PendingShrink>)> {
            fn arb_shrink_args(cap: usize) -> impl Strategy<Value = Option<PendingShrink>> {
                (0..=cap).prop_flat_map(|target| match NonZeroUsize::new(target) {
                    Some(target) => (Just(target), (0..=target.get()))
                        .prop_map(|(target, current)| Some(PendingShrink { target, current }))
                        .boxed(),
                    None => Just(None).boxed(),
                })
            }

            // Use a small capacity so that we have a higher chance to exercise
            // wrapping around logic.
            (1..=MAX_CAP).prop_flat_map(|cap| {
                arb_shrink_args(cap).prop_flat_map(move |shrink| {
                    let max_len = cap - shrink.as_ref().map_or(0, |r| r.current);
                    //  cap      head     len
                    (Just(cap), 0..cap, 0..=max_len, Just(shrink))
                })
            })
        }

        pub(super) fn arb_ring_buffer() -> impl Strategy<Value = RingBuffer> {
            arb_ring_buffer_args().prop_map(|(cap, head, len, shrink)| RingBuffer {
                storage: vec![0; cap],
                head,
                len,
                shrink,
            })
        }

        /// A strategy for a [`RingBuffer`] and a valid length to mark read.
        pub(super) fn with_readable() -> impl Strategy<Value = (RingBuffer, usize)> {
            arb_ring_buffer_args().prop_flat_map(|(cap, head, len, shrink)| {
                (Just(RingBuffer { storage: vec![0; cap], head, len, shrink }), 0..=len)
            })
        }

        /// A strategy for a [`RingBuffer`] and a valid length to make readable.
        pub(super) fn with_written() -> impl Strategy<Value = (RingBuffer, usize)> {
            arb_ring_buffer_args().prop_flat_map(|(cap, head, len, shrink)| {
                let rb = RingBuffer { storage: vec![0; cap], head, len, shrink };
                let max_written = cap - len - shrink.map_or(0, |r| r.current);
                (Just(rb), 0..=max_written)
            })
        }

        /// A strategy for a [`RingBuffer`], a valid offset and data to write.
        pub(super) fn with_offset_data() -> impl Strategy<Value = (RingBuffer, usize, Vec<u8>)> {
            arb_ring_buffer_args().prop_flat_map(|(cap, head, len, shrink)| {
                let writable_len = cap - len - shrink.map_or(0, |r| r.current);
                (0..=writable_len).prop_flat_map(move |offset| {
                    (0..=writable_len - offset).prop_flat_map(move |data_len| {
                        (
                            Just(RingBuffer { storage: vec![0; cap], head, len, shrink }),
                            Just(offset),
                            proptest::collection::vec(1..=u8::MAX, data_len),
                        )
                    })
                })
            })
        }

        /// A strategy for a [`RingBuffer`], its readable data, and how many
        /// bytes to consume.
        pub(super) fn with_read_data() -> impl Strategy<Value = (RingBuffer, Vec<u8>, usize)> {
            arb_ring_buffer_args().prop_flat_map(|(cap, head, len, shrink)| {
                proptest::collection::vec(1..=u8::MAX, len).prop_flat_map(move |data| {
                    // Fill the RingBuffer with the data.
                    let mut rb = RingBuffer { storage: vec![0; cap], head, len: 0, shrink };
                    assert_eq!(rb.write_at(0, &&data[..]), len);
                    rb.make_readable(len);
                    (Just(rb), Just(data), 0..=len)
                })
            })
        }

        pub(super) fn with_new_target_size() -> impl Strategy<Value = (RingBuffer, usize)> {
            arb_ring_buffer_args().prop_flat_map(|(cap, head, len, shrink)| {
                (0..MAX_CAP * 2).prop_map(move |target_size| {
                    let rb = RingBuffer { storage: vec![0; cap], head, len, shrink };
                    (rb, target_size)
                })
            })
        }
    }
    mod send_payload {
        use super::*;

        pub(super) fn with_index() -> impl Strategy<Value = (SendPayload<'static>, usize)> {
            proptest::prop_oneof![
                (Just(SendPayload::Contiguous(TEST_BYTES)), 0..TEST_BYTES.len()),
                (0..TEST_BYTES.len()).prop_flat_map(|split_at| {
                    (
                        Just(SendPayload::Straddle(
                            &TEST_BYTES[..split_at],
                            &TEST_BYTES[split_at..],
                        )),
                        0..TEST_BYTES.len(),
                    )
                })
            ]
        }

        pub(super) fn with_offset_and_length(
        ) -> impl Strategy<Value = (SendPayload<'static>, usize, usize)> {
            with_index().prop_flat_map(|(payload, index)| {
                (Just(payload), Just(index), 0..=TEST_BYTES.len() - index)
            })
        }
    }
}
