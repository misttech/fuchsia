// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::buffer_allocator::BufferAllocator as PoolBufferAllocator;
use std::borrow::Borrow;
use std::marker::PhantomData;
use std::ops::{Bound, Range, RangeBounds};
use std::slice::SliceIndex;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};

pub use crate::buffer_allocator::BufferFuture;

/// An entity capable of reclaiming a memory buffer range when dropped.
pub trait BufferAllocator: Send + Sync + std::fmt::Debug + 'static {
    /// Frees or reclaims the specified memory range.
    fn free_buffer(&self, range: Range<usize>);
}

pub(super) fn round_down<T>(value: T, granularity: T) -> T
where
    T: num::Num + Copy,
{
    value - value % granularity
}

pub(super) fn round_up<T>(value: T, granularity: T) -> T
where
    T: num::Num + Copy,
{
    round_down(value + granularity - T::one(), granularity)
}

// Returns a range within a range.
// For example, subrange(100..200, 20..30) = 120..130.
fn subrange<R: RangeBounds<usize>>(source: &Range<usize>, bounds: &R) -> Range<usize> {
    let subrange = (match bounds.start_bound() {
        Bound::Included(&s) => source.start + s,
        Bound::Excluded(&s) => source.start + s + 1,
        Bound::Unbounded => source.start,
    })..(match bounds.end_bound() {
        Bound::Included(&e) => source.start + e + 1,
        Bound::Excluded(&e) => source.start + e,
        Bound::Unbounded => source.end,
    });
    assert!(subrange.end <= source.end);
    subrange
}

fn split_range(range: &Range<usize>, mid: usize) -> (Range<usize>, Range<usize>) {
    let l = range.end - range.start;
    let base = range.start;
    (base..base + mid, base + mid..base + l)
}
/// Buffer is a read-write buffer that can be used for I/O with the block device. They are created
/// by a BufferAllocator, and automatically deallocate themselves when they go out of scope.
///
/// Most usage will be on the unowned BufferRef and MutableBufferRef types, since these types are
/// used for Device::read and Device::write.
///
/// Buffers are always block-aligned (both in offset and length), but unaligned slices can be made
/// with the reference types. That said, the Device trait requires aligned BufferRef and
/// MutableBufferRef objects, so alignment must be restored by the time a device read/write is
/// requested.
///
/// For example, when writing an unaligned amount of data to the device, generally two Buffers
/// would need to be involved; the input Buffer could be used to write everything up to the last
/// block, and a second single-block alignment Buffer would be used to read-modify-update the last
/// block.
use std::sync::Arc;

#[derive(Debug)]
pub struct BufferImpl<'a, H: Borrow<A>, A: ?Sized + BufferAllocator> {
    slice: MutPtrByteSlice<'a>,
    range: Range<usize>,
    allocator: H,
    _phantom: PhantomData<fn() -> &'a A>,
}

pub type Buffer<'a> = BufferImpl<'a, &'a PoolBufferAllocator, PoolBufferAllocator>;
pub type OwnedBuffer = BufferImpl<'static, Arc<dyn BufferAllocator>, dyn BufferAllocator>;

// Alias for the traits which need to be satisfied for `subslice` and friends.
// This trait is automatically satisfied for most typical uses (a..b, a.., ..b, ..).
pub trait SliceRange: Clone + RangeBounds<usize> + SliceIndex<[u8], Output = [u8]> {}
impl<T> SliceRange for T where T: Clone + RangeBounds<usize> + SliceIndex<[u8], Output = [u8]> {}

impl<'a, H: Borrow<A>, A: ?Sized + BufferAllocator> BufferImpl<'a, H, A> {
    pub(super) fn new(slice: MutPtrByteSlice<'a>, range: Range<usize>, allocator: H) -> Self {
        assert_eq!(slice.len(), range.end - range.start);
        Self { slice, range, allocator, _phantom: PhantomData }
    }

