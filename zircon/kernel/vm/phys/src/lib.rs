// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The PMM arena selection algorithm.
//!
//! We also expose the [`AlignedAllocationOrHoleIterator`] iterator which emits allocation ranges
//! and holes as reserved ranges aligned to page boundaries.  This iterator is useful for wiring
//! used or unusable pages within an arena.

#![no_std]

use memalloc::NormalizedRangeIterator;

/// The aligned-allocation-or-hole iterator.
///
/// Within a given collection of normalized ranges, `self.ranges`, this iterator will identify each
/// aligned allocation or hole in RAM, yielding these ranges with its `next()` method.
///
/// This routine is of particular interest to the PMM, since we need to wire all such ranges that
/// fall within an arena.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AlignedAllocationOrHoleIterator<Ranges, const PAGE_SIZE: u64> {
    ranges: Ranges,
    prev: Option<memalloc::Range>,
    pending: Option<memalloc::Range>,
}

impl<Ranges, const PAGE_SIZE: u64> AlignedAllocationOrHoleIterator<Ranges, PAGE_SIZE>
where
    Ranges: NormalizedRangeIterator,
{
    /// Construct an aligned-allocation-or-hole iterator.
    ///
    /// It is a compile time requirement that `PAGE_SIZE` be a power of two.
    pub fn new(ranges: impl IntoIterator<IntoIter = Ranges>) -> Self {
        const {
            assert!(PAGE_SIZE.is_power_of_two());
        }
        Self { ranges: ranges.into_iter(), prev: None, pending: None }
    }
}

impl<Ranges, const PAGE_SIZE: u64> Iterator for AlignedAllocationOrHoleIterator<Ranges, PAGE_SIZE>
where
    Ranges: NormalizedRangeIterator,
{
    type Item = memalloc::Range;

    fn next(&mut self) -> Option<memalloc::Range> {
        // We use `self.pending` to store a range to be yielded after a hole.
        if let Some(pending) = self.pending.take() {
            return Some(pending);
        }

        let mut range = self.ranges.next()?;

        // Eagerly align the range out if it is not free RAM.
        if range.ty() != memalloc::Type::FreeRam {
            range = range.align_out(PAGE_SIZE);
        }

        let mut hole_to_yield = None;
        if let Some(prev) = self.prev {
            // If `prev` is an allocation or hole, its aligning may have eaten into `range`; trim
            // accordingly.
            if prev.ty() != memalloc::Type::FreeRam {
                if prev.end() >= range.end() {
                    return self.next();
                }
                if prev.end() > range.addr() {
                    range = memalloc::Range::new(prev.end(), range.end() - prev.end(), range.ty());
                }
            }

            // Check for a hole between `prev` and `range`.
            if prev.end() < range.addr() {
                let hole = memalloc::Range::new(
                    prev.end(),
                    range.addr() - prev.end(),
                    memalloc::Type::Reserved,
                )
                .align_out(PAGE_SIZE);

                // If `range` is free RAM, aligning the hole below it may have eaten into it; trim
                // accordingly.
                if range.ty() == memalloc::Type::FreeRam {
                    if hole.end() >= range.end() {
                        return Some(hole);
                    }
                    if hole.end() > range.addr() {
                        range =
                            memalloc::Range::new(hole.end(), range.end() - hole.end(), range.ty());
                    }
                }
                hole_to_yield = Some(hole);
            }
        }

        let mut range_to_yield = None;
        // We can only yield non free RAM ranges, and all non RAM ranges must be counted as
        // reserved.
        if range.ty() != memalloc::Type::FreeRam {
            if !range.ty().is_ram() {
                range = memalloc::Range::new(range.addr(), range.size(), memalloc::Type::Reserved);
            }
            range_to_yield = Some(range);
        }
        self.prev = Some(range);

        match (hole_to_yield, range_to_yield) {
            (Some(hole), Some(range)) => {
                self.pending = Some(range);
                Some(hole)
            }
            (Some(hole), None) => Some(hole),
            (None, Some(range)) => Some(range),
            (None, None) => self.next(),
        }
    }
}

#[cfg(test)]
mod tests {

    use super::*;

    const PAGE_SIZE: u64 = 0x1000;

