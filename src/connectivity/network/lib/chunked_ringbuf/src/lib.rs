// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(
    missing_docs,
    unreachable_patterns,
    clippy::useless_conversion,
    clippy::redundant_clone,
    clippy::precedence
)]

//! A chunked ring buffer implementation.
//!
//! The chunk size is defined at time of construction. When a write results
//! in a chunk that is greater than or equal to chunk size, the current
//! tail pointer is recorded to delimit chunks. When there is not enough room
//! for a write, the next chunk is discarded to make room for the write.

#![allow(dead_code)]

use std::collections::VecDeque;

/// Chunked ring buffer.
///
/// The buffer is divided into chunks of at least `chunk_size` as declared
/// at time of construction. When making room for a write into the buffer,
/// entire chunks are discarded until there is sufficient room. A smaller
/// chunk size has the advantage of higher utilization of the space in the
/// ring buffer in exchange for more memory used keeping track of chunk
/// boundaries and more work done when discarding chunks. Conversely a
/// larger chunk size means that utilization of the buffer space has higher
/// variance, but less memory is needed and discards are less often. Users
/// should choose a chunk size appropriate for their use case.
#[derive(Debug)]
pub struct RingBuffer {
    buf: Vec<u8>,
    chunk_size: usize,
    tail: usize,
    // Empty when the ring buffer is completely empty, otherwise the front
    // element is the head pointer. This fact is used to distinguish between
    // the buffer being completely full vs completely empty.
    boundary_indices: VecDeque<usize>,
}

impl RingBuffer {
    /// Create a new chunked ring buffer.
    ///
    /// The capacity of the buffer is set to the next multiple of `chunk_size`
    /// larger than or equal to `size`.
    pub fn new(size: usize, chunk_size: usize) -> Self {
        let size = if chunk_size == 0 { size } else { size.next_multiple_of(chunk_size) };
        Self { buf: vec![0u8; size], chunk_size, tail: 0, boundary_indices: VecDeque::new() }
    }

    /// Get the head/read pointer.
    fn head(&self) -> usize {
        *self.boundary_indices.front().unwrap_or(&self.tail)
    }

    /// Compute the total number of bytes stored in the buffer.
    pub fn len(&self) -> usize {
        let head = self.head();
        if self.tail == head && !self.boundary_indices.is_empty() {
            self.buf.len()
        } else {
            self.bytes_between(head, self.tail)
        }
    }

    fn bytes_between(&self, i: usize, j: usize) -> usize {
        if j >= i { j - i } else { self.buf.len() - i + j }
    }

    /// Return the data stored in the ring buffer as two byte slices.
    pub fn get_view(&self) -> (&[u8], &[u8]) {
        let head = self.head();
        if self.tail > head || self.tail == head && self.boundary_indices.is_empty() {
            (&self.buf[head..self.tail], &[])
        } else {
            (&self.buf[head..], &self.buf[..self.tail])
        }
    }
}

