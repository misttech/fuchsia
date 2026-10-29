// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! An offset-based buddy allocator.
//!
//! Allocations select the smallest available block order (`block_size * 2^order`) that fits the
//! requested size and, within that order, always prioritize the lowest available offset. This packs
//! allocations of a given order toward the beginning of the managed address range and reuses
//! recently freed lower offsets first.

use std::collections::BTreeMap;
use std::ops::Range;

// Stores a list of free block byte offsets for a given order.
//
// FreeLists are kept sorted in descending order of offset (highest offset at the front, lowest
// offset at the back). This allows `pop()` to remove and return the lowest offset in O(1) time.
type FreeList = Vec<usize>;

/// Returns the smallest order whose block size (`block_size * 2^order`) is at least `size` bytes.
///
/// # Panics
///
/// Panics if `block_size` is zero or not a power of two.
#[inline]
pub fn order(size: usize, block_size: usize) -> usize {
    assert!(block_size.is_power_of_two(), "block_size must be a non-zero power of two");
    if size <= block_size {
        return 0;
    }
    (usize::BITS - ((size - 1) >> block_size.trailing_zeros()).leading_zeros()) as usize
}

// Returns the largest order whose block size (`block_size * 2^order`) is at most `size` bytes.
// `block_size` must be a non-zero power of two and `size >= block_size`.
#[inline]
fn order_fit(size: usize, block_size: usize) -> usize {
    debug_assert!(size >= block_size);
    (size >> block_size.trailing_zeros()).ilog2() as usize
}

// Returns the byte size of a block at `order` (`block_size * 2^order`).
#[inline]
fn size_for_order(order: usize, block_size: usize) -> usize {
    block_size << order
}

// Greedily decomposes `capacity` (a multiple of `block_size`) into decreasing power-of-two blocks
// from offset `0`. When `capacity` is not a power of two, each order receives at most one initial
// block at an even multiple of its block size, and any out-of-bounds buddy beyond `capacity` can
// never exist in `free_lists`, preventing false coalescing across remainder boundaries.
fn initial_free_lists(capacity: usize, block_size: usize) -> Vec<FreeList> {
    if capacity == 0 {
        return Vec::new();
    }
    let max_order = order_fit(capacity, block_size);
    let mut free_lists = vec![FreeList::new(); max_order + 1];
    let mut offset = 0;
    while offset < capacity {
        let ord = order_fit(capacity - offset, block_size);
        let block_bytes = size_for_order(ord, block_size);
        free_lists[ord].push(offset);
        offset += block_bytes;
    }
    free_lists
}

/// A buddy allocator that manages byte offsets within a contiguous region of `[0..capacity)`.
#[derive(Debug)]
pub struct BuddyAllocator {
    block_size: usize,
    capacity: usize,
    allocated_bytes: usize,
    // The index corresponds to the order of free memory blocks in the free list.
    free_lists: Vec<FreeList>,
    // Maps allocated start offsets to their allocated block order (`u8`, as `order < usize::BITS`).
    allocation_map: BTreeMap<usize, u8>,
}

impl BuddyAllocator {
    /// Creates a new `BuddyAllocator` managing `[0 .. size & !(block_size - 1))`.
    ///
    /// # Panics
    ///
    /// Panics if `block_size` is zero or not a power of two.
    pub fn new(size: usize, block_size: usize) -> Self {
        assert!(block_size.is_power_of_two(), "block_size must be a non-zero power of two");
        let capacity = size & !(block_size - 1);
        let free_lists = initial_free_lists(capacity, block_size);
        Self {
            block_size,
            capacity,
            allocated_bytes: 0,
            free_lists,
            allocation_map: BTreeMap::new(),
        }
    }