    /// Takes a read-only reference to this buffer.
    pub fn as_ref(&self) -> BufferRef<'_> {
        self.subslice(..)
    }

    /// Takes a read-only reference to this buffer over `range` (which must be within the size of
    /// the buffer).
    pub fn subslice<R: SliceRange>(&self, range: R) -> BufferRef<'_> {
        let new_range = subrange(&self.range, &range);
        let relative_range =
            (new_range.start - self.range.start)..(new_range.end - self.range.start);
        let slice = self.slice.as_ptr_slice().subslice(relative_range);
        BufferRef { slice, start: new_range.start, end: new_range.end }
    }

    /// Takes a read-write reference to this buffer.
    pub fn as_mut(&mut self) -> MutableBufferRef<'_> {
        self.subslice_mut(..)
    }

    /// Takes a read-write reference to this buffer over `range` (which must be within the size of
    /// the buffer).
    pub fn subslice_mut<R: SliceRange>(&mut self, range: R) -> MutableBufferRef<'_> {
        let new_range = subrange(&self.range, &range);
        let relative_range =
            (new_range.start - self.range.start)..(new_range.end - self.range.start);
        let slice = self.slice.reborrow().subslice_mut(relative_range);
        MutableBufferRef { slice, range: new_range }
    }

    /// Returns the buffer's capacity.
    pub fn len(&self) -> usize {
        self.range.end - self.range.start
    }

    /// Returns a slice of the buffer's contents.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: `self.slice` points to a valid memory range of `self.len()` bytes.
        unsafe { std::slice::from_raw_parts(self.slice.as_ptr(), self.len()) }
    }

    /// Returns a mutable slice of the buffer's contents.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: `&mut self` guarantees exclusive mutable access to this range of `self.len()`
        // bytes.
        unsafe { std::slice::from_raw_parts_mut(self.slice.as_mut_ptr(), self.len()) }
    }

    /// Copies the contents of this buffer into `dest`.
    ///
    /// # Panics
    ///
    /// Panics if `dest.len() != self.len()`.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        self.slice.as_ptr_slice().copy_to_slice(dest);
    }

    /// Copies the contents of `src` into this buffer.
    ///
    /// # Panics
    ///
    /// Panics if `src.len() != self.len()`.
    pub fn copy_from_slice(&mut self, src: &[u8]) {
        self.as_mut_slice().copy_from_slice(src);
    }

    /// Fills the buffer with `val`.
    pub fn fill(&mut self, val: u8) {
        self.slice.fill(val);
    }

    /// Returns the range in the underlying BufferSource that this buffer covers.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Returns a reference to the allocator.
    pub fn allocator(&self) -> &A {
        self.allocator.borrow()
    }

    /// Returns the buffer's contents as a Vec.
    pub fn to_vec(&self) -> Vec<u8> {
        self.as_ref().to_vec()
    }

    /// Appends the buffer's contents to `vec`.
    pub fn append_to(&self, vec: &mut Vec<u8>) {
        self.as_ref().append_to(vec)
    }

    /// Returns a raw pointer to the buffer's contents.
    pub fn as_ptr(&self) -> *const u8 {
        self.slice.as_ptr()
    }

    /// Returns a mutable raw pointer to the buffer's contents.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.slice.as_mut_ptr()
    }

    /// Returns a read-only pointer slice over the buffer.
    pub fn as_ptr_slice(&self) -> PtrByteSlice<'_> {
        self.slice.as_ptr_slice()
    }

    /// Returns a mutable pointer slice over the buffer.
    pub fn as_mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        self.slice.reborrow()
    }
}

impl<'a, H: Borrow<A>, A: ?Sized + BufferAllocator> Drop for BufferImpl<'a, H, A> {
    fn drop(&mut self) {
        self.allocator.borrow().free_buffer(self.range.clone());
    }
}

/// BufferRef is an unowned, read-only view over a Buffer.
#[derive(Clone, Copy, Debug)]
pub struct BufferRef<'a> {
    slice: PtrByteSlice<'a>,
    start: usize, // Not range so that we get Copy.
    end: usize,
}

impl<'a> BufferRef<'a> {
    /// Returns the buffer's capacity.
    pub fn len(&self) -> usize {
        self.end - self.start
    }

    pub fn is_empty(&self) -> bool {
        self.end == self.start
    }

