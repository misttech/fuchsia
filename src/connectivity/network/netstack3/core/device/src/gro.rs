// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Constructs that support Generic Receive Offload (GRO) at the device layer.

use alloc::vec::Vec;
use core::marker::PhantomData;

use netstack3_base::ChecksumRxOffloading;

/// A buffer that may be backed by a contiguous memory slice.
pub trait MaybeContiguousBuffer {
    /// Returns the length of the buffer.
    fn len(&self) -> usize;
    /// Returns the buffer as a contiguous slice. If `self` is non-contiguous,
    /// `storage` is used as a scratch space for linearization. Otherwise,
    /// `storage` remains untouched.
    // TODO(https://fxbug.dev/42051635): pass strongly owned buffers down to the
    // stack instead of requiring coalesced GRO frames and/or fragmented packets
    // to be copied into contiguous memory.
    fn linearized<'a>(&'a mut self, storage: &'a mut Vec<u8>) -> &'a mut [u8];
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

/// An item yielded by GRO processing.
#[derive(Debug, PartialEq, Eq)]
pub struct GroOutputItem<'s, T> {
    /// Target for the frame.
    pub target: T,
    /// Checksum offload state for the frame.
    pub checksum_offload: ChecksumRxOffloading,
    /// Mutable slice view into the frame buffer.
    pub slice: &'s mut [u8],
}

/// An iterator adapter for GRO processing.
pub struct GroIter<'a, I, B, T, E> {
    iter: I,
    _coalescing_vec: &'a mut Vec<u8>,
    linearization_vec: &'a mut Vec<u8>,
    // Holds the last buffer from `iter` into which a slice was directly
    // returned from `next()` in order to ensure that the buffer outlives the
    // slice.
    owned_prev_buffer: Option<B>,
    _marker: PhantomData<(T, E)>,
}

impl<'a, I, B, T, E> GroIter<'a, I, B, T, E> {
    /// Creates a new `GroIter`.
    pub fn new(
        iter: I,
        coalescing_vec: &'a mut Vec<u8>,
        linearization_vec: &'a mut Vec<u8>,
    ) -> Self {
        coalescing_vec.clear();
        linearization_vec.clear();
        Self {
            iter,
            _coalescing_vec: coalescing_vec,
            linearization_vec,
            owned_prev_buffer: None,
            _marker: PhantomData,
        }
    }
}

impl<'a, I, B, T, E> GroIter<'a, I, B, T, E>
where
    B: MaybeContiguousBuffer,
    I: Iterator<Item = Result<GroInputItem<B, T>, E>>,
{
    /// Advances the iterator and returns the next GRO output item.
    pub fn next<'s>(&'s mut self) -> Option<Result<GroOutputItem<'s, T>, E>> {
        self.owned_prev_buffer = None;
        // TODO(https://fxbug.dev/452980285): Create a setting to control whether
        // GRO is enabled and implement TCP coalescing.
        let item_res = self.iter.next()?;
        let GroInputItem { buffer, target, checksum_offload } = match item_res {
            Ok(item) => item,
            Err(e) => return Some(Err(e)),
        };
        let slice = self.owned_prev_buffer.insert(buffer).linearized(self.linearization_vec);
        Some(Ok(GroOutputItem { target, checksum_offload, slice }))
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
        fn len(&self) -> usize {
            self.buf.len()
        }

        fn linearized<'a>(&'a mut self, storage: &'a mut Vec<u8>) -> &'a mut [u8] {
            if self.contiguous {
                &mut self.buf[..]
            } else {
                let frame_length = self.buf.len();
                if storage.len() < frame_length {
                    storage.resize(frame_length, 0);
                }
                let slice = &mut storage[..frame_length];
                slice.copy_from_slice(&self.buf);
                slice
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

        let mut coalescing_vec = Vec::new();
        let mut linearization_vec = Vec::new();
        let mut output = Vec::new();
        let mut gro = GroIter::new(items.into_iter(), &mut coalescing_vec, &mut linearization_vec);
        while let Some(item) = gro.next() {
            let GroOutputItem { target: (), checksum_offload: _offload, slice } = item.unwrap();
            output.push(slice.to_vec());
        }

        assert_eq!(output, vec![vec![1, 2, 3], vec![4, 5, 6]]);
    }
}
