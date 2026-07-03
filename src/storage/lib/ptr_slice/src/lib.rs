// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe wrappers for raw pointer byte slices.
//!
//! This crate provides [`PtrByteSlice`] and [`MutPtrByteSlice`], which are designed for use in
//! scenarios involving cross-process shared memory (e.g., communication with driver processes or
//! other untrusted components).
//!
//! ### Rationale
//!
//! In a multi-process system like Fuchsia, processes often share memory via VMOs (Virtual Memory
//! Objects). If a process shares a memory region with another process, that other process (which
//! may be compromised or untrusted) can modify the memory concurrently at any time.
//!
//! In Rust, creating a standard reference (`&[u8]` or `&mut [u8]`) over memory that can be
//! modified concurrently by another party is **Undefined Behavior (UB)**. The Rust compiler
//! assumes that the data behind a shared reference (`&T`) is immutable and cannot change
//! unexpectedly, allowing it to perform optimizations that assume stability. If the memory changes
//! concurrently, these assumptions are violated.
//!
//! To avoid UB, we must avoid creating standard Rust references to concurrently-modifiable shared
//! memory. Instead, we must treat the shared memory as raw pointers.
//!
//! [`PtrByteSlice`] and [`MutPtrByteSlice`] wrap these raw pointers and provide a safe API to:
//! 1.  **Copy data out** of the shared region into private, allocator-managed memory (e.g., via
//!     `copy_to_slice` or `to_vec`). Once copied, the private data is safe from concurrent
//!     modification and can be safely represented as standard Rust slices.
//! 2.  **Perform structured access** (e.g., via `chunks` or `chunks_mut`) only when the underlying
//!     types guarantee that arbitrary byte patterns are valid (via `FromBytes`) and we accept that
//!     the values might change (though we must still be careful about Time-of-Check to Time-of-Use
//!     (TOCTOU) vulnerabilities).
//!
//! By removing direct access to the underlying slice (i.e., not providing `as_slice` or
//! `as_mut_slice` methods), this crate enforces that helper components must copy data into trusted
//! buffers before operating on it, ensuring both memory safety (no UB) and robustness against
//! concurrent modification.
//!
//! This crate does nothing to prevent data races; responsibility for handling data races lies
//! elsewhere.

use std::marker::PhantomData;
use zerocopy::FromBytes;

/// A read-only view of a raw pointer byte slice, providing a safe API.
#[derive(Debug, Copy, Clone)]
pub struct PtrByteSlice<'a> {
    slice: *const [u8],
    _marker: PhantomData<&'a [u8]>,
}

impl<'a> PtrByteSlice<'a> {
    /// Creates a new `PtrByteSlice` from a raw pointer to a byte slice.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `slice` is valid for reads for the lifetime `'a`.
    pub unsafe fn new(slice: *const [u8]) -> Self {
        Self { slice, _marker: PhantomData }
    }

    /// Returns the length of the slice in bytes.
    pub fn len(&self) -> usize {
        self.slice.len()
    }