    // An empty input yields no holes or allocations.
    #[test]
    fn aligned_allocation_or_holes_test_empty() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(aligned.next(), None);
    }

    // Basic functionality of `AlignedAllocationOrHoleIterator` when all ranges are page aligned.
    //
    // Input:  [Z]   [F][K]
    // Output: [Z][R]   [K]
    #[test]
    fn aligned_allocation_or_holes_test_already_aligned_regions() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x1000, memalloc::Type::DataZbi),
            memalloc::Range::new(0x2000, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x3000, 0x1000, memalloc::Type::Kernel),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::DataZbi))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x1000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x3000, 0x1000, memalloc::Type::Kernel))
        );
        assert_eq!(aligned.next(), None);
    }

    // Holes and free ram sharing a page are aligned out to full-page holes.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [F]   [Free]   [F]
    // Output: [ R  ]      [ R  ]
    #[test]
    fn aligned_allocation_or_holes_test_hole_and_free_ram_in_same_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2800, 0x0800, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x2000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }

    // Allocations and free ram sharing a page are aligned out to full-page allocations.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [F][L][Free][T][F]
    // Output: [ L  ]      [ T  ]
    #[test]
    fn aligned_allocation_or_holes_test_allocation_and_free_ram_in_same_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0800, 0x0800, memalloc::Type::PhysLog),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2000, 0x0800, memalloc::Type::TemporaryPhysHandoff),
            memalloc::Range::new(0x2800, 0x0800, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::PhysLog))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x2000, 0x1000, memalloc::Type::TemporaryPhysHandoff))
        );
        assert_eq!(aligned.next(), None);
    }

    // Holes and allocations sharing a page are aligned out to full-page allocations.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [L]   [Free]   [T]
    // Output: [ L  ]      [ T  ]
    #[test]
    fn aligned_allocation_or_holes_test_hole_and_allocation_in_same_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::PhysLog),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2800, 0x0800, memalloc::Type::TemporaryPhysHandoff),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::PhysLog))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x2000, 0x1000, memalloc::Type::TemporaryPhysHandoff))
        );
        assert_eq!(aligned.next(), None);
    }

    // The following four tests verify how `AlignedAllocationOrHoleIterator` trims or swallows a
    // range when the preceding allocation or hole expands outward into its page.

    // An allocation expands outward and partially trims a multi-page free ram range.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [L][  Free  ]  [F]
    // Output: [ L  ]      [ R  ]
    #[test]
    fn aligned_allocation_or_holes_test_allocation_partially_trims_free_ram() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::PhysLog),
            memalloc::Range::new(0x0800, 0x1800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2800, 0x0800, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::PhysLog))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x2000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }

    // A hole expands outward and partially trims a multi-page free ram range.
    //
    //         [    ][    ]  <-- Page boundaries
    // Input:  [F]  [ Free]
    // Output: [ R  ]
    #[test]
    fn aligned_allocation_or_holes_test_hole_partially_trims_free_ram() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0c00, 0x1400, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }

    // Two allocations share a page and the second allocation extends into the next page.
    //
    // In this case, the first allocation claims the shared page and trims the start of the second
    // allocation to the next page boundary.
    //
    //         [    ][    ]  <-- Page boundaries
    // Input:  [Z][ Kernel]
    // Output: [ Z  ][ K  ]
    #[test]
    fn aligned_allocation_or_holes_test_shared_page_allocation_partially_trims_allocation() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::DataZbi),
            memalloc::Range::new(0x0800, 0x1800, memalloc::Type::Kernel),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::DataZbi))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x1000, 0x1000, memalloc::Type::Kernel))
        );
        assert_eq!(aligned.next(), None);
    }

    // Two sub-page allocations share a single page.
    //
    // In this case, the left allocation expands to fill the page and completely swallows the right
    // allocation.
    //
    //         [    ][    ]  <-- Page boundaries
    // Input:  [Z][K][Free]
    // Output: [ Z  ]
    #[test]
    fn aligned_allocation_or_holes_test_shared_page_allocation_swallows_allocation() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::DataZbi),
            memalloc::Range::new(0x0800, 0x0800, memalloc::Type::Kernel),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::DataZbi))
        );
        assert_eq!(aligned.next(), None);
    }

    // TODO(https://fxbug.dev/577163902) evaluate the behavior documented by the tests below to see
    // which behavior we want to preserve or not.

    // The following four tests verify how `AlignedAllocationOrHoleIterator` behaves when multiple
    // sub-page holes, free ram ranges, and allocations share the same page.

    // Two sub-page holes within the same page separated by sub-page free ram.
    //
    // In this case, when the first aligned hole swallows the middle sub-page free ram range `prev`
    // is not updated. When the next range at 0x1000 is visited, the iterator detects a hole from
    // 0x0200 to 0x1000 and emits [0x0000, 0x1000) a second time.
    //
    // We might otherwise expect the iterator to emit [0x0000, 0x1000) only once.
    //
    //         [       ][       ]  <-- Page boundaries
    // Input:  [F] [F]  [  Free ]
    // Output: [   R   ]
    //         [   R   ]
    #[test]
    fn aligned_allocation_or_holes_test_multiple_holes_in_same_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0200, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0400, 0x0200, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }

    // Two holes where the first is within a single page and the second starts in that same page
    // and extends into the next page.
    //
    // In this case, because `prev` remains the first range after the first hole swallows the
    // second free ram range, the second hole is formed from 0x0200 to 0x1800 and aligns outward to
    // 0x2000, overlapping the already-emitted first page.
    //
    // We might otherwise expect a single two page hole.
    //
    //         [       ][       ]  <-- Page boundaries
    // Input:  [F] [F]      [ F ]
    // Output: [   R   ]
    //         [    Reserved    ]
    #[test]
    fn aligned_allocation_or_holes_test_multiple_holes_in_same_page_second_extends_past_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0200, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0400, 0x0200, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1800, 0x0800, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x2000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }

    // Free ram, a hole, and an allocation all share the same page.
    //
    // In this case, the allocation aligns outward to the start of the page before checking for a
    // hole after `prev`, eating the hole and yielding only the full-page allocation.
    //
    //         [       ][       ]  <-- Page boundaries
    // Input:  [F]  [L ][  Free ]
    // Output: [PhysLog]
    #[test]
    fn aligned_allocation_or_holes_test_free_ram_hole_and_allocation_in_same_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0400, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0800, 0x0800, memalloc::Type::PhysLog),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::PhysLog))
        );
        assert_eq!(aligned.next(), None);
    }

    // Two sub-page free ram ranges (with a hole between them) followed by an allocation in the
    // same page.
    //
    // In this case, the hole is emitted first as [0x0, 0x1000) and swallows the second free ram
    // range without updating `prev`. Because `prev` is still the first free ram range.  The
    // subsequent allocation is not trimmed against the hole and is also emitted as [0x0, 0x1000).
    //
    // We might otherwise expect only a single range for [0x0000, 0x1000) to be emitted, with the
    // allocation taking precedence over the hole.
    //
    //         [        ][        ]  <-- Page boundaries
    // Input:  [F] [F][L][  Free  ]
    // Output: [Reserved]
    //         [PhysLog ]
    #[test]
    fn aligned_allocation_or_holes_test_two_free_ram_ranges_and_allocation_in_same_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0400, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0600, 0x0200, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0800, 0x0800, memalloc::Type::PhysLog),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::PhysLog))
        );
        assert_eq!(aligned.next(), None);
    }

    // The following two tests verify that `AlignedAllocationOrHoleIterator` treats non-ram ranges
    // as `Reserved` holes, regardless of where they appear in the input.

    // A peripheral range between two free ram ranges is yielded as a `Reserved` hole.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [Free][Peri][Free]
    // Output:       [ R  ]
    #[test]
    fn aligned_allocation_or_holes_test_peripheral_between_free_ram() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::Peripheral),
            memalloc::Range::new(0x2000, 0x1000, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x1000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }

    // Leading and trailing peripheral ranges are yielded as `Reserved` holes.
    //
    // In this case, because non-ram ranges are converted to `Reserved` in-loop rather than
    // filtered out beforehand, peripheral ranges before the first ram range and after the last ram
    // range are still emitted as `Reserved` holes.
    //
    // We might otherwise expect holes only to be emitted between ram ranges.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [Peri][Free][Peri]
    // Output: [ R  ]      [ R  ]
    #[test]
    fn aligned_allocation_or_holes_test_leading_and_trailing_peripheral() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Peripheral),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2000, 0x1000, memalloc::Type::Peripheral),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x2000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }

    // A single unaligned free ram range yields no holes.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:     [   Free   ]
    // Output:
    #[test]
    fn aligned_allocation_or_holes_test_single_unaligned_free_ram() {
        let ranges =
            memalloc::testing::NormalizedRangeCheckingIterator::new([memalloc::Range::new(
                0x0800,
                0x1000,
                memalloc::Type::FreeRam,
            )]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(aligned.next(), None);
    }

    // The following two tests verify that `AlignedAllocationOrHoleIterator` yields each aligned
    // allocation or hole immediately without coalescing adjacent ranges of the same type.

    // Two allocations of the same type separated by sub-page free ram.
    //
    // In this case, the first allocation aligns outward over the sub-page free ram to 0x1000 and
    // is emitted immediately, followed by the second allocation without merging.
    //
    // We might otherwise expect adjacent aligned allocations of the same type to be coalesced into
    // a single twp page range.
    //
    //         [    ][    ]  <-- Page boundaries
    // Input:  [K][F][Kern]
    // Output: [ K  ][Kern]
    #[test]
    fn aligned_allocation_or_holes_test_adjacent_aligned_allocations_not_merged() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::Kernel),
            memalloc::Range::new(0x0800, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1000, 0x1000, memalloc::Type::Kernel),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Kernel))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x1000, 0x1000, memalloc::Type::Kernel))
        );
        assert_eq!(aligned.next(), None);
    }

    // Two holes in adjacent pages separated by sub-page free ram that is eaten by the second hole.
    //
    // In this case each hole is emitted as a standalone hole.
    //
    // We might otherwise expect the two adjacent hole ranges to be coalesced into a single two
    // page range.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [F]   [F]   [Free]
    // Output: [ R  ][ R  ]
    #[test]
    fn aligned_allocation_or_holes_test_adjacent_aligned_holes_not_merged() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1000, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2000, 0x1000, memalloc::Type::FreeRam),
        ]);

        let mut aligned = AlignedAllocationOrHoleIterator::<_, PAGE_SIZE>::new(ranges);

        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x0000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(
            aligned.next(),
            Some(memalloc::Range::new(0x1000, 0x1000, memalloc::Type::Reserved))
        );
        assert_eq!(aligned.next(), None);
    }
}
