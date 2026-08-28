// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Constructs that support Generic Receive Offload (GRO) at the device layer.

use alloc::vec::Vec;
use core::marker::PhantomData;

use assert_matches::assert_matches;
use derivative::Derivative;

use netstack3_base::ChecksumRxOffloading;

/// A slice view of a buffer, which is either a contiguous slice or linearized
/// into scratch storage.
#[derive(Debug)]
pub enum BufferSlice<'a, 'b> {
    /// A slice view directly into a contiguous buffer.
    Contiguous(&'a mut [u8]),
    /// A slice view into scratch storage after linearizing a non-contiguous
    /// buffer.
    Linearized(&'b mut [u8]),
}

impl BufferSlice<'_, '_> {
    /// Returns an immutable slice view of the buffer.
    pub fn as_slice(&self) -> &[u8] {
        match self {
            Self::Contiguous(s) => s,
            Self::Linearized(s) => s,
        }
    }

    /// Returns a mutable slice view of the buffer.
    pub fn as_slice_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Contiguous(s) => s,
            Self::Linearized(s) => s,
        }
    }
}

/// A buffer that may be backed by a contiguous memory slice.
pub trait MaybeContiguousBuffer {
    /// Obtains a slice view into the buffer, linearizing into `storage` if
    /// necessary.
    fn linearized<'a, 'b>(&'a mut self, storage: &'b mut Vec<u8>) -> BufferSlice<'a, 'b>;
}

/// An input buffer item for GRO processing.
#[derive(Debug, PartialEq, Eq)]
pub struct GroInputItem<B, T> {
    /// The buffer.
    pub buffer: B,
    /// Target for the incoming frame.
    pub target: T,
    /// Checksum offload state for the incoming frame.
    pub checksum_offload: ChecksumRxOffloading,
}

/// Buffers associated with a GRO output item.
#[derive(Debug)]
pub enum GroOutputBuffers<'a, B, O> {
    /// A single contiguous buffer.
    Contiguous(B),
    /// A single buffer that was linearized into temporary scratch space.
    Linearized {
        /// The linearized slice view into scratch storage.
        slice: &'a mut [u8],
        /// The original buffer.
        buffer: B,
    },
    /// A coalesced set of buffers.
    Coalesced {
        /// The coalesced slice view into coalescing storage.
        slice: &'a mut [u8],
        /// The original buffers that formed this frame.
        buffers: O,
    },
}

impl<'a, B: MaybeContiguousBuffer, O> GroOutputBuffers<'a, B, O> {
    /// Returns a mutable slice view of the frame buffer.
    pub fn slice_mut(&mut self) -> &mut [u8] {
        match self {
            Self::Contiguous(b) => {
                // Note: it's safe to assert here because `Self::Contiguous` is
                // always constructed from a `BufferSlice::Contiguous`.
                assert_matches!(
                    b.linearized(&mut Vec::new()),
                    BufferSlice::Contiguous(slice) => slice
                )
            }
            Self::Linearized { slice, .. } | Self::Coalesced { slice, .. } => slice,
        }
    }
}

/// An item yielded by GRO processing.
#[derive(Debug)]
pub struct GroOutputItem<'a, B, T, O> {
    /// Target for the frame.
    pub target: T,
    /// Checksum offload state for the frame.
    pub checksum_offload: ChecksumRxOffloading,
    /// The buffer(s) associated with this frame.
    pub buffers: GroOutputBuffers<'a, B, O>,
}

/// Persistent reusable buffer storage for GRO to save on per-batch allocations.
#[derive(Debug, Derivative)]
#[derivative(Default(bound = ""))]
pub struct GroBufferStorage<B> {
    /// Buffer for GRO coalescing.
    coalescing_vec: Vec<u8>,
    /// Holds onto the original buffers while building a coalesced frame before
    /// it's passed to the stack.
    coalesced_buffers: Vec<B>,
    /// Buffer for linearization of fragmented buffers.
    linearization_vec: Vec<u8>,
}

impl<B> GroBufferStorage<B> {
    /// Creates a new `GroBufferStorage`.
    pub fn new() -> Self {
        Self::default()
    }

    /// Adapts the provided iterator of packet buffers into a GRO iterator.
    pub fn coalesce<I, T>(&mut self, iter: I) -> GroIter<'_, I, B, T> {
        GroIter::new(iter, self)
    }

    fn clear(&mut self) {
        self.coalescing_vec.clear();
        self.linearization_vec.clear();
        self.coalesced_buffers.clear();
    }
}

/// An iterator adapter for GRO processing.
pub struct GroIter<'a, I, B, T> {
    iter: I,
    storage: &'a mut GroBufferStorage<B>,
    _marker: PhantomData<T>,
}

impl<'a, I, B, T> GroIter<'a, I, B, T> {
    fn new(iter: I, storage: &'a mut GroBufferStorage<B>) -> Self {
        Self { iter, storage, _marker: PhantomData }
    }
}

impl<'a, I, B, T> Drop for GroIter<'a, I, B, T> {
    fn drop(&mut self) {
        self.storage.clear();
    }
}

enum ProcessingResult<'a, B, T, O> {
    // TODO(https://fxbug.dev/452980285): Remove once used.
    #[expect(dead_code)]
    Continue,
    Return(GroOutputItem<'a, B, T, O>),
}

