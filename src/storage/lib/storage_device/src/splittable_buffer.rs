// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::buffer::{BufferAllocator, OwnedBuffer};
use std::ops::Range;
use std::ptr::slice_from_raw_parts_mut;
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

    fn is_trusted(&self) -> bool {
        self.parent_buffer.try_as_slice().is_some()
    }
}

/// A clonable handle referencing the underlying `SplittableBufferInner`.
///
/// Can be captured inside background sub-read callbacks and consumed via `into_buffer(self)`
/// once all child `OwnedBuffer`s and other handles drop to recover the merged `OwnedBuffer`.
#[derive(Clone, Debug)]
pub struct SplittableBufferHandle {
    inner: Arc<SplittableBufferInner>,
}

impl SplittableBufferHandle {
    /// Consumes this handle and attempts to unwrap the underlying `Arc<SplittableBufferInner>`.
    ///
    /// Returns `Some(parent_buffer)` if this handle and all child `OwnedBuffer`s carved from
    /// `SplittableBuffer` have been dropped (`Arc::strong_count == 1`). Otherwise returns `None`.
    pub fn into_buffer(self) -> Option<OwnedBuffer> {
        Arc::into_inner(self.inner).map(|inner| inner.parent_buffer)
    }
}

/// A wrapper around `OwnedBuffer` that allows carving out independent child `OwnedBuffer`s
/// and recovering the original `OwnedBuffer` via a `SplittableBufferHandle` once all child
/// buffers have been dropped.
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

impl SplittableBuffer {
    /// Creates a new `SplittableBuffer` along with a clonable `SplittableBufferHandle` that can be
    /// used to recover the merged `buffer` once all child `OwnedBuffer`s and other handles drop.
    pub fn new(mut buffer: OwnedBuffer) -> (Self, SplittableBufferHandle) {
        let remaining_range = buffer.range();
        let current_ptr = buffer.as_mut_ptr();
        let inner = Arc::new(SplittableBufferInner { parent_buffer: buffer });
        let handle = SplittableBufferHandle { inner: inner.clone() };
        let splittable = Self { inner, current_ptr, remaining_range };
        (splittable, handle)
    }

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
        let slice = unsafe { MutPtrByteSlice::new(slice_from_raw_parts_mut(ptr, len)) };
        OwnedBuffer::new(slice, child_range, self.inner.clone() as Arc<dyn BufferAllocator>)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buffer_allocator::{BufferAllocator as PoolBufferAllocator, BufferSource};

    #[fuchsia::test]
    async fn test_splittable_buffer_handle_into_buffer() {
        let source = BufferSource::new(4096);
        let pool = Arc::new(PoolBufferAllocator::new(512, source));
        let owned = pool.allocate_buffer_sync_owned(2048);

        let (mut splittable, handle) = SplittableBuffer::new(owned);
        let mut child1 = splittable.take_prefix(1024);
        let mut child2 = splittable.take_prefix(1024);
        child1.as_mut_ptr_slice().fill(0x33);
        child2.as_mut_ptr_slice().fill(0x44);

        // Drop splittable and child1 first while child2 is still active.
        drop(splittable);
        drop(child1);

        let handle_clone = handle.clone();
        assert!(handle_clone.into_buffer().is_none());

        // Drop child2. Now only handle remains (strong count == 1), so into_buffer succeeds!
        drop(child2);
        let merged = handle.into_buffer().expect("into_buffer must succeed when sole reference");
        assert_eq!(merged.len(), 2048);
        assert!(merged.as_ptr_slice().subslice(0..1024).iter_as::<u8>().all(|b| b == 0x33));
        assert!(merged.as_ptr_slice().subslice(1024..2048).iter_as::<u8>().all(|b| b == 0x44));
    }
}