    /// Returns `true` if the slice has a length of 0.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies the contents of this slice into a safe Rust mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if `dest` is smaller than `self.len()`.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        assert!(dest.len() >= self.len());
        // SAFETY:
        // - `self.slice` is valid for reads of `self.len()` bytes (guaranteed by `Self::new`
        //   safety contract).
        // - `dest` is valid for writes of `self.len()` bytes (ensured by the assert).
        // - The memory regions do not overlap because `dest` is an exclusive Rust reference.
        unsafe {
            std::ptr::copy_nonoverlapping(self.slice as *const u8, dest.as_mut_ptr(), self.len());
        }
    }

    /// Returns a subslice of this pointer slice.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    pub fn subslice(&self, range: std::ops::Range<usize>) -> Self {
        assert!(range.start <= range.end);
        assert!(range.end <= self.len());
        // SAFETY:
        // - `range` is within the bounds of `self.slice` (ensured by asserts).
        // - The original `self.slice` is valid for reads for `'a`, so any subslice of it
        //   is also valid for reads for `'a`.
        unsafe {
            let new_ptr = (self.slice as *const u8).add(range.start);
            let new_slice = std::ptr::slice_from_raw_parts(new_ptr, range.end - range.start);
            Self::new(new_slice)
        }
    }

    /// Splits the slice into two at the given index.
    ///
    /// # Panics
    ///
    /// Panics if `mid` is out of bounds.
    pub fn split_at(self, mid: usize) -> (Self, Self) {
        assert!(mid <= self.len());
        // SAFETY:
        // - `mid` is within the bounds of `self.slice` (ensured by assert).
        // - The two subslices are valid for reads for `'a` as they are parts of the original
        //   valid slice.
        unsafe {
            let ptr = self.slice as *const u8;
            (
                Self::new(std::ptr::slice_from_raw_parts(ptr, mid)),
                Self::new(std::ptr::slice_from_raw_parts(ptr.add(mid), self.len() - mid)),
            )
        }
    }

    /// Returns the raw pointer to the slice.
    pub fn as_raw_slice_ptr(&self) -> *const [u8] {
        self.slice
    }

    /// Returns a raw pointer to the start of the slice.
    pub fn as_ptr(&self) -> *const u8 {
        self.slice as *const u8
    }

    /// Allocates a new heap Vector and copies the contents into it.
    /// Bypasses zero-initialization using raw pointer copies.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut vec = Vec::with_capacity(self.len());
        // SAFETY: The memory is guaranteed to be valid for reads up to `self.len()`
        // for the lifetime of this pointer slice.
        unsafe {
            std::ptr::copy_nonoverlapping(self.slice as *const u8, vec.as_mut_ptr(), self.len());
            vec.set_len(self.len());
        }
        vec
    }

    /// Appends the contents of this slice to the given vector, expanding its capacity if needed.
    /// Bypasses zero-initialization using raw pointer copies.
    pub fn append_to(&self, vec: &mut Vec<u8>) {
        let old_len = vec.len();
        let new_len = old_len + self.len();
        vec.reserve(self.len());
        // SAFETY:
        // - We reserved enough capacity in `vec` to fit `self.len()` more bytes.
        // - `dest_ptr` points to the unused capacity.
        // - `self.slice` is valid for reads of `self.len()` bytes.
        // - The source and destination do not overlap because `vec` is owned and allocated
        //   separately.
        unsafe {
            let dest_ptr = vec.as_mut_ptr().add(old_len);
            std::ptr::copy_nonoverlapping(self.slice as *const u8, dest_ptr, self.len());
            vec.set_len(new_len);
        }
    }

    /// Returns an iterator over read-only chunks of type `T`.
    ///
    /// # Panics
    ///
    /// Panics if the slice is not aligned to `T` or if its length in bytes is not a multiple of
    /// `size_of::<T>()`.
    pub fn chunks<T: Copy + FromBytes>(&self) -> Chunks<'_, T> {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        assert!(size > 0, "Chunk size must be greater than 0");
        assert_eq!(self.slice as *const u8 as usize % align, 0, "Slice is not aligned to T");
        assert_eq!(self.len() % size, 0, "Slice length is not a multiple of T size");

        // SAFETY:
        // - `self.slice` is aligned to `T` (ensured by assert).
        // - The end pointer is calculated within the bounds of the original slice.
        // - Pointer arithmetic within the same allocated object is safe.
        let end = unsafe { (self.slice as *const T).add(self.len() / size) };
        Chunks { ptr: self.slice as *const T, end, _marker: PhantomData }
    }
}

/// A mutable view of a raw pointer byte slice, providing a safe API.
#[derive(Debug)]
pub struct MutPtrByteSlice<'a> {
    slice: *mut [u8],
    _marker: PhantomData<&'a mut [u8]>,
}

impl<'a> MutPtrByteSlice<'a> {
    /// Creates a new `MutPtrByteSlice` from a raw mutable pointer to a byte slice.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `slice` is valid for reads and writes for the lifetime `'a`.
    pub unsafe fn new(slice: *mut [u8]) -> Self {
        Self { slice, _marker: PhantomData }
    }

    /// Returns the length of the slice in bytes.
    pub fn len(&self) -> usize {
        self.slice.len()
    }

    /// Returns `true` if the slice has a length of 0.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Copies the contents of this slice into a safe Rust mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if `dest` is smaller than `self.len()`.
    pub fn copy_to_slice(&self, dest: &mut [u8]) {
        assert!(dest.len() >= self.len());
        // SAFETY:
        // - `self.slice` is valid for reads of `self.len()` bytes (guaranteed by `Self::new`
        //   safety contract).
        // - `dest` is valid for writes of `self.len()` bytes (ensured by the assert).
        // - The memory regions do not overlap because `dest` is an exclusive Rust reference.
        unsafe {
            std::ptr::copy_nonoverlapping(self.slice as *mut u8, dest.as_mut_ptr(), self.len());
        }
    }

