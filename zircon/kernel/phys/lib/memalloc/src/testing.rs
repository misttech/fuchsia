// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A utility for test environments.
//!
//! This module exposes the [`NormalizedRangeCheckingIterator`] to assist in constructing a
//! validated iterator over collections of ranges used for testing.

/// An iterator which verifies that the underlying `iter` is a [`NormalizedRangeIterator`].
#[derive(Clone)]
pub struct NormalizedRangeCheckingIterator<I> {
    iter: I,
    prev: Option<crate::Range>,
}

impl<I> NormalizedRangeCheckingIterator<I>
where
    I: Iterator<Item = crate::Range>,
{
    pub fn new(ranges: impl IntoIterator<IntoIter = I>) -> Self {
        Self { iter: ranges.into_iter(), prev: None }
    }
}

impl<I> Iterator for NormalizedRangeCheckingIterator<I>
where
    I: Iterator<Item = crate::Range>,
{
    type Item = crate::Range;

    fn next(&mut self) -> Option<crate::Range> {
        let cur = self.iter.next()?;
        if let Some(prev) = self.prev {
            assert!(prev.end() < cur.addr() || (prev.end() == cur.addr() && prev.ty() != cur.ty()));
        }
        self.prev = Some(cur);
        Some(cur)
    }
}

// Safety: `NormalizedRangeCheckingIterator` will panic if it violates the normalized invariant.
unsafe impl<I> crate::NormalizedRangeIterator for NormalizedRangeCheckingIterator<I> where
    I: Iterator<Item = crate::Range>
{
}

#[cfg(test)]
mod tests {
    use super::*;

    // Panic when the underlying iterator has two adjacent ranges of the same type.
    #[test]
    #[should_panic]
    fn normalized_range_checking_iterator_test_maximally_contiguous() {
        let mut ranges = NormalizedRangeCheckingIterator::new([
            crate::Range::new(0, 0x1000, crate::Type::FreeRam),
            crate::Range::new(0x1000, 0x1000, crate::Type::FreeRam),
        ]);

        ranges.next();
        ranges.next();
    }

    // Panic when underlying iterator is not lexicographically sorted.
    #[test]
    #[should_panic]
    fn normalized_range_checking_iterator_test_lexicographically_sorted() {
        let mut ranges = NormalizedRangeCheckingIterator::new([
            crate::Range::new(0x5000, 0x1000, crate::Type::FreeRam),
            crate::Range::new(0, 0x1000, crate::Type::FreeRam),
        ]);

        ranges.next();
        ranges.next();
    }

    // Panic when underlying iterator has overlapping ranges.
    #[test]
    #[should_panic]
    fn normalized_range_checking_iterator_test_non_overlapping() {
        let mut ranges = NormalizedRangeCheckingIterator::new([
            crate::Range::new(0, 0x1000, crate::Type::FreeRam),
            crate::Range::new(0, 0x1000, crate::Type::FreeRam),
        ]);

        ranges.next();
        ranges.next();
    }
}