    /// Returns a slice of the buffer's contents.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: The caller must ensure safety if the buffer is shared. This is a temporary
        // compatibility shim during the soft-transition.
        unsafe { std::slice::from_raw_parts(self.slice.as_ptr(), self.len()) }
    }

    /// Slices and consumes this reference. See Buffer::subslice.
    pub fn subslice<R: SliceRange>(&self, range: R) -> BufferRef<'_> {
        let new_range = subrange(&self.range(), &range);
        let relative_range = (new_range.start - self.start)..(new_range.end - self.start);
        let slice = self.slice.subslice(relative_range);
        BufferRef { slice, start: new_range.start, end: new_range.end }
    }

    /// Splits at `mid` (included in the right child), yielding two BufferRefs.
    pub fn split_at(&self, mid: usize) -> (BufferRef<'_>, BufferRef<'_>) {
        let ranges = split_range(&self.range(), mid);
        let (left_slice, right_slice) = self.slice.split_at(mid);
        (
            BufferRef { slice: left_slice, start: ranges.0.start, end: ranges.0.end },
            BufferRef { slice: right_slice, start: ranges.1.start, end: ranges.1.end },
        )
    }

    /// Returns the range in the underlying BufferSource that this BufferRef covers.
    pub fn range(&self) -> Range<usize> {
        self.start..self.end
    }

    /// Copies the contents of this buffer into `dest`.
    ///
    /// # Panics
    ///
    /// Panics if `dest.len() != self.len()`.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        self.slice.copy_to_slice(dest);
    }

    /// Returns the buffer's contents as a Vec.
    pub fn to_vec(&self) -> Vec<u8> {
        self.slice.to_vec()
    }

    /// Appends the buffer's contents to `vec`.
    pub fn append_to(&self, vec: &mut Vec<u8>) {
        self.slice.append_to(vec);
    }

    /// Returns a raw pointer to the buffer's contents.
    pub fn as_ptr(&self) -> *const u8 {
        self.slice.as_ptr()
    }

    /// Returns a read-only pointer slice over the buffer.
    pub fn as_ptr_slice(&self) -> PtrByteSlice<'a> {
        self.slice
    }
}

/// MutableBufferRef is an unowned, read-write view of a Buffer.
#[derive(Debug)]
pub struct MutableBufferRef<'a> {
    slice: MutPtrByteSlice<'a>,
    range: Range<usize>,
}

impl<'a> MutableBufferRef<'a> {
    /// Returns the buffer's capacity.
    pub fn len(&self) -> usize {
        self.range.end - self.range.start
    }

    pub fn is_empty(&self) -> bool {
        self.range.end == self.range.start
    }