    /// Copies the contents of another read-only pointer slice into this mutable slice.
    ///
    /// # Panics
    ///
    /// Panics if the lengths of the slices do not match.
    pub fn copy_from_ptr_slice(&mut self, src: PtrByteSlice<'_>) {
        assert_eq!(self.len(), src.len());
        // SAFETY:
        // - `self.slice` is valid for writes of `self.len()` bytes.
        // - `src` is valid for reads of `src.len()` (which equals `self.len()`) bytes.
        // - They do not overlap because `self` (mutable) and `src` (immutable) cannot alias
        //   under Rust's borrowing rules.
        unsafe {
            std::ptr::copy_nonoverlapping(src.as_ptr(), self.slice as *mut u8, self.len());
        }
    }

    /// Fills the slice with the given byte value.
    pub fn fill(&mut self, val: u8) {
        // SAFETY: `self.slice` is valid for writes of `self.len()` bytes.
        unsafe {
            std::ptr::write_bytes(self.slice as *mut u8, val, self.len());
        }
    }

    /// Returns a read-only view of this slice.
    pub fn as_ptr_slice(&self) -> PtrByteSlice<'_> {
        // SAFETY: `self.slice` is valid for reads (since it is valid for writes) for `'a`.
        unsafe { PtrByteSlice::new(self.slice as *const [u8]) }
    }

    /// Returns a mutable subslice of this pointer slice.
    ///
    /// # Panics
    ///
    /// Panics if the range is out of bounds.
    pub fn subslice_mut(&mut self, range: std::ops::Range<usize>) -> Self {
        assert!(range.start <= range.end);
        assert!(range.end <= self.len());
        // SAFETY:
        // - `range` is within the bounds of `self.slice` (ensured by asserts).
        // - The original `self.slice` is valid for reads and writes for `'a`, so any subslice of it
        //   is also valid for reads and writes for `'a`.
        unsafe {
            let new_ptr = (self.slice as *mut u8).add(range.start);
            let new_slice = std::ptr::slice_from_raw_parts_mut(new_ptr, range.end - range.start);
            Self::new(new_slice)
        }
    }

    /// Splits the slice into two at the given index.
    ///
    /// # Panics
    ///
    /// Panics if `mid` is out of bounds.
    pub fn split_at_mut(self, mid: usize) -> (Self, Self) {
        assert!(mid <= self.len());
        // SAFETY:
        // - `mid` is within the bounds of `self.slice` (ensured by assert).
        // - The two subslices are valid for reads and writes for `'a` as they are parts of the
        //   original valid slice.
        // - They do not overlap.
        unsafe {
            let ptr = self.slice as *mut u8;
            (
                Self::new(std::ptr::slice_from_raw_parts_mut(ptr, mid)),
                Self::new(std::ptr::slice_from_raw_parts_mut(ptr.add(mid), self.len() - mid)),
            )
        }
    }

    /// Returns a raw pointer to the start of the slice.
    pub fn as_ptr(&self) -> *const u8 {
        self.slice as *const u8
    }

    /// Returns a raw mutable pointer to the start of the slice.
    pub fn as_mut_ptr(&self) -> *mut u8 {
        self.slice as *mut u8
    }

    /// Reborrows the mutable slice with a shorter lifetime.
    pub fn reborrow(&mut self) -> MutPtrByteSlice<'_> {
        MutPtrByteSlice { slice: self.slice, _marker: std::marker::PhantomData }
    }

    /// Allocates a new heap Vector and copies the contents into it.
    /// Bypasses zero-initialization using raw pointer copies.
    pub fn to_vec(&self) -> Vec<u8> {
        let mut vec = Vec::with_capacity(self.len());
        // SAFETY: The memory is guaranteed to be valid for reads up to `self.len()`
        // for the lifetime of this pointer slice.
        unsafe {
            std::ptr::copy_nonoverlapping(self.slice as *mut u8, vec.as_mut_ptr(), self.len());
            vec.set_len(self.len());
        }
        vec
    }

    /// Appends the contents of this slice to the given vector, expanding its capacity if needed.
    /// Bypasses zero-initialization using raw pointer copies.
    pub fn append_to(&self, vec: &mut Vec<u8>) {
        let old_len = vec.len();
        let new_len = old_len + self.len();
        vec.reserve(self.len());
        // SAFETY:
        // - We reserved enough capacity in `vec` to fit `self.len()` more bytes.
        // - `dest_ptr` points to the unused capacity.
        // - `self.slice` is valid for reads of `self.len()` bytes.
        // - The source and destination do not overlap because `vec` is owned and allocated
        //   separately.
        unsafe {
            let dest_ptr = vec.as_mut_ptr().add(old_len);
            std::ptr::copy_nonoverlapping(self.slice as *mut u8, dest_ptr, self.len());
            vec.set_len(new_len);
        }
    }

    /// Returns an iterator over mutable chunks of type `T`.
    ///
    /// # Panics
    ///
    /// Panics if the slice is not aligned to `T` or if its length in bytes is not a multiple of
    /// `size_of::<T>()`.
    pub fn chunks_mut<T: Copy + FromBytes>(&mut self) -> ChunksMut<'_, T> {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        assert!(size > 0, "Chunk size must be greater than 0");
        assert_eq!(self.slice as *mut u8 as usize % align, 0, "Slice is not aligned to T");
        assert_eq!(self.len() % size, 0, "Slice length is not a multiple of T size");

        // SAFETY:
        // - `self.slice` is aligned to `T` (ensured by assert).
        // - The end pointer is calculated within the bounds of the original slice.
        // - Pointer arithmetic within the same allocated object is safe.
        let end = unsafe { (self.slice as *mut T).add(self.len() / size) };
        ChunksMut { ptr: self.slice as *mut T, end, _marker: PhantomData }
    }
}

