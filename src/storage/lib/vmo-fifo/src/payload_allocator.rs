// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use buddy_allocator::BuddyAllocator;

/// Sentinel offset indicating that a reservation or FIFO slot does not own a block in the
/// [`BuddyAllocator`] (used for zero-byte reservations and empty slots).
pub const NO_ALLOCATION: u32 = u32::MAX;

// 63-bit index mask matching `State::INDEX_MASK` in `vmo_fifo`.
const INDEX_MASK: u64 = (1 << 63) - 1;

/// Manages payload allocations in the VMO payload region using a [`BuddyAllocator`] and tracks
/// committed allocations per FIFO slot so they are reclaimed as the consumer advances `read_head`.
pub struct PayloadAllocator {
    allocator: BuddyAllocator,
    queue_mask: u64,
    last_reclaimed_read_index: u64,
    // Physical byte offset in the payload region attached to each FIFO slot, or `NO_ALLOCATION`
    // if no allocation is attached to that slot.
    slot_allocations: Box<[u32]>,
}

impl PayloadAllocator {
    /// Creates a new `PayloadAllocator`.
    ///
    /// * `payload_capacity` - Total size of the payload region in bytes (must be `>= alignment`).
    /// * `queue_capacity` - Power-of-two maximum number of message slots in the `SharedQueue`.
    /// * `alignment` - Power-of-two byte alignment (and minimum buddy block size) for allocations.
    pub fn new(payload_capacity: usize, queue_capacity: usize, alignment: usize) -> Self {
        assert!(alignment > 0 && alignment.is_power_of_two(), "Alignment must be a power of two.");
        assert!(
            queue_capacity > 0 && queue_capacity.is_power_of_two(),
            "Queue capacity must be a power of two."
        );
        // Cap capacity to `NO_ALLOCATION` so `u32::MAX` remains exclusively reserved as the
        // sentinel offset for empty slots and zero-byte reservations.
        let capped_capacity = payload_capacity.min(NO_ALLOCATION as usize);
        let aligned_capacity = capped_capacity & !(alignment - 1);
        Self {
            allocator: BuddyAllocator::new(aligned_capacity, alignment),
            queue_mask: (queue_capacity - 1) as u64,
            last_reclaimed_read_index: 0,
            slot_allocations: vec![NO_ALLOCATION; queue_capacity].into_boxed_slice(),
        }
    }

    /// Returns the total usable byte capacity of the aligned payload region.
    pub fn payload_capacity(&self) -> usize {
        self.allocator.capacity()
    }

    /// Returns the maximum contiguous allocation size in bytes supported by this payload region.
    pub fn max_allocation_size(&self) -> usize {
        self.allocator.max_allocation_size()
    }

    /// Returns `true` if a single allocation of `size` bytes leaving `headroom_bytes` unallocated
    /// can ever be satisfied by this allocator once enough in-flight allocations are freed.
    pub fn is_within_capacity(&self, size: usize, headroom_bytes: usize) -> bool {
        if size == 0 {
            return true;
        }
        if size > self.allocator.max_allocation_size() {
            return false;
        }
        let block_bytes = self.allocator.block_size()
            << buddy_allocator::order(size, self.allocator.block_size());
        block_bytes.saturating_add(headroom_bytes) <= self.allocator.capacity()
    }

    /// Returns `true` if `slot_index` has already been retired (`< last_reclaimed_read_index` in
    /// 63-bit modular index space).
    pub fn is_slot_reclaimed(&self, slot_index: u64) -> bool {
        (slot_index.wrapping_sub(self.last_reclaimed_read_index) & INDEX_MASK) > self.queue_mask
    }

    /// Attempts to allocate `size` bytes of contiguous space in the payload region while leaving
    /// at least `headroom_bytes` unallocated afterwards.
    ///
    /// Returns the physical byte offset (or [`NO_ALLOCATION`] when `size == 0`) on success.
    pub fn allocate(&mut self, size: usize, headroom_bytes: usize) -> Option<u32> {
        if size == 0 {
            return Some(NO_ALLOCATION);
        }
        if size > self.allocator.max_allocation_size() {
            return None;
        }
        if headroom_bytes > 0 {
            let block_bytes = self.allocator.block_size()
                << buddy_allocator::order(size, self.allocator.block_size());
            if self.allocator.free_bytes() < block_bytes.saturating_add(headroom_bytes) {
                return None;
            }
        }
        let (offset, _allocated_len) = self.allocator.allocate(size)?;
        Some(offset as u32)
    }

    /// Associates an allocated block at `offset` with `slot_index` so it will be freed when
    /// [`Self::reclaim_consumed_slots`] advances past `slot_index`.
    pub fn commit_to_slot(&mut self, slot_index: u64, offset: u32) {
        if offset == NO_ALLOCATION {
            return;
        }
        let index = (slot_index & self.queue_mask) as usize;
        debug_assert_eq!(self.slot_allocations[index], NO_ALLOCATION);
        self.slot_allocations[index] = offset;
    }