impl<'a, I, B, T, E> GroIter<'a, I, B, T>
where
    B: MaybeContiguousBuffer,
    I: Iterator<Item = Result<GroInputItem<B, T>, E>>,
{
    /// Advances the iterator and returns the next GRO output item.
    pub fn next<'b>(
        &'b mut self,
    ) -> Option<Result<GroOutputItem<'b, B, T, alloc::vec::Drain<'b, B>>, E>> {
        loop {
            let item = match self.iter.next()? {
                Ok(item) => item,
                Err(e) => return Some(Err(e)),
            };

            match Self::process_input_item(self.storage, item) {
                ProcessingResult::Continue => {}
                ProcessingResult::Return(out) => return Some(Ok(out)),
            }
        }
    }

    /// Processes a GRO input item and returns the action to be taken as a
    /// result of the processing.
    fn process_input_item<'b>(
        storage: &'b mut GroBufferStorage<B>,
        item: GroInputItem<B, T>,
    ) -> ProcessingResult<'b, B, T, alloc::vec::Drain<'b, B>> {
        let GroInputItem { mut buffer, target, checksum_offload } = item;

        // TODO(https://fxbug.dev/452980285): Create a setting to control whether
        // GRO is enabled and implement TCP coalescing.
        let buffer_slice = buffer.linearized(&mut storage.linearization_vec);
        let buffers = match buffer_slice {
            BufferSlice::Contiguous(_) => GroOutputBuffers::Contiguous(buffer),
            BufferSlice::Linearized(slice) => GroOutputBuffers::Linearized { buffer, slice },
        };
        ProcessingResult::Return(GroOutputItem { target, checksum_offload, buffers })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    #[derive(Debug, PartialEq, Eq)]
    struct TestBuffer {
        buf: Vec<u8>,
        contiguous: bool,
    }

    impl MaybeContiguousBuffer for TestBuffer {
        fn linearized<'a, 'b>(&'a mut self, storage: &'b mut Vec<u8>) -> BufferSlice<'a, 'b> {
            if self.contiguous {
                BufferSlice::Contiguous(&mut self.buf[..])
            } else {
                let frame_length = self.buf.len();
                if storage.len() < frame_length {
                    storage.resize(frame_length, 0);
                }
                let slice = &mut storage[..frame_length];
                slice.copy_from_slice(&self.buf);
                BufferSlice::Linearized(slice)
            }
        }
    }

    #[test]
    fn process_gro_handles_fragmented() {
        let items: Vec<Result<GroInputItem<TestBuffer, ()>, ()>> = vec![
            Ok(GroInputItem {
                buffer: TestBuffer { buf: vec![1, 2, 3], contiguous: true },
                target: (),
                checksum_offload: ChecksumRxOffloading::FullyOffloaded,
            }),
            Ok(GroInputItem {
                buffer: TestBuffer { buf: vec![4, 5, 6], contiguous: false },
                target: (),
                checksum_offload: ChecksumRxOffloading::FullyOffloaded,
            }),
        ];

        let mut storage = GroBufferStorage::new();
        let mut output = Vec::new();
        let mut gro = storage.coalesce(items.into_iter());
        while let Some(item) = gro.next() {
            let mut item = item.unwrap();
            output.push(item.buffers.slice_mut().to_vec());
        }

        assert_eq!(output, vec![vec![1, 2, 3], vec![4, 5, 6]]);
    }

    #[derive(Debug)]
    struct TrackedBuffer {
        buf: Vec<u8>,
        contiguous: bool,
        dropped: alloc::sync::Arc<core::sync::atomic::AtomicBool>,
    }

    impl Drop for TrackedBuffer {
        fn drop(&mut self) {
            self.dropped.store(true, core::sync::atomic::Ordering::SeqCst);
        }
    }

    impl MaybeContiguousBuffer for TrackedBuffer {
        fn linearized<'a, 'b>(&'a mut self, storage: &'b mut Vec<u8>) -> BufferSlice<'a, 'b> {
            if self.contiguous {
                BufferSlice::Contiguous(&mut self.buf[..])
            } else {
                let frame_length = self.buf.len();
                if storage.len() < frame_length {
                    storage.resize(frame_length, 0);
                }
                let slice = &mut storage[..frame_length];
                slice.copy_from_slice(&self.buf);
                BufferSlice::Linearized(slice)
            }
        }
    }

    #[test]
    fn test_gro_buffers_dropped_when_item_dropped() {
        use alloc::sync::Arc;
        use core::sync::atomic::{AtomicBool, Ordering};

        let dropped1 = Arc::new(AtomicBool::new(false));
        let dropped2 = Arc::new(AtomicBool::new(false));

        let items: Vec<Result<GroInputItem<TrackedBuffer, ()>, ()>> = vec![
            Ok(GroInputItem {
                buffer: TrackedBuffer {
                    buf: vec![1, 2, 3],
                    contiguous: true,
                    dropped: dropped1.clone(),
                },
                target: (),
                checksum_offload: ChecksumRxOffloading::FullyOffloaded,
            }),
            Ok(GroInputItem {
                buffer: TrackedBuffer {
                    buf: vec![4, 5, 6],
                    contiguous: false,
                    dropped: dropped2.clone(),
                },
                target: (),
                checksum_offload: ChecksumRxOffloading::FullyOffloaded,
            }),
        ];

        let mut storage = GroBufferStorage::new();
        let mut gro = storage.coalesce(items.into_iter());

        let mut item1 = gro.next().unwrap().unwrap();
        assert_eq!(item1.buffers.slice_mut(), &[1, 2, 3]);
        assert!(!dropped1.load(Ordering::SeqCst));
        assert!(!dropped2.load(Ordering::SeqCst));
        drop(item1);
        assert!(dropped1.load(Ordering::SeqCst));
        assert!(!dropped2.load(Ordering::SeqCst));

        let mut item2 = gro.next().unwrap().unwrap();
        assert_eq!(item2.buffers.slice_mut(), &[4, 5, 6]);
        assert!(dropped1.load(Ordering::SeqCst));
        assert!(!dropped2.load(Ordering::SeqCst));
        drop(item2);
        assert!(dropped2.load(Ordering::SeqCst));
    }
}