// SAFETY: `PtrByteSlice` is conceptually a read-only view of a byte slice (`&[u8]`).
// It does not allow mutation and does not own the underlying memory.
// It is safe to send it to another thread (`Send`) and share it among threads (`Sync`)
// because the underlying memory is guaranteed to be valid for the lifetime `'a`.
unsafe impl Send for PtrByteSlice<'_> {}
// SAFETY: See comment above.
unsafe impl Sync for PtrByteSlice<'_> {}
// SAFETY: `MutPtrByteSlice` is conceptually a mutable view of a byte slice (`&mut [u8]`).
// It enforces exclusive access because it does not implement `Clone` or `Copy`,
// and all mutating methods require `&mut self` or ownership.
// It is safe to send it to another thread (`Send`) because only one thread can possess it
// at a time.
unsafe impl Send for MutPtrByteSlice<'_> {}
// SAFETY: `MutPtrByteSlice` is safe to share among threads (`Sync`) because it does not
// permit safe concurrent mutation through a shared reference (`&self`).
unsafe impl Sync for MutPtrByteSlice<'_> {}

impl<'a> From<&'a [u8]> for PtrByteSlice<'a> {
    fn from(slice: &'a [u8]) -> Self {
        // SAFETY: A standard Rust reference is guaranteed to be valid for reads.
        unsafe { Self::new(slice as *const [u8]) }
    }
}

impl<'a> From<MutPtrByteSlice<'a>> for PtrByteSlice<'a> {
    fn from(slice: MutPtrByteSlice<'a>) -> Self {
        // SAFETY: MutPtrByteSlice guarantees the memory is valid for 'a.
        // Since we consume the MutPtrByteSlice, we can safely return a PtrByteSlice with the same
        // lifetime.
        unsafe { Self::new(slice.slice as *const [u8]) }
    }
}

impl<'a> From<&'a mut [u8]> for MutPtrByteSlice<'a> {
    fn from(slice: &'a mut [u8]) -> Self {
        // SAFETY: A standard Rust mutable reference is guaranteed to be valid and exclusive.
        unsafe { Self::new(slice as *mut [u8]) }
    }
}

/// An iterator over read-only chunks of a pointer slice.
pub struct Chunks<'a, T> {
    ptr: *const T,
    end: *const T,
    _marker: PhantomData<&'a T>,
}

impl<'a, T: Copy + FromBytes> Iterator for Chunks<'a, T> {
    type Item = Chunk<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ptr == self.end {
            None
        } else {
            let current = self.ptr;
            // SAFETY: `self.ptr` is less than `self.end` (checked), so adding 1 is within the
            // bounds of the allocation.
            self.ptr = unsafe { self.ptr.add(1) };
            Some(Chunk { ptr: current, _marker: PhantomData })
        }
    }
}