impl std::io::Write for RingBuffer {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.write_vectored(&[std::io::IoSlice::new(buf)])
    }

    /// Write vectored data to the ring buffer.
    ///
    /// All data passed in a single call to `write_vectored` is considered
    /// a single "item" and will be stored in one chunk (never split when
    /// discarding).
    fn write_vectored(&mut self, bufs: &[std::io::IoSlice<'_>]) -> std::io::Result<usize> {
        let total_len: usize = bufs.iter().map(|b| b.len()).sum();
        if total_len == 0 {
            return Ok(0);
        }
        if total_len > self.buf.len() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "write is larger than buffer size",
            ));
        }

        while self.len() + total_len > self.buf.len() {
            let _ = self.boundary_indices.pop_front();
            if self.boundary_indices.is_empty() {
                break;
            }
        }
        if self.boundary_indices.is_empty() {
            self.boundary_indices.push_back(self.tail);
        }

        for slice in bufs {
            let slice = slice.as_ref();
            if self.tail + slice.len() >= self.buf.len() {
                let remaining = self.buf.len() - self.tail;
                assert!(remaining > 0);
                self.buf[self.tail..self.tail + remaining].copy_from_slice(&slice[..remaining]);
                let data_remaining = slice.len() - remaining;
                if data_remaining > 0 {
                    self.buf[..data_remaining].copy_from_slice(&slice[remaining..]);
                }
                self.tail = data_remaining;
            } else {
                self.buf[self.tail..self.tail + slice.len()].copy_from_slice(&slice);
                self.tail += slice.len();
            }
        }

        let penultimate = self
            .boundary_indices
            .back()
            .expect("boundary indices must contain at least one element");
        let bytes = if *penultimate == self.tail {
            self.buf.len()
        } else {
            self.bytes_between(*penultimate, self.tail)
        };
        if bytes >= self.chunk_size {
            self.boundary_indices.push_back(self.tail);
        }
        Ok(total_len)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_write_no_wrap() {
        let mut cb = RingBuffer::new(8, 4);
        const DATA: &[u8] = b"hello";
        cb.write_all(DATA).unwrap();

        assert_eq!(cb.boundary_indices, VecDeque::from([0, 5]));
        let (v1, v2) = cb.get_view();
        assert_eq!(v1, DATA);
        assert_eq!(v2, &[]);
    }

    #[test]
    fn test_write_wrap() {
        let mut cb = RingBuffer::new(8, 4);
        assert_eq!(
            cb.write_vectored(&[std::io::IoSlice::new(b"foo"), std::io::IoSlice::new(b"bar")])
                .unwrap(),
            6
        );
        assert_eq!(cb.boundary_indices, VecDeque::from([0, 6]));

        assert_eq!(
            cb.write_vectored(&[std::io::IoSlice::new(b"baz"), std::io::IoSlice::new(b"qux")])
                .unwrap(),
            6
        );
        assert_eq!(cb.boundary_indices, VecDeque::from([6, 4]));

        let (v1, v2) = cb.get_view();
        assert_eq!(v1, b"ba");
        assert_eq!(v2, b"zqux");
    }

    #[test]
    fn test_write_exact_fill() {
        let mut cb = RingBuffer::new(8, 4);
        cb.write_all(b"12345678").unwrap();
        assert_eq!(cb.boundary_indices, VecDeque::from([0, 0]));

        assert_eq!(cb.tail, 0);

        let (v1, v2) = cb.get_view();
        assert_eq!(v1, b"12345678");
        assert_eq!(v2, &[]);

        cb.write_all(b"9").unwrap();
        let (v1, v2) = cb.get_view();
        assert_eq!(v1, b"9");
        assert_eq!(v2, &[]);
    }

    #[test]
    fn test_write_smaller_than_chunk_size() {
        const CHUNK_SIZE: usize = 4;
        let mut cb = RingBuffer::new(8, CHUNK_SIZE);
        for i in 1u8..4 {
            cb.write_all(&[i]).unwrap();
            assert_eq!(cb.boundary_indices, VecDeque::from([0]));
        }
        cb.write_all(&[4]).unwrap();
        assert_eq!(cb.boundary_indices, VecDeque::from([0, CHUNK_SIZE]));

        let (v1, v2) = cb.get_view();
        assert_eq!(v1, &[1, 2, 3, 4]);
        assert_eq!(v2, &[]);
    }

    #[test]
    fn test_zero_chunk_size_zero_writes() {
        let mut cb = RingBuffer::new(8, 0);
        // Call methods we actually needed to implement instead of write_all
        // which checks for zero-length writes themselves.
        assert_eq!(cb.write(b"").unwrap(), 0);
        assert_eq!(cb.boundary_indices, VecDeque::new());
        let (v1, v2) = cb.get_view();
        assert_eq!(v1, &[]);
        assert_eq!(v2, &[]);
    }

    #[test]
    fn test_chunk_size_equals_buffer_size_fill_and_overwrite() {
        let mut cb = RingBuffer::new(8, 8);

        cb.write_all(b"12345678").unwrap();
        cb.write_all(b"9").unwrap();

        let (v1, v2) = cb.get_view();
        assert_eq!(v1, b"9");
        assert_eq!(v2, &[]);
    }

    #[test]
    fn test_zero_chunk_size_fill_and_overwrite() {
        const N: usize = 4;
        let mut cb = RingBuffer::new(N, 0);

        for i in 1u8..=4 {
            cb.write_all(&[i]).unwrap();
        }

        cb.write_all(&[5]).unwrap();
        let (v1, v2) = cb.get_view();
        assert_eq!([v1, v2].concat(), vec![2, 3, 4, 5]);

        cb.write_all(&[6]).unwrap();
        let (v1, v2) = cb.get_view();
        assert_eq!([v1, v2].concat(), vec![3, 4, 5, 6]);
    }
}