    /// Returns the minimum allocation granularity (`block_size`, a power of two).
    #[inline]
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Returns the total usable byte capacity managed by this allocator (a multiple of
    /// `block_size`).
    #[inline]
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Returns the largest single allocation size (`block_size * 2^max_order`) that this allocator
    /// can ever satisfy.
    #[inline]
    pub fn max_allocation_size(&self) -> usize {
        if self.free_lists.is_empty() {
            0
        } else {
            size_for_order(self.free_lists.len() - 1, self.block_size)
        }
    }

    /// Returns the total number of bytes currently allocated (including power-of-two rounding).
    #[inline]
    pub fn allocated_bytes(&self) -> usize {
        self.allocated_bytes
    }

    /// Returns the total number of bytes currently unallocated.
    #[inline]
    pub fn free_bytes(&self) -> usize {
        self.capacity - self.allocated_bytes
    }

    /// Attempts to allocate a contiguous region of at least `size` bytes, rounded up to the next
    /// power-of-two multiple of `block_size`.
    ///
    /// Returns `Some((offset, allocated_size))` on success, or `None` if no contiguous buddy block
    /// of the required order is currently available (or if `size > self.max_allocation_size()`).
    pub fn allocate(&mut self, size: usize) -> Option<(usize, usize)> {
        if size > self.max_allocation_size() {
            return None;
        }
        let requested_order = order(size, self.block_size);

        // Pick the smallest possible order with a free entry.
        let mut ord = (requested_order..self.free_lists.len())
            .find(|&idx| !self.free_lists[idx].is_empty())?;

        // Split the free region until it matches `requested_order`. Because `ord` was the smallest
        // non-empty order >= `requested_order`, all lower-order lists `< ord` are guaranteed empty
        // before we push the upper buddy half.
        let offset = self.free_lists[ord].pop().unwrap();
        while ord > requested_order {
            ord -= 1;
            debug_assert!(self.free_lists[ord].is_empty());
            self.free_lists[ord].push(offset + size_for_order(ord, self.block_size));
        }

        let allocated_len = size_for_order(ord, self.block_size);
        self.allocation_map.insert(offset, ord as u8);
        self.allocated_bytes += allocated_len;
        Some((offset, allocated_len))
    }

    /// Frees a previously allocated region starting at `offset`, coalescing with free buddies.
    ///
    /// Returns the allocated power-of-two block size that was freed.
    ///
    /// # Panics
    ///
    /// Panics if `offset` does not correspond to an active allocation.
    pub fn free(&mut self, offset: usize) -> usize {
        let mut ord = self
            .allocation_map
            .remove(&offset)
            .unwrap_or_else(|| panic!("No allocation record found for offset {offset}"))
            as usize;
        let freed_size = size_for_order(ord, self.block_size);
        self.allocated_bytes -= freed_size;

        // Merge as many free buddy slots as we can. Comparator arguments in `binary_search_by`
        // are reversed (`buddy.cmp(probe)`) because `FreeList` is sorted in descending order.
        let mut cur_offset = offset;
        let mut block_bytes = freed_size;
        while ord < self.free_lists.len() - 1 {
            let buddy = cur_offset ^ block_bytes;
            let Ok(idx) = self.free_lists[ord].binary_search_by(|probe| buddy.cmp(probe)) else {
                break;
            };
            self.free_lists[ord].remove(idx);
            cur_offset = std::cmp::min(cur_offset, buddy);
            ord += 1;
            block_bytes <<= 1;
        }

        let idx = match self.free_lists[ord].binary_search_by(|probe| cur_offset.cmp(probe)) {
            Ok(_) => panic!("Unexpectedly found {cur_offset} in free list {ord}"),
            Err(idx) => idx,
        };
        self.free_lists[ord].insert(idx, cur_offset);
        freed_size
    }

    /// Returns an iterator over all currently unallocated byte ranges.
    pub fn free_ranges(&self) -> impl Iterator<Item = Range<usize>> + '_ {
        let block_size = self.block_size;
        self.free_lists.iter().enumerate().flat_map(move |(ord, list)| {
            let len = size_for_order(ord, block_size);
            list.iter().map(move |&offset| offset..(offset + len))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_odd_sized_capacity_decomposition() {
        // 123 == 64 + 32 + 16 + 8 + 2 + 1 (last 1 byte is rounded down for block_size = 2).
        let mut allocator = BuddyAllocator::new(123, 2);
        assert_eq!(allocator.block_size(), 2);
        assert_eq!(allocator.capacity(), 122);
        assert_eq!(allocator.max_allocation_size(), 64);

        let expected = [(64, 0), (32, 64), (16, 96), (8, 112), (2, 120)];
        for (size, expected_offset) in expected {
            assert_eq!(allocator.allocate(size), Some((expected_offset, size)));
        }
        assert_eq!(allocator.allocated_bytes(), 122);
        assert_eq!(allocator.free_bytes(), 0);
        assert_eq!(allocator.allocate(2), None);

        for (size, offset) in expected {
            assert_eq!(allocator.free(offset), size);
        }
        assert_eq!(allocator.allocated_bytes(), 0);
        assert_eq!(allocator.free_bytes(), 122);
    }

    #[fuchsia::test]
    fn test_non_power_of_two_split_and_coalesce_safety() {
        let mut allocator = BuddyAllocator::new(123, 2);
        // Requests above `max_allocation_size` (64) must return `None` even with 122 bytes free.
        assert_eq!(allocator.allocate(65), None);
        assert_eq!(allocator.allocate(122), None);

        // Split all remainder blocks down to 61 order-0 blocks of 2 bytes each.
        let mut offsets = Vec::new();
        while let Some((off, len)) = allocator.allocate(2) {
            assert_eq!(len, 2);
            offsets.push(off);
        }
        assert_eq!(offsets.len(), 61);
        offsets.sort_unstable();
        assert_eq!(offsets, (0..61).map(|i| i * 2).collect::<Vec<_>>());

        // Free in reverse and interleaved order to verify adjacent remainder blocks (e.g. 112..120
        // and 120..122) never falsely coalesce with out-of-bounds buddies.
        for &off in offsets.iter().rev().step_by(2) {
            assert_eq!(allocator.free(off), 2);
        }
        for &off in offsets.iter().rev().skip(1).step_by(2) {
            assert_eq!(allocator.free(off), 2);
        }
        assert_eq!(allocator.allocated_bytes(), 0);

        let expected = [(64, 0), (32, 64), (16, 96), (8, 112), (2, 120)];
        for (size, expected_offset) in expected {
            assert_eq!(allocator.allocate(size), Some((expected_offset, size)));
        }
    }

    #[fuchsia::test]
    fn test_lowest_offset_first_and_coalescing() {
        let mut allocator = BuddyAllocator::new(8192, 512);
        let (off0, len0) = allocator.allocate(1024).unwrap();
        let (off1, len1) = allocator.allocate(1024).unwrap();
        assert_eq!((off0, len0), (0, 1024));
        assert_eq!((off1, len1), (1024, 1024));

        // Freeing the lower offset makes it immediately preferred by the next allocation.
        assert_eq!(allocator.free(off0), 1024);
        assert_eq!(allocator.allocate(1024), Some((0, 1024)));

        // Freeing both coalesces the entire 8192-byte pool back together.
        allocator.free(0);
        allocator.free(off1);
        assert_eq!(allocator.allocate(8192), Some((0, 8192)));
        allocator.free(0);
        assert_eq!(allocator.free_bytes(), 8192);
    }

    #[fuchsia::test]
    fn test_multiple_free_blocks_same_order_lowest_first() {
        let mut allocator = BuddyAllocator::new(8192, 512);
        // Allocate 6 blocks of 1024 bytes at offsets 0, 1024, 2048, 3072, 4096, 5120.
        for i in 0..6 {
            assert_eq!(allocator.allocate(1024), Some((i * 1024, 1024)));
        }

        // Free non-adjacent blocks out of order so none can coalesce with their buddies (1024,
        // 3072, 5120 remain allocated), populating `free_lists[1]` with 3 elements.
        allocator.free(2048);
        allocator.free(0);
        allocator.free(4096);

        // Subsequent allocations must pop in strictly ascending offset order: 0, 2048, 4096.
        assert_eq!(allocator.allocate(1024), Some((0, 1024)));
        assert_eq!(allocator.allocate(1024), Some((2048, 1024)));
        assert_eq!(allocator.allocate(1024), Some((4096, 1024)));

        // Free 4096, 0, 2048 again, then free buddy 1024 so `0` and `1024` coalesce to 2048 while
        // `2048` and `4096` remain in `free_lists[1]`.
        allocator.free(4096);
        allocator.free(0);
        allocator.free(2048);
        allocator.free(1024);

        assert_eq!(allocator.allocate(2048), Some((0, 2048)));
        assert_eq!(allocator.allocate(1024), Some((2048, 1024)));
        assert_eq!(allocator.allocate(1024), Some((4096, 1024)));
    }

    #[fuchsia::test]
    fn test_multi_level_split_and_out_of_order_coalesce() {
        let mut allocator = BuddyAllocator::new(8192, 512);
        for i in 0..16 {
            assert_eq!(allocator.allocate(512), Some((i * 512, 512)));
        }
        assert_eq!(allocator.free_bytes(), 0);

        // Free all even-indexed blocks first (no coalescing possible yet).
        for i in (0..16).step_by(2) {
            assert_eq!(allocator.free(i * 512), 512);
        }
        // Free all odd-indexed blocks in reverse order, triggering multi-level cascading merges.
        for i in (1..16).step_by(2).rev() {
            assert_eq!(allocator.free(i * 512), 512);
        }
        assert_eq!(allocator.free_bytes(), 8192);
        assert_eq!(allocator.allocate(8192), Some((0, 8192)));
    }

    #[fuchsia::test]
    fn test_free_ranges() {
        let mut allocator = BuddyAllocator::new(4096, 512);
        assert_eq!(allocator.free_ranges().collect::<Vec<_>>(), vec![0..4096]);

        let (off0, _) = allocator.allocate(512).unwrap();
        let (off1, _) = allocator.allocate(512).unwrap();
        assert_eq!(off0, 0);
        assert_eq!(off1, 512);
        allocator.free(off0);

        let mut ranges: Vec<Range<usize>> = allocator.free_ranges().collect();
        ranges.sort_by_key(|r| r.start);
        assert_eq!(ranges, vec![0..512, 1024..2048, 2048..4096]);
        assert_eq!(ranges.iter().map(|r| r.len()).sum::<usize>(), allocator.free_bytes());

        allocator.free(off1);
        assert_eq!(allocator.free_ranges().collect::<Vec<_>>(), vec![0..4096]);
    }

    #[fuchsia::test]
    fn test_order_boundaries_and_rounding() {
        assert_eq!(order(0, 512), 0);
        assert_eq!(order(1, 512), 0);
        assert_eq!(order(511, 512), 0);
        assert_eq!(order(512, 512), 0);
        assert_eq!(order(513, 512), 1);
        assert_eq!(order(1024, 512), 1);
        assert_eq!(order(1025, 512), 2);
        assert_eq!(order(1536, 512), 2);
        assert_eq!(order(2048, 512), 2);
        assert_eq!(order(2049, 512), 3);
        assert_eq!(order(usize::MAX, 4096), (usize::BITS - 12) as usize);

        let mut allocator = BuddyAllocator::new(4096, 512);
        assert_eq!(allocator.allocate(0), Some((0, 512)));
        assert_eq!(allocator.allocate(513), Some((1024, 1024)));
        assert_eq!(allocator.allocated_bytes(), 1536);
        assert_eq!(allocator.allocate(usize::MAX), None);
    }

    #[fuchsia::test]
    fn test_zero_or_sub_block_capacity() {
        for size in [0, 1, 4095] {
            let mut allocator = BuddyAllocator::new(size, 4096);
            assert_eq!(allocator.capacity(), 0);
            assert_eq!(allocator.max_allocation_size(), 0);
            assert_eq!(allocator.free_bytes(), 0);
            assert_eq!(allocator.allocate(4096), None);
            assert_eq!(allocator.free_ranges().count(), 0);
        }
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "block_size must be a non-zero power of two")]
    fn test_new_zero_block_size_panics() {
        let _ = BuddyAllocator::new(4096, 0);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "block_size must be a non-zero power of two")]
    fn test_new_non_power_of_two_block_size_panics() {
        let _ = BuddyAllocator::new(4096, 3);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "No allocation record found for offset 0")]
    fn test_free_unallocated_offset_panics() {
        let mut allocator = BuddyAllocator::new(4096, 512);
        allocator.free(0);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "No allocation record found for offset 512")]
    fn test_free_unaligned_interior_offset_panics() {
        let mut allocator = BuddyAllocator::new(4096, 512);
        let _ = allocator.allocate(1024).unwrap();
        allocator.free(512);
    }

    #[fuchsia::test(logging = false)]
    #[should_panic(expected = "No allocation record found for offset 0")]
    fn test_double_free_panics() {
        let mut allocator = BuddyAllocator::new(4096, 512);
        let (off, _) = allocator.allocate(1024).unwrap();
        allocator.free(off);
        allocator.free(off);
    }
}
