// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::buffer::{BufferAllocator, OwnedBuffer};
use std::ops::Range;
use std::sync::Arc;
use storage_ptr_slice::MutPtrByteSlice;

#[derive(Debug)]
pub(crate) struct SplittableBufferInner {
    parent_buffer: OwnedBuffer,
}

impl BufferAllocator for SplittableBufferInner {
    fn free_buffer(&self, _range: Range<usize>) {
        // No-op: Dropping the child `OwnedBuffer` drops its `Arc<dyn BufferAllocator>`,
        // which automatically decrements the `Arc` reference count of `SplittableBufferInner`.
    }
}

/// A wrapper around `OwnedBuffer` that allows carving out independent child `OwnedBuffer`s
/// and recovering the original `OwnedBuffer` once all child buffers have been dropped.
#[derive(Debug)]
pub struct SplittableBuffer {
    inner: Arc<SplittableBufferInner>,
    current_ptr: *mut u8,
    remaining_range: Range<usize>,
}

// SAFETY: `current_ptr` points into `inner.parent_buffer`'s VMO / memory region, which can be
// sent across threads.
unsafe impl Send for SplittableBuffer {}
unsafe impl Sync for SplittableBuffer {}

impl From<OwnedBuffer> for SplittableBuffer {
    fn from(mut buffer: OwnedBuffer) -> Self {
        let remaining_range = buffer.range();
        let current_ptr = buffer.as_mut_ptr();
        Self {
            inner: Arc::new(SplittableBufferInner { parent_buffer: buffer }),
            current_ptr,
            remaining_range,
        }
    }
}

impl SplittableBuffer {
    /// Returns the remaining unallocated range available for splitting.
    pub fn remaining_range(&self) -> Range<usize> {
        self.remaining_range.clone()
    }

    /// Carves out the first `len` bytes of the remaining unsplit buffer as a new `OwnedBuffer`.
    ///
    /// # Panics
    ///
    /// Panics if `len` exceeds `remaining_range.len()`.
    pub fn take_prefix(&mut self, len: usize) -> OwnedBuffer {
        assert!(len <= self.remaining_range.len());
        let child_range = self.remaining_range.start..self.remaining_range.start + len;
        self.remaining_range.start += len;
        let ptr = self.current_ptr;
        self.current_ptr = self.current_ptr.wrapping_add(len);

        // SAFETY: `child_range` is strictly within the original parent buffer bounds and
        // never overlaps with any other prefix taken from `remaining_range`. The
        // `Arc<SplittableBufferInner>` keeps the parent `OwnedBuffer` alive for `'static`.
        let slice = unsafe { MutPtrByteSlice::new(std::ptr::slice_from_raw_parts_mut(ptr, len)) };
        OwnedBuffer::new(slice, child_range, self.inner.clone() as Arc<dyn BufferAllocator>)
    }
}

impl TryFrom<SplittableBuffer> for OwnedBuffer {
    type Error = SplittableBuffer;

    fn try_from(splittable: SplittableBuffer) -> Result<Self, Self::Error> {
        let remaining_range = splittable.remaining_range.clone();
        let current_ptr = splittable.current_ptr;
        match Arc::try_unwrap(splittable.inner) {
            Ok(inner) => Ok(inner.parent_buffer),
            Err(arc) => Err(SplittableBuffer { inner: arc, current_ptr, remaining_range }),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_allocator::{BufferAllocator as PoolBufferAllocator, BufferSource};

    #[fuchsia::test]
    async fn test_splittable_buffer_split_and_merge() {
        let source = BufferSource::new(4096);
        let pool = Arc::new(PoolBufferAllocator::new(512, source));

        let owned = pool.allocate_buffer_sync_owned(2048);
        assert_eq!(owned.len(), 2048);

        let mut splittable = SplittableBuffer::from(owned);
        assert_eq!(splittable.remaining_range().len(), 2048);

        // Take two child buffers
        let mut child1 = splittable.take_prefix(1024);
        let mut child2 = splittable.take_prefix(1024);
        assert_eq!(splittable.remaining_range().len(), 0);

        child1.as_mut_slice().fill(0x11);
        child2.as_mut_slice().fill(0x22);

        // Trying to convert splittable back while children exist should fail
        let splittable = match OwnedBuffer::try_from(splittable) {
            Ok(_) => panic!("Should fail while children are active"),
            Err(s) => s,
        };

        std::mem::drop(child1);
        std::mem::drop(child2);

        // Now that all children are dropped, try_from should succeed
        let merged = OwnedBuffer::try_from(splittable).expect("Merge must succeed");
        assert_eq!(merged.len(), 2048);
        assert!(merged.as_slice()[..1024].iter().all(|&b| b == 0x11));
        assert!(merged.as_slice()[1024..].iter().all(|&b| b == 0x22));
    }
}