    /// Immediately frees an uncommitted allocation at `offset`.
    pub fn free(&mut self, offset: u32) {
        if offset == NO_ALLOCATION {
            return;
        }
        self.allocator.free(offset as usize);
    }

    /// Frees payload allocations attached to FIFO slots `< new_read_index`.
    // TODO(https://fxbug.dev/530494057): Add function to decommit memory pages from the VMO when
    // they are reclaimed to avoid holding onto physical RAM while the device is idle.
    pub fn reclaim_consumed_slots(&mut self, new_read_index: u64) {
        let advanced = new_read_index.wrapping_sub(self.last_reclaimed_read_index) & INDEX_MASK;
        if advanced == 0 {
            return;
        }
        // Only `self.slot_allocations.len()` distinct slots exist in the ring; capping `steps`
        // bounds work to `O(queue_capacity)` even when a VMO is pre-seeded to a high starting
        // `read_index` (e.g. near `INDEX_MASK` in wrap-around tests), while ignoring stale/regressed
        // `new_read_index` values whose modular difference wraps into `(INDEX_MASK - queue_mask)..`.
        if advanced > INDEX_MASK - self.queue_mask {
            return;
        }
        let steps = advanced.min(self.slot_allocations.len() as u64);
        let start = new_read_index.wrapping_sub(steps) & INDEX_MASK;
        for step in 0..steps {
            let slot = ((start.wrapping_add(step) & INDEX_MASK) & self.queue_mask) as usize;
            let offset = std::mem::replace(&mut self.slot_allocations[slot], NO_ALLOCATION);
            if offset != NO_ALLOCATION {
                self.allocator.free(offset as usize);
            }
        }
        self.last_reclaimed_read_index = new_read_index;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn free_bytes(allocator: &PayloadAllocator) -> usize {
        allocator.allocator.free_bytes()
    }

    #[test]
    fn test_allocate_sequential_and_non_power_of_two_capacity() {
        // 100 bytes with 8-byte alignment rounds down to 96 bytes (64 + 32) usable capacity,
        // with max single allocation size of 64 bytes.
        let mut allocator = PayloadAllocator::new(100, 8, 8);
        assert_eq!(allocator.payload_capacity(), 96);
        assert_eq!(allocator.max_allocation_size(), 64);
        assert!(allocator.is_within_capacity(64, 0));
        assert!(!allocator.is_within_capacity(65, 0));
        assert!(allocator.is_within_capacity(64, 32));
        assert!(!allocator.is_within_capacity(64, 33));

        let off1 = allocator.allocate(20, 0).unwrap();
        // 20 bytes rounds up to a 32-byte buddy block.
        assert_eq!(free_bytes(&allocator), 64);
        allocator.commit_to_slot(0, off1);

        let off2 = allocator.allocate(32, 0).unwrap();
        assert_ne!(off1, off2);
        assert_eq!(free_bytes(&allocator), 32);
        allocator.commit_to_slot(1, off2);

        allocator.reclaim_consumed_slots(2);
        assert_eq!(free_bytes(&allocator), 96);
    }

    #[test]
    fn test_cancel_allocation() {
        let mut allocator = PayloadAllocator::new(100, 8, 8);
        let off1 = allocator.allocate(50, 0).unwrap();
        assert_eq!(free_bytes(&allocator), 32);

        allocator.free(off1);
        assert_eq!(free_bytes(&allocator), 96);
    }

    #[test]
    fn test_empty_vs_full() {
        let mut allocator = PayloadAllocator::new(128, 8, 64);

        let off1 = allocator.allocate(64, 0).unwrap();
        assert_eq!(free_bytes(&allocator), 64);
        allocator.commit_to_slot(0, off1);

        let off2 = allocator.allocate(64, 0).unwrap();
        assert_eq!(free_bytes(&allocator), 0);
        allocator.commit_to_slot(1, off2);

        assert!(allocator.allocate(10, 0).is_none());

        allocator.reclaim_consumed_slots(1);
        assert_eq!(free_bytes(&allocator), 64);

        allocator.reclaim_consumed_slots(2);
        assert_eq!(free_bytes(&allocator), 128);
    }

    #[test]
    fn test_zero_size_allocation() {
        let mut allocator = PayloadAllocator::new(65536, 16, 4096);
        let mut read_index = 0;

        let off0 = allocator.allocate(8192, 0).unwrap();
        allocator.commit_to_slot(0, off0);
        assert_eq!(free_bytes(&allocator), 65536 - 8192);

        let off1 = allocator.allocate(0, 0).unwrap();
        assert_eq!(off1, NO_ALLOCATION);
        allocator.commit_to_slot(1, off1);
        assert_eq!(free_bytes(&allocator), 65536 - 8192);

        read_index += 1;
        allocator.reclaim_consumed_slots(read_index);
        assert_eq!(free_bytes(&allocator), 65536);

        let off2 = allocator.allocate(4096, 0).unwrap();
        allocator.commit_to_slot(2, off2);

        read_index += 1;
        allocator.reclaim_consumed_slots(read_index);
        assert_eq!(free_bytes(&allocator), 65536 - 4096);

        read_index += 1;
        allocator.reclaim_consumed_slots(read_index);
        assert_eq!(free_bytes(&allocator), 65536);
    }

    #[test]
    fn test_out_of_order_reservations_and_retire() {
        let page = 4096;
        let mut allocator = PayloadAllocator::new(4 * page, 8, page);

        // Two concurrent reservations: A gets [0..8192), B gets [8192..16384).
        let off_a = allocator.allocate(8192, 0).unwrap();
        let off_b = allocator.allocate(8192, 0).unwrap();
        assert_eq!(off_a, 0);
        assert_eq!(off_b, 8192);
        assert!(allocator.allocate(page, 0).is_none());

        // B finishes I/O first and commits to FIFO slot 0.
        allocator.commit_to_slot(0, off_b);

        // Consumer retires slot 0 while A is still writing to [0..8192).
        allocator.reclaim_consumed_slots(1);

        // Only [8192..16384) must be reusable; [0..8192) must remain protected for A.
        let off_c = allocator.allocate(8192, 0).unwrap();
        assert_eq!(off_c, 8192);
        assert!(allocator.allocate(page, 0).is_none());

        // A commits to slot 2 (simulating prefix commits at slots 1 and 2) and C cancels.
        allocator.commit_to_slot(2, off_a);
        allocator.free(off_c);

        assert_eq!(free_bytes(&allocator), 2 * page);
        allocator.reclaim_consumed_slots(2);
        assert_eq!(free_bytes(&allocator), 2 * page);
        allocator.reclaim_consumed_slots(3);
        assert_eq!(free_bytes(&allocator), 4 * page);
    }

    #[test]
    fn test_headroom_reservation_and_buddy_rounding() {
        let page = 4096;
        let mut allocator = PayloadAllocator::new(8 * page, 16, page);

        // Non-power-of-two size (3 pages) rounds up to a 4-page buddy block, so requiring 5 pages
        // of headroom exceeds the 8-page capacity while 4 pages of headroom fits.
        assert!(!allocator.is_within_capacity(3 * page, 5 * page));
        assert!(allocator.is_within_capacity(3 * page, 4 * page));
        assert!(allocator.allocate(3 * page, 5 * page).is_none());

        // With 2 pages of headroom required, an 8-page allocation must be rejected while a
        // 4-page allocation succeeds (leaving 4 pages >= 2 pages of headroom).
        assert!(allocator.allocate(8 * page, 2 * page).is_none());
        let off1 = allocator.allocate(4 * page, 2 * page).unwrap();
        assert_eq!(free_bytes(&allocator), 4 * page);

        // Another 4-page allocation with 2 pages of headroom must fail (would leave 0 pages),
        // while a 2-page allocation with 2 pages of headroom succeeds.
        assert!(allocator.allocate(4 * page, 2 * page).is_none());
        let off2 = allocator.allocate(2 * page, 2 * page).unwrap();
        assert_eq!(free_bytes(&allocator), 2 * page);

        // A high-priority allocation with headroom = 0 can consume the remaining 2 pages.
        let off3 = allocator.allocate(2 * page, 0).unwrap();
        assert_eq!(free_bytes(&allocator), 0);

        allocator.free(off1);
        allocator.free(off2);
        allocator.free(off3);
        assert_eq!(free_bytes(&allocator), 8 * page);
    }

    #[test]
    fn test_slot_wrap_around_and_idempotent_reclaim() {
        let page = 4096;
        let queue_capacity = 4;
        let mut allocator = PayloadAllocator::new(4 * page, queue_capacity, page);

        for slot in 0..(3 * queue_capacity as u64) {
            let off = allocator.allocate(page, 0).unwrap();
            allocator.commit_to_slot(slot, off);
            assert!(!allocator.is_slot_reclaimed(slot));
            allocator.reclaim_consumed_slots(slot + 1);
            assert!(allocator.is_slot_reclaimed(slot));
            // Repeating or regressing `new_read_index` must be a no-op and never double-free.
            allocator.reclaim_consumed_slots(slot + 1);
            allocator.reclaim_consumed_slots(slot);
            assert_eq!(allocator.last_reclaimed_read_index, slot + 1);
            assert_eq!(free_bytes(&allocator), 4 * page);
        }

        // Verify 63-bit index wrap-around near `INDEX_MASK`.
        let start_idx = INDEX_MASK - 1;
        allocator.reclaim_consumed_slots(start_idx);
        assert_eq!(allocator.last_reclaimed_read_index, start_idx);

        let off_a = allocator.allocate(page, 0).unwrap();
        allocator.commit_to_slot(start_idx, off_a);
        assert!(!allocator.is_slot_reclaimed(start_idx));

        let off_b = allocator.allocate(page, 0).unwrap();
        allocator.commit_to_slot(INDEX_MASK, off_b);
        assert!(!allocator.is_slot_reclaimed(INDEX_MASK));

        allocator.reclaim_consumed_slots(0);
        assert!(allocator.is_slot_reclaimed(start_idx));
        assert!(allocator.is_slot_reclaimed(INDEX_MASK));
        assert_eq!(free_bytes(&allocator), 4 * page);
    }
}