/// A read-only chunk of a pointer slice.
pub struct Chunk<'a, T> {
    ptr: *const T,
    _marker: PhantomData<&'a T>,
}

impl<T: Copy + FromBytes> Chunk<'_, T> {
    /// Reads the value from the chunk.
    ///
    /// Since alignment and validity were verified once when the iterator was created,
    /// this access is safe and fast.
    pub fn read(&self) -> T {
        // SAFETY: The pointer is guaranteed to be valid and aligned.
        unsafe { std::ptr::read(self.ptr) }
    }
}

/// An iterator over mutable chunks of a pointer slice.
pub struct ChunksMut<'a, T> {
    ptr: *mut T,
    end: *mut T,
    _marker: PhantomData<&'a mut T>,
}

impl<'a, T: Copy + FromBytes> Iterator for ChunksMut<'a, T> {
    type Item = ChunkMut<'a, T>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.ptr == self.end {
            None
        } else {
            let current = self.ptr;
            // SAFETY: `self.ptr` is less than `self.end` (checked), so adding 1 is within the
            // bounds of the allocation.
            self.ptr = unsafe { self.ptr.add(1) };
            Some(ChunkMut { ptr: current, _marker: PhantomData })
        }
    }
}

/// A mutable chunk of a pointer slice.
pub struct ChunkMut<'a, T> {
    ptr: *mut T,
    _marker: PhantomData<&'a mut T>,
}

impl<T: Copy + FromBytes> ChunkMut<'_, T> {
    /// Reads the value from the chunk.
    ///
    /// Since alignment and validity were verified once when the iterator was created,
    /// this access is safe and fast.
    pub fn read(&self) -> T {
        // SAFETY: The pointer is guaranteed to be valid and aligned.
        unsafe { std::ptr::read(self.ptr) }
    }

    /// Writes a value to the chunk.
    ///
    /// Since alignment and validity were verified once when the iterator was created,
    /// this access is safe and fast.
    pub fn write(&self, val: T) {
        // SAFETY: The pointer is guaranteed to be valid and aligned.
        unsafe { std::ptr::write(self.ptr, val) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::IntoBytes;

    #[derive(Copy, Clone, Debug, FromBytes, IntoBytes)]
    #[repr(C, align(4))]
    struct Aligned4(u32);

    #[test]
    fn test_chunks_success() {
        let bytes = [0u8; 16];
        let slice = PtrByteSlice::from(&bytes[..]);
        let chunks = slice.chunks::<Aligned4>();
        assert_eq!(chunks.count(), 4);
    }

    #[test]
    #[should_panic(expected = "Slice is not aligned to T")]
    fn test_chunks_unaligned_panic() {
        #[repr(C, align(4))]
        struct AligningBuffer {
            buffer: [u8; 17],
        }
        let aligned = AligningBuffer { buffer: [0u8; 17] };
        let slice = PtrByteSlice::from(&aligned.buffer[1..17]);
        let _ = slice.chunks::<Aligned4>();
    }

    #[test]
    #[should_panic]
    fn test_chunks_missized_panic() {
        let bytes = [0u8; 15];
        let slice = PtrByteSlice::from(&bytes[..]);
        let _ = slice.chunks::<Aligned4>();
    }

    #[test]
    fn test_chunks_mut_success() {
        let mut bytes = [0u8; 16];
        let mut slice = MutPtrByteSlice::from(&mut bytes[..]);
        let chunks = slice.chunks_mut::<Aligned4>();
        assert_eq!(chunks.count(), 4);
    }

    #[test]
    #[should_panic(expected = "Slice is not aligned to T")]
    fn test_chunks_mut_unaligned_panic() {
        #[repr(C, align(4))]
        struct AligningBuffer {
            buffer: [u8; 17],
        }
        let mut aligned = AligningBuffer { buffer: [0u8; 17] };
        let mut slice = MutPtrByteSlice::from(&mut aligned.buffer[1..17]);
        let _ = slice.chunks_mut::<Aligned4>();
    }

    #[test]
    #[should_panic]
    fn test_chunks_mut_missized_panic() {
        let mut bytes = [0u8; 15];
        let mut slice = MutPtrByteSlice::from(&mut bytes[..]);
        let _ = slice.chunks_mut::<Aligned4>();
    }
}
