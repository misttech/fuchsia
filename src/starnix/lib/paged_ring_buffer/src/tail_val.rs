// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bitfield::bitfield;
use fuchsia_rcu::RcuDroppable;
use std::sync::atomic::{AtomicU64, Ordering};

bitfield! {
    /// A value of `LocklessRingBuffer::tail_page`: the index of the tail node plus a generation.
    ///
    /// The low `INDEX_BITS` bits hold the node index. The bits above them hold a generation
    /// counter that advances every time the tail moves and on every `enable()` that resets the
    /// ring. A thread that sampled `tail_page`, was delayed while the ring wrapped back around to
    /// the same node, and then compares its sample against `tail_page` again therefore sees a
    /// mismatch instead of mistaking the later tail for its own.
    ///
    /// Linux compares `tail_page` pointers without a generation. Its writers are per-CPU and nest,
    /// so an interrupted writer resumes only after the writers nested inside it finish. Writers
    /// here are threads that can stay preempted while other threads move the tail around the
    /// whole ring.
    ///
    /// 20 index bits allow 2^20 nodes, which is 4 GiB of 4 KiB pages per ring. `new()` rejects
    /// larger rings.
    ///
    /// The 44-bit generation wraps after 2^44 advances. Every tail move uses up one page of the
    /// ring, and pages are at least 4 KiB, so a wrap takes at least 2^44 * 4 KiB = 64 PiB of
    /// pages. At 2^20 tail moves per second (4 GiB/s of 4 KiB pages) that is 2^24 seconds, about
    /// 194 days. A delayed thread is fooled only if it stays preempted for an exact multiple of
    /// 2^44 advances.
    #[derive(Clone, Copy, PartialEq, Eq)]
    pub(crate) struct TailVal(u64);
    impl Debug;
    u64;
    /// The generation counter. The setter masks its argument to the field, so the generation
    /// wraps to 0 instead of overflowing.
    pub(crate) generation, set_generation: 63, TailVal::INDEX_BITS;
    /// The index of the tail node.
    pub(crate) index, set_index: TailVal::INDEX_BITS - 1, 0;
}

impl TailVal {
    const INDEX_BITS: usize = 20;
    /// The largest number of nodes whose indices fit in the index field.
    pub(crate) const MAX_NODES: usize = 1 << Self::INDEX_BITS;

    /// Generation 0, node 0.
    pub(crate) const INITIAL: TailVal = TailVal(0);

    /// Returns the value that moves the tail to `next_idx` in the next generation.
    pub(crate) fn next(self, next_idx: usize) -> TailVal {
        assert!(next_idx < Self::MAX_NODES, "node index {next_idx} does not fit in TailVal");
        let mut val = self;
        val.set_generation(self.generation().wrapping_add(1));
        val.set_index(next_idx as u64);
        val
    }
}

/// An atomic `TailVal`. It mirrors the `AtomicU64` methods that `tail_page` needs, so the raw
/// encoding never leaks to callers.
#[derive(RcuDroppable)]
pub(crate) struct AtomicTailVal(AtomicU64);

impl AtomicTailVal {
    pub(crate) fn new(val: TailVal) -> Self {
        Self(AtomicU64::new(val.0))
    }

    pub(crate) fn load(&self, order: Ordering) -> TailVal {
        TailVal(self.0.load(order))
    }

    pub(crate) fn store(&self, val: TailVal, order: Ordering) {
        self.0.store(val.0, order)
    }

    pub(crate) fn compare_exchange(
        &self,
        current: TailVal,
        new: TailVal,
        success: Ordering,
        failure: Ordering,
    ) -> Result<TailVal, TailVal> {
        self.0.compare_exchange(current.0, new.0, success, failure).map(TailVal).map_err(TailVal)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const MAX_INDEX: u64 = TailVal::MAX_NODES as u64 - 1;

    #[test]
    fn test_tail_val_next_replaces_index_and_bumps_generation() {
        let val = TailVal::INITIAL.next(5);
        assert_eq!((val.generation(), val.index()), (1, 5));
        let val = val.next(2);
        assert_eq!((val.generation(), val.index()), (2, 2));
        let val = val.next(TailVal::MAX_NODES - 1);
        assert_eq!((val.generation(), val.index()), (3, MAX_INDEX));
    }

    #[test]
    fn test_tail_val_generation_wraps_to_zero() {
        let max_generation = (1 << 44) - 1;
        let val = TailVal(u64::MAX);
        assert_eq!((val.generation(), val.index()), (max_generation, MAX_INDEX));
        let val = val.next(3);
        assert_eq!((val.generation(), val.index()), (0, 3));
    }

    #[test]
    #[should_panic(expected = "does not fit in TailVal")]
    fn test_tail_val_next_rejects_out_of_range_index() {
        let _ = TailVal::INITIAL.next(TailVal::MAX_NODES);
    }
}