    /// Returns a read-only view of the buffer.
    pub fn as_ref(&self) -> BufferRef<'_> {
        BufferRef { slice: self.slice.as_ptr_slice(), start: self.range.start, end: self.range.end }
    }

    /// Consumes this reference and returns a read-only view.
    pub fn into_ref(self) -> BufferRef<'a> {
        BufferRef { slice: self.slice.into(), start: self.range.start, end: self.range.end }
    }

    /// Returns a slice of the buffer's contents.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: The caller must ensure safety if the buffer is shared. This is a temporary
        // compatibility shim during the soft-transition.
        unsafe { std::slice::from_raw_parts(self.slice.as_ptr(), self.len()) }
    }

    /// Returns a mutable slice of the buffer's contents.
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: The caller must ensure safety if the buffer is shared. This is a temporary
        // compatibility shim during the soft-transition.
        unsafe { std::slice::from_raw_parts_mut(self.slice.as_mut_ptr(), self.len()) }
    }

    /// Reborrows this reference with a lesser lifetime. This mirrors the usual borrowing semantics
    /// (i.e. the borrow ends when the new reference goes out of scope), and exists so that a
    /// MutableBufferRef can be subsliced without consuming it.
    ///
    /// For example:
    ///    let mut buf: MutableBufferRef<'_> = ...;
    ///    {
    ///        let sub = buf.reborrow().subslice_mut(a..b);
    ///    }
    pub fn reborrow(&mut self) -> MutableBufferRef<'_> {
        MutableBufferRef { slice: self.slice.reborrow(), range: self.range.clone() }
    }

    /// Slices this reference. See Buffer::subslice.
    pub fn subslice<R: SliceRange>(&self, range: R) -> BufferRef<'_> {
        let new_range = subrange(&self.range, &range);
        let relative_range =
            (new_range.start - self.range.start)..(new_range.end - self.range.start);
        let slice = self.slice.as_ptr_slice().subslice(relative_range);
        BufferRef { slice, start: new_range.start, end: new_range.end }
    }

    /// Slices and consumes this reference. See Buffer::subslice_mut.
    pub fn subslice_mut<R: SliceRange>(mut self, range: R) -> MutableBufferRef<'a> {
        let new_range = subrange(&self.range, &range);
        let relative_range =
            (new_range.start - self.range.start)..(new_range.end - self.range.start);
        self.slice = self.slice.subslice_mut(relative_range);
        self.range = new_range;
        self
    }

    /// Splits at `mid` (included in the right child), yielding two BufferRefs.
    pub fn split_at(&self, mid: usize) -> (BufferRef<'_>, BufferRef<'_>) {
        let ranges = split_range(&self.range, mid);
        let (left_slice, right_slice) = self.slice.as_ptr_slice().split_at(mid);
        (
            BufferRef { slice: left_slice, start: ranges.0.start, end: ranges.0.end },
            BufferRef { slice: right_slice, start: ranges.1.start, end: ranges.1.end },
        )
    }

    /// Consumes the reference and splits it at `mid` (included in the right child), yielding two
    /// MutableBufferRefs.
    pub fn split_at_mut(self, mid: usize) -> (MutableBufferRef<'a>, MutableBufferRef<'a>) {
        let ranges = split_range(&self.range, mid);
        let (left_slice, right_slice) = self.slice.split_at_mut(mid);
        (
            MutableBufferRef { slice: left_slice, range: ranges.0 },
            MutableBufferRef { slice: right_slice, range: ranges.1 },
        )
    }

    /// Returns the range in the underlying BufferSource that this MutableBufferRef covers.
    pub fn range(&self) -> Range<usize> {
        self.range.clone()
    }

    /// Copies the contents of this buffer into `dest`.
    ///
    /// # Panics
    ///
    /// Panics if `dest.len() != self.len()`.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        self.slice.copy_to_slice(dest);
    }

    /// Copies the contents of `src` into this buffer.
    ///
    /// # Panics
    ///
    /// Panics if `src.len() != self.len()`.
    pub fn copy_from_slice(&mut self, src: &[u8]) {
        self.slice.copy_from_ptr_slice(src.into());
    }

    /// Fills the buffer with `val`.
    pub fn fill(&mut self, val: u8) {
        self.slice.fill(val);
    }

    /// Returns the buffer's contents as a Vec.
    pub fn to_vec(&self) -> Vec<u8> {
        self.slice.to_vec()
    }

    /// Appends the buffer's contents to `vec`.
    pub fn append_to(&self, vec: &mut Vec<u8>) {
        self.slice.append_to(vec);
    }

    /// Returns a raw pointer to the buffer's contents.
    pub fn as_ptr(&self) -> *const u8 {
        self.slice.as_ptr()
    }

    /// Returns a mutable raw pointer to the buffer's contents.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.slice.as_mut_ptr()
    }

    /// Returns a read-only pointer slice over the buffer.
    pub fn as_ptr_slice(&self) -> PtrByteSlice<'_> {
        self.slice.as_ptr_slice()
    }

    /// Returns a mutable pointer slice over the buffer.
    pub fn as_mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        self.slice.reborrow()
    }

    /// Consumes this reference and returns a mutable pointer slice.
    pub fn into_mut_ptr_slice(self) -> MutPtrByteSlice<'a> {
        self.slice
    }
}

// SAFETY: BufferRef is a read-only view over allocator-managed memory. It does not allow
// mutation and behaves like `&[u8]`, which is Send and Sync.
unsafe impl Send for BufferRef<'_> {}
// SAFETY: See Send impl above.
unsafe impl Sync for BufferRef<'_> {}

// SAFETY: MutableBufferRef behaves like `&mut [u8]`. It enforces exclusivity (no overlapping
// views) and does not have interior mutability, making it safe to Send and Sync.
unsafe impl Send for MutableBufferRef<'_> {}
// SAFETY: See Send impl above.
unsafe impl Sync for MutableBufferRef<'_> {}
