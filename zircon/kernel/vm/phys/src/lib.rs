// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The PMM arena selection algorithm.
//!
//! Here we expose the [`SelectionIterator`] iterator which injests a collection of normalized
//! memalloc ranges as input and yields arena selections.
//!
//! Arena selections are exposed as either a [`SelectionError`] or a [`Selection`].  A selection
//! error is emitted if the algorithm cannot begin an arena selection at a given memalloc range.
//! Otherwise a selection will be emitted with information on where bookkeeping can be placed for
//! the arena.

#![no_std]

use core::iter::Peekable;

use memalloc::NormalizedRangeIterator;

/// The maximum number of wasted/unusable bytes the arena selection process should tolerate.
/// These correspond to the bookkeeping slots used to account for page-sized holes in between
/// actual RAM within an arena.
pub const MAX_WASTED_BYTES: u64 = 0x8_4000; // 528 KiB

/// The number of bytes of bookkeeping data (a `vm_page_t`) which tracks one page of memory.
const BOOKKEEPING_BYTES_PER_PAGE: u64 = 48;

const _: () = {
    assert!(BOOKKEEPING_BYTES_PER_PAGE > 0);
};

/// A physical memory range type with no usage information.
///
/// `Range` is an internal representation of a range of physical memory that can be used to specify
/// an arena and the space for bookkeeping within said arena.
///
/// `addr + size` never overflows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Range {
    base: u64,
    size: u64,
}

impl Range {
    /// Constructs a new range.
    ///
    /// If `addr + size > u64::MAX`, the size is reduced so there is no overflow.
    pub const fn new(base: u64, size: u64) -> Self {
        let end = base.saturating_add(size);
        Self { base, size: end - base }
    }

    /// The start address of a range.
    pub const fn base(&self) -> u64 {
        self.base
    }

    /// The size of the range.
    pub const fn size(&self) -> u64 {
        self.size
    }

    /// The exclusive endpoint of the range.
    pub const fn end(&self) -> u64 {
        self.base() + self.size()
    }
}

/// A PMM arena selection.
///
/// This object specifies the bounds of an arena, the space within said arena wherein bookkeeping
/// is stored, and the number of bytes of bookkeeping which are used to track unusable pages within
/// the arena-- otherwise referred to as the wasted bytes.
///
/// `bookkeeping` must be contained within `arena`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    arena: Range,
    bookkeeping: Range,
    wasted_bytes: u64,
}

impl Selection {
    /// Construct a new arena selection.
    ///
    /// # Panics
    ///
    /// Panics if `bookkeeping` is not a subrange of `arena`.
    pub const fn new(arena: Range, bookkeeping: Range, wasted_bytes: u64) -> Self {
        assert!(bookkeeping.base() >= arena.base());
        assert!(bookkeeping.base() < arena.end());
        assert!(bookkeeping.end() <= arena.end());
        Self { arena, bookkeeping, wasted_bytes }
    }

    /// The bounds of the arena selection.
    pub const fn arena(&self) -> Range {
        self.arena
    }

    /// The bounds of the bookkeeping within the arena selection.
    pub const fn bookkeeping(&self) -> Range {
        self.bookkeeping
    }

    /// The number of wasted bytes within the bookkeeping of the arena selection.
    pub const fn wasted_bytes(&self) -> u64 {
        self.wasted_bytes
    }
}

/// The error that might occur when selecting an arena.
///
/// Arena selections are anchored to the first range within them.  If, starting from this range, no
/// suitable arena can be selected, this error describes why.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SelectionError {
    // No run of free RAM was sufficiently large enough to account for the associated would-be
    // arena.
    NoBookKeepingSpace(memalloc::Range),
    // The range itself was too small, resulting in the aligned arena either being empty or only
    // fitting its bookkeeping (which is not useful).
    TooSmall(memalloc::Range),
}

impl SelectionError {
    /// Returns the memory range where arena selection failed.
    pub const fn range(&self) -> &memalloc::Range {
        match self {
            SelectionError::NoBookKeepingSpace(range) | SelectionError::TooSmall(range) => range,
        }
    }

    /// Convert a [`SelectionError`] to a string slice.
    pub const fn as_str(&self) -> &'static str {
        match self {
            SelectionError::NoBookKeepingSpace(_) => "no bookkeeping space found",
            SelectionError::TooSmall(_) => "too small (less than two pages in size)",
        }
    }
}

/// The arena selection algorithm as an iterator.
///
/// This iterator selects PMM arenas from a provided collection of normalized memory ranges,
/// `ranges`, and identifies suitable bookkeeping space within them.  The bookkeeping is placed to
/// avoid ranges which specify allocations made before PMM initialization.
///
/// An arena can encompass more than one range and might contain 'holes' between ranges, as it may
/// sometimes be preferable to bridge disjoint RAM within the same arena and keep the total amount
/// of arenas low.
///
/// The tolerance for the inclusion of holes is parameterized by `max_wasted_bytes`.
/// `max_wasted_bytes` specifies the number of acceptable bytes of bookkeeping data within an arena
/// which track holes, or otherwise unusable memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectionIterator<Ranges, const PAGE_SIZE: u64> {
    ranges: Ranges,
    max_wasted_bytes: u64,
}

impl<Ranges, const PAGE_SIZE: u64> SelectionIterator<Ranges, PAGE_SIZE>
where
    Ranges: NormalizedRangeIterator + Clone,
{
    /// Construct a selection iterator.
    ///
    /// It is a compile time requirement that `PAGE_SIZE` both be a power of two and greater than
    /// `BOOKKEEPING_BYTES_PER_PAGE`.
    pub fn new(
        ranges: impl IntoIterator<IntoIter = Ranges>,
        max_wasted_bytes: u64,
    ) -> SelectionIterator<Peekable<impl NormalizedRangeIterator + Clone>, PAGE_SIZE> {
        const {
            assert!(PAGE_SIZE.is_power_of_two());
            assert!(BOOKKEEPING_BYTES_PER_PAGE <= PAGE_SIZE);
        }
        SelectionIterator {
            ranges: ranges
                .into_iter()
                .filter_map_ranges(|t| if t.is_ram() { Some(t) } else { None })
                .peekable(),
            max_wasted_bytes,
        }
    }

    /// Rounds `n` up to the nearest multiple of `PAGE_SIZE`, saturating at `u64::MAX`.
    const fn round_up(n: u64) -> u64 {
        match n.checked_add(PAGE_SIZE - 1) {
            Some(val) => val & PAGE_SIZE.wrapping_neg(),
            None => u64::MAX,
        }
    }

    /// Rounds `n` down to the nearest multiple of `PAGE_SIZE`.
    const fn round_down(n: u64) -> u64 {
        n & PAGE_SIZE.wrapping_neg()
    }

    /// Computes the number of bytes of bookkeeping needed to track `arena_size` bytes of memory.
    const fn bookkeeping_size(arena_size: u64) -> u64 {
        Self::round_up((arena_size / PAGE_SIZE) * BOOKKEEPING_BYTES_PER_PAGE)
    }
}

impl<Ranges, const PAGE_SIZE: u64> Iterator for SelectionIterator<Peekable<Ranges>, PAGE_SIZE>
where
    Ranges: NormalizedRangeIterator + Clone,
{
    type Item = Result<Selection, SelectionError>;

    /// Yield the next arena selection.
    ///
    /// Starting at the current range within `self.ranges`, we try to build up an arena.  If we
    /// fail to build it up, we throw away the range and report the error.  Then we try to build an
    /// arena starting at the next range.
    ///
    /// A prospective arena is built up, incorporating new RAM until the wastage limit is exceeded.
    ///
    /// Once a space for bookkeeping is found, we continue until we reach the last range or we find
    /// that incorporating new RAM results in insufficient bookkeeping space or excess wastage.  In
    /// which case we commit what we have and leave the new range to begin the next arena on the
    /// next call to `next()`.
    fn next(&mut self) -> Option<Result<Selection, SelectionError>> {
        // In the event we cannot build up an arena selection starting from `start_of_arena`, then
        // the next call to `next()` needs to start at the next range.  That means all further
        // iteration on the `self.ranges` collection has to be done separately from the
        // `self.ranges` iterator.  `inner_iter` is how we iterate while being able to backtrack in
        // the event of an error.
        let mut inner_iter = self.ranges.clone();
        let start_of_arena = *inner_iter.peek()?;
        self.ranges.next();

        // Prepare a tentative arena selection to be submitted.
        //
        // At this stage, the algorithm has determined that it cannot pack any more ranges into the
        // arena, and it is either time to fail out, or commit what we have.
        let commit_arena = |arena: Range, bookkeeping_space: Range, wasted_bytes: u64| {
            let arena = Range::new(arena.base(), Self::round_down(arena.size()));
            if arena.size() <= PAGE_SIZE {
                // No point of committing the arena if it's empty or would be dedicated
                // entirely to its own bookkeeping.
                return Err(SelectionError::TooSmall(start_of_arena));
            }

            let bookkeeping_end = Self::round_down(bookkeeping_space.end());
            let bookkeeping_size = Self::bookkeeping_size(arena.size());
            let bookkeeping = Range::new(bookkeeping_end - bookkeeping_size, bookkeeping_size);

            Ok(Selection::new(arena, bookkeeping, wasted_bytes))
        };

        // We need to track the first range for error reporting purposes.
        let mut selected: Option<Range> = None;
        let mut bookkeeping_space: Option<Range> = None;
        let mut wasted_bytes: u64 = 0;

        // Now we collect as many ranges as possible until we can't.  `peek()` is used so that we
        // don't advance the `inner_iter` until we decide to incorporate the range.
        while let Some(range) = inner_iter.peek() {
            // Attempt to initialize the arena starting at the provided range, and
            // possibly bookkeeping within it).
            let Some(arena) = selected else {
                debug_assert!(bookkeeping_space.is_none());
                let start = Self::round_up(range.addr());
                let initial_arena = Range::new(start, range.end() - start);
                if range.ty() == memalloc::Type::FreeRam
                    && Self::bookkeeping_size(initial_arena.size()) <= initial_arena.size()
                {
                    bookkeeping_space = Some(initial_arena);
                }
                selected = Some(initial_arena);
                inner_iter.next();
                continue;
            };

            // First check that we aren't exceeding the wastage limit by incorporating the new
            // range.
            let pages_in_hole = Self::round_up(range.addr() - arena.end()) / PAGE_SIZE;
            let waste_from_hole = pages_in_hole * BOOKKEEPING_BYTES_PER_PAGE;
            let new_wasted_bytes = wasted_bytes.saturating_add(waste_from_hole);

            if new_wasted_bytes > self.max_wasted_bytes {
                let Some(bookkeeping_space) = bookkeeping_space else {
                    return Some(Err(SelectionError::NoBookKeepingSpace(start_of_arena)));
                };
                let result = commit_arena(arena, bookkeeping_space, wasted_bytes);
                if result.is_ok() {
                    // No need to backtrack on the next call to `next()`.
                    self.ranges = inner_iter;
                }
                return Some(result);
            }

            // Next, check that there is space for bookkeeping with the new range.  If there is, we
            // store that selected space in `new_bookkeeping_space`.

            // If the old bookkeeping range still works, tentatively use that...
            let new_arena_size = range.end() - arena.base();
            let new_bookkeeping_size_requirement = Self::bookkeeping_size(new_arena_size);
            let mut new_bookkeeping_space =
                bookkeeping_space.filter(|old| old.size() >= new_bookkeeping_size_requirement);

            // ...but if the new range is free RAM range and would allow for more
            // bookkeeping capacity use that instead.
            if range.ty() == memalloc::Type::FreeRam {
                // Begin bookkeeping at a page-aligned boundary, since we might need
                // to reserve the page just before it.
                let start = Self::round_up(range.addr());
                if start < range.end() {
                    let size = range.end() - start;
                    let new_works_better = size >= new_bookkeeping_size_requirement
                        && new_bookkeeping_space.is_none_or(|old| size > old.size());
                    if new_works_better {
                        new_bookkeeping_space = Some(Range::new(start, size));
                    }
                }
            }

            // If we can't find enough space for bookkeeping with the incorporated range, commit
            // what we have.
            if let Some(bookkeeping_space) = bookkeeping_space
                && new_bookkeeping_space.is_none()
            {
                let result = commit_arena(arena, bookkeeping_space, wasted_bytes);
                if result.is_ok() {
                    // No need to backtrack on the next call to `next()`.
                    self.ranges = inner_iter;
                }
                return Some(result);
            }

            // We have determined we can incorporate the range into the arena.
            selected = Some(Range::new(arena.base(), new_arena_size));
            bookkeeping_space = new_bookkeeping_space;
            wasted_bytes = new_wasted_bytes;
            inner_iter.next();
        }

        // We reached the end of the collection of ranges, commit what we have if we can.
        let arena = selected?;
        if let Some(bookkeeping_space) = bookkeeping_space {
            let result = commit_arena(arena, bookkeeping_space, wasted_bytes);
            if result.is_ok() {
                // No need to backtrack on the next call to `next()`.
                self.ranges = inner_iter;
            }
            return Some(result);
        } else {
            return Some(Err(SelectionError::NoBookKeepingSpace(start_of_arena)));
        }
    }
}

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
    const MAX_WASTED_BYTES: u64 = 0x1000;

    /// Round `n` up to a multiple of `a`.
    const fn round_up_to(n: u64, a: u64) -> u64 {
        (n + a - 1) & a.wrapping_neg()
    }

    /// The number of pages of bookkeeping data needed to track `total_pages` pages of memory.
    const fn num_bookkeeping_pages(total_pages: u64) -> u64 {
        let num_bookkeeping_bytes = total_pages * BOOKKEEPING_BYTES_PER_PAGE;
        let num_bookkeeping_bytes_aligned = round_up_to(num_bookkeeping_bytes, PAGE_SIZE);
        num_bookkeeping_bytes_aligned / PAGE_SIZE
    }

    // We can only select arenas from ram type ranges.
    //
    // Input:  [  Peripheral ]         [Peri]
    // Output:
    #[test]
    fn selection_test_no_ram() {
        // No ranges at all.
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([]);
        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);
        assert_eq!(selected_arenas.next(), None);

        // Only peripheral ranges.
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0, 0x5000, memalloc::Type::Peripheral),
            memalloc::Range::new(0x8000, 0x2000, memalloc::Type::Peripheral),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(selected_arenas.next(), None);
    }

    // When assigning bookkeeping pages, we can only select pages in free ram.
    //
    // Input:     [Kern][Z][    FreeRam     ]
    // Output:    [                       [B]
    #[test]
    fn selection_test_bookkeeping_avoids_allocations() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x2000, memalloc::Type::Kernel),
            memalloc::Range::new(0x3000, 0x1000, memalloc::Type::DataZbi),
            memalloc::Range::new(0x4000, 0x6000, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(Range::new(0x1000, 0x9000), Range::new(0x9000, 0x1000), 0,)))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Bookkeeping goes in the largest range of free ram within the arena.
    //
    // Input:     [Free][Kern][   FreeRam   ]   [Free]
    // Output:    [                       [B]        ]
    #[test]
    fn selection_test_bookkeeping_in_largest_free_ram_subrange() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x2000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x3000, 0x2000, memalloc::Type::Kernel),
            memalloc::Range::new(0x5000, 0x5000, memalloc::Type::FreeRam),
            memalloc::Range::new(0xb000, 0x2000, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x1000, 0xc000),
                Range::new(0x9000, 0x1000),
                BOOKKEEPING_BYTES_PER_PAGE,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Arenas are selected from a page-aligned subset of ranges.
    //
    //         [    ][    ][    ][    ][    ]  <-- Page boundaries
    // Input:     [ N ][     FreeRam      ]
    // Output:       [              [B]
    #[test]
    fn selection_test_arenas_and_bookkeeping_are_page_aligned() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0800, 0x0c00, memalloc::Type::Nvram),
            memalloc::Range::new(0x1400, 0x3000, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(Range::new(0x1000, 0x3000), Range::new(0x3000, 0x1000), 0,)))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // A new arena is created when we would otherwise waste more than `max_wasted_bytes` of
    // bookkeeping.
    //
    //
    //          1       85               87                86         2
    //         |-|---------------|-----------------|----------------|----|
    // Input:  [F]               [     FreeRam     ]                [Free]
    //
    //                                           3
    //                                        |----|
    // Output: [                              [Book]                [  [B]
    // Wasted: 4080                                                 0
    #[test]
    fn selection_test_arenas_around_wastage_limit() {
        // We contrive addresses so that there is a gap just within the wastage limit (85 pages)
        // between the first and second ranges, but a gap that would result in
        // excessive waste between the second and third (86 pages).

        const MAX_UNUSABLE_PAGES: u64 = MAX_WASTED_BYTES / BOOKKEEPING_BYTES_PER_PAGE; // 85 pages

        // Calculate range end values in terms of pages.

        const FIRST_START: u64 = 0;
        const FIRST_END: u64 = 1;

        const SECOND_START: u64 = FIRST_END + MAX_UNUSABLE_PAGES; // 86
        const SECOND_END: u64 = SECOND_START + MAX_UNUSABLE_PAGES + 2; // 173

        const THIRD_START: u64 = SECOND_END + MAX_UNUSABLE_PAGES + 1; // 259
        const THIRD_END: u64 = THIRD_START + 2; // 261

        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(
                FIRST_START * PAGE_SIZE,               // addr: 0
                (FIRST_END - FIRST_START) * PAGE_SIZE, // size: 1
                memalloc::Type::FreeRam,
            ),
            memalloc::Range::new(
                SECOND_START * PAGE_SIZE,                // addr: 86
                (SECOND_END - SECOND_START) * PAGE_SIZE, // size: 87
                memalloc::Type::FreeRam,
            ),
            memalloc::Range::new(
                THIRD_START * PAGE_SIZE,               // addr: 259
                (THIRD_END - THIRD_START) * PAGE_SIZE, // size: 2
                memalloc::Type::FreeRam,
            ),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        // By design, we expect the first gap to be included in the first arena, but
        // not the second gap (resulting in a second arena).

        const FIRST_ARENA_BOOKKEEPING_PAGES: u64 = num_bookkeeping_pages(SECOND_END - FIRST_START);
        const SECOND_ARENA_BOOKKEEPING_PAGES: u64 = num_bookkeeping_pages(THIRD_END - THIRD_START);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(
                    FIRST_START * PAGE_SIZE,                // base: 0
                    (SECOND_END - FIRST_START) * PAGE_SIZE, // size: 173
                ),
                Range::new(
                    (SECOND_END - FIRST_ARENA_BOOKKEEPING_PAGES) * PAGE_SIZE, // base: 170
                    FIRST_ARENA_BOOKKEEPING_PAGES * PAGE_SIZE,                // size: 3
                ),
                MAX_UNUSABLE_PAGES * BOOKKEEPING_BYTES_PER_PAGE, // 4080 bytes
            )))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(
                    THIRD_START * PAGE_SIZE,               // base: 259
                    (THIRD_END - THIRD_START) * PAGE_SIZE, // size: 2
                ),
                Range::new(
                    (THIRD_END - SECOND_ARENA_BOOKKEEPING_PAGES) * PAGE_SIZE, // base: 260
                    SECOND_ARENA_BOOKKEEPING_PAGES * PAGE_SIZE,               // size: 1
                ),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // If there is not enough free ram to store bookkeeping data, we yield an error.
    //
    //             1           88             22         22         22
    //            |-|--------------------|----------|----------|----------|
    // Input:     [Z]                    [  Kernel  ]          [ FreeRam  ]
    // Output:    BookErr                [                              [B]
    #[test]
    fn selection_test_error_no_bookkeeping_space() {
        const MAX_UNUSABLE_PAGES: u64 = MAX_WASTED_BYTES / BOOKKEEPING_BYTES_PER_PAGE; // 85 pages
        const HOLE_PAGES: u64 = round_up_to(MAX_UNUSABLE_PAGES + 1, 4); // 88 pages

        // We contrive three ranges such that the gap between the first and the second
        // is too large to bridge within an arena - resulting in us not being able to
        // find bookkeeping for the first - as well as the gap between the second and
        // the third indeed being bridgeable.

        const FIRST_START: u64 = 1;
        const FIRST_END: u64 = 2;

        const SECOND_START: u64 = FIRST_END + HOLE_PAGES; // 90
        const SECOND_END: u64 = SECOND_START + (HOLE_PAGES / 4); // 112

        const THIRD_START: u64 = SECOND_END + (HOLE_PAGES / 4); // 134
        const THIRD_END: u64 = THIRD_START + (HOLE_PAGES / 4); // 156

        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(
                FIRST_START * PAGE_SIZE,               // addr: 1
                (FIRST_END - FIRST_START) * PAGE_SIZE, // size: 1
                memalloc::Type::DataZbi,
            ),
            memalloc::Range::new(
                SECOND_START * PAGE_SIZE,                // addr: 90
                (SECOND_END - SECOND_START) * PAGE_SIZE, // size: 22
                memalloc::Type::Kernel,
            ),
            memalloc::Range::new(
                THIRD_START * PAGE_SIZE,               // addr: 134
                (THIRD_END - THIRD_START) * PAGE_SIZE, // size: 22
                memalloc::Type::FreeRam,
            ),
        ]);

        // The second and third range should comprise the only arena.
        const BOOKKEEPING_PAGES: u64 = num_bookkeeping_pages(THIRD_END - SECOND_START);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::NoBookKeepingSpace(memalloc::Range::new(
                FIRST_START * PAGE_SIZE,               // addr: 1
                (FIRST_END - FIRST_START) * PAGE_SIZE, // size: 1
                memalloc::Type::DataZbi,
            ))))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(
                    SECOND_START * PAGE_SIZE,               // base: 90
                    (THIRD_END - SECOND_START) * PAGE_SIZE, // size: 66
                ),
                Range::new(
                    (THIRD_END - BOOKKEEPING_PAGES) * PAGE_SIZE, // base: 155
                    BOOKKEEPING_PAGES * PAGE_SIZE,               // size: 1
                ),
                (HOLE_PAGES / 4) * BOOKKEEPING_BYTES_PER_PAGE, // 1056 bytes
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Error if less than a page is available.
    #[test]
    fn selection_test_less_than_one_page() {
        let ranges =
            memalloc::testing::NormalizedRangeCheckingIterator::new([memalloc::Range::new(
                0x1000,
                0x0800,
                memalloc::Type::FreeRam,
            )]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::TooSmall(memalloc::Range::new(
                0x1000,
                0x0800,
                memalloc::Type::FreeRam,
            )))),
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Error if less than two pages are available.
    #[test]
    fn selection_test_less_than_two_pages() {
        let ranges =
            memalloc::testing::NormalizedRangeCheckingIterator::new([memalloc::Range::new(
                0x1000,
                0x1800,
                memalloc::Type::FreeRam,
            )]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::TooSmall(memalloc::Range::new(
                0x1000,
                0x1800,
                memalloc::Type::FreeRam,
            )))),
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Error if less than two pages are available after aligning.
    #[test]
    fn selection_test_less_than_two_pages_after_aligning() {
        let ranges =
            memalloc::testing::NormalizedRangeCheckingIterator::new([memalloc::Range::new(
                0x0800,
                0x2000,
                memalloc::Type::FreeRam,
            )]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::TooSmall(memalloc::Range::new(
                0x0800,
                0x2000,
                memalloc::Type::FreeRam,
            ))))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // The following test cases mirror the memory configurations of supported boards and use the
    // production wastage limit (528 KiB) to simulate practical boot outcomes.
    const PROD_MAX_WASTED_BYTES: u64 = 0x8_4000;

    // QemuTcgArm.
    #[test]
    fn selection_test_qemu_tcg_arm() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000_0000, 0x4000_0000, memalloc::Type::Peripheral),
            memalloc::Range::new(
                0x0_4000_0000,
                0x1_3fff_0000 - 0x0_4000_0000,
                memalloc::Type::FreeRam,
            ),
            memalloc::Range::new(
                0x1_3fff_0000,
                0x1_4000_0000 - 0x1_3fff_0000,
                memalloc::Type::Nvram,
            ),
        ]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0x4000_0000, 0x1_4000_0000); bookkeeping @ [0x1_3cff_0000, 0x1_3fff_0000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x4000_0000, 0x1_0000_0000),
                Range::new(0x1_3cff_0000, 0x300_0000),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // QemuTcgX86.
    #[test]
    fn selection_test_qemu_tcg_x86() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(
                0x0_0010_0000,
                0x0_7ffd_f000 - 0x0_0010_0000,
                memalloc::Type::FreeRam,
            ),
            memalloc::Range::new(
                0x1_0000_0000,
                0x2_8000_0000 - 0x1_0000_0000,
                memalloc::Type::FreeRam,
            ),
        ]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0x10_0000, 0x7ffd_f000); bookkeeping @ [0x7e7e_2000, 0x7ffd_f000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x10_0000, 0x7fed_f000),
                Range::new(0x7e7e_2000, 0x17f_d000),
                0,
            )))
        );
        // [0x1_0000_0000, 0x2_8000_0000); bookkeeping @ [0x2_7b80_0000, 0x2_8000_0000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x1_0000_0000, 0x1_8000_0000),
                Range::new(0x2_7b80_0000, 0x480_0000),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // QemuTcgRiscv.
    #[test]
    fn selection_test_qemu_tcg_riscv() {
        let ranges =
            memalloc::testing::NormalizedRangeCheckingIterator::new([memalloc::Range::new(
                0x8004_0000,
                0x2_8000_0000 - 0x8004_0000,
                memalloc::Type::FreeRam,
            )]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0x8004_0000, 0x2_8000_0000); bookkeeping @ [0x2_7a00_0000, 0x2_8000_0000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x8004_0000, 0x1_fffc_0000),
                Range::new(0x2_7a00_0000, 0x600_0000),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Nuc11.
    #[test]
    fn selection_test_nuc11() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0010_0000, 0x3780_4000 - 0x0010_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x417f_f000, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(
                0x1_0000_0000,
                0x4_b080_0000 - 0x1_0000_0000,
                memalloc::Type::FreeRam,
            ),
        ]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0x10_0000, 0x3780_4000); bookkeeping @ [0x36d9_e000, 0x3780_4000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x10_0000, 0x3770_4000),
                Range::new(0x36d9_e000, 0xa6_6000),
                0,
            )))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::TooSmall(memalloc::Range::new(
                0x417f_f000,
                0x1000,
                memalloc::Type::FreeRam,
            ))))
        );
        // [0x1_0000_0000, 0x4_b080_0000); bookkeeping @ [0x4_a56e_8000, 0x4_b080_0000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x1_0000_0000, 0x3_b080_0000),
                Range::new(0x4_a56e_8000, 0xb11_8000),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Vim3.
    #[test]
    fn selection_test_vim3() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000_0000, 0x0500_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0730_0000, 0x0740_0000 - 0x0730_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0750_0000, 0xe000_0000 - 0x0750_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(
                0xfe00_0000,
                0x1_0000_0000 - 0xfe00_0000,
                memalloc::Type::Peripheral,
            ),
        ]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0, 0xe000_0000); bookkeeping @ [0xdd60_0000, 0xe000_0000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0, 0xe000_0000),
                Range::new(0xdd60_0000, 0x2a0_0000),
                0x6_c000,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Astro.
    #[test]
    fn selection_test_astro() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000_0000, 0x0500_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0740_0000, 0x5f7f_e000 - 0x0740_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x5f7f_e000, 0x5f80_0000 - 0x5f7f_e000, memalloc::Type::Nvram),
            memalloc::Range::new(
                0xf580_0000,
                0x1_0000_0000 - 0xf580_0000,
                memalloc::Type::Peripheral,
            ),
        ]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0, 0x5f80_0000); bookkeeping @ [0x5e61_6000, 0x5f7f_e000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0, 0x5f80_0000),
                Range::new(0x5e61_6000, 0x11e_8000),
                0x6_c000,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Sherlock.
    #[test]
    fn selection_test_sherlock() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0010_0000, 0x0500_0000 - 0x0010_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0740_0000, 0x5f7f_e000 - 0x0740_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x5f7f_e000, 0x5f80_0000 - 0x5f7f_e000, memalloc::Type::Nvram),
            memalloc::Range::new(0x5f80_0000, 0x7680_0000 - 0x5f80_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x7700_0000, 0x8000_0000 - 0x7700_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(
                0xf580_0000,
                0x1_0000_0000 - 0xf580_0000,
                memalloc::Type::Peripheral,
            ),
        ]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0x10_0000, 0x8000_0000); bookkeeping @ [0x5e00_1000, 0x5f7f_e000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x10_0000, 0x7ff0_0000),
                Range::new(0x5e00_1000, 0x17f_d000),
                0x8_4000,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Nelson.
    #[test]
    fn selection_test_nelson() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000_0000, 0x0500_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x0740_0000, 0x5f7f_e000 - 0x0740_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x5f7f_e000, 0x5f80_0000 - 0x5f7f_e000, memalloc::Type::Nvram),
            memalloc::Range::new(0x5f80_0000, 0x8000_0000 - 0x5f80_0000, memalloc::Type::FreeRam),
            memalloc::Range::new(
                0xf580_0000,
                0x1_0000_0000 - 0xf580_0000,
                memalloc::Type::Peripheral,
            ),
        ]);

        let mut selected_arenas =
            SelectionIterator::<_, PAGE_SIZE>::new(ranges, PROD_MAX_WASTED_BYTES);

        // [0, 0x8000_0000); bookkeeping @ [0x5dff_e000, 0x5f7f_e000)
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0, 0x8000_0000),
                Range::new(0x5dff_e000, 0x180_0000),
                0x6_c000,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // When an unaligned allocated range ends past a page boundary at the end of a split arena.
    //
    // In this scenario, the arena size is rounded down to page alignment (0x54000), leaving the
    // unaligned tail of the ZBI (up to 0x54f00) outside the arena.
    //
    //         [   ][        ][   ][   ]                 [   ]  <-- Page boundaries
    // Input:  [ F ][ Kernel ][  ZBI  ]    (large hole)  [ F ]
    // Output: [ [B]              ]                      TooSmall
    #[test]
    fn selection_test_split_arena_boundary_issue() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1000, 0x52000, memalloc::Type::Kernel),
            memalloc::Range::new(0x53000, 0x1f00, memalloc::Type::DataZbi),
            memalloc::Range::new(0x20_0000, 0x1000, memalloc::Type::FreeRam), // forces arena split
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(Range::new(0x0, 0x54000), Range::new(0x0, 0x1000), 0,)))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::TooSmall(memalloc::Range::new(
                0x20_0000,
                0x1000,
                memalloc::Type::FreeRam,
            ))))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // TODO(https://fxbug.dev/577163902) evaluate the behavior documented by the tests below to see
    // which behavior we want to preserve or not.

    // The following two tests verify that the arena selection algorithm uses byte length rather
    // than page boundaries to calculate the number of hole pages in an arena (and therefore the
    // wasted bytes).

    // A sub-page hole straddles a page boundary.
    //
    // In this case, the selection algorithm will round the hole up to one page in size, and
    // therefore yield us 48 wasted bytes.
    //
    // We might otherwise expect the algorithm to treat this hole as two pages in size, and instead
    // result in 96 wasted bytes.
    //
    //         [    ][    ][    ][    ][    ][    ]  <-- Page boundaries
    // Input:  [    FreeRam   ]     [   FreeRam   ]
    // Output: [     [Book]                       ]
    // Wasted: 48
    #[test]
    fn selection_test_unaligned_hole_straddling_page_boundary() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x2c00, memalloc::Type::FreeRam),
            memalloc::Range::new(0x3400, 0x2c00, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x0000, 0x6000),
                Range::new(0x1000, 0x1000),
                BOOKKEEPING_BYTES_PER_PAGE,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Multiple sub-page holes within the same page.
    //
    // In this case, the selection algorithm will count each hole as its own page-sized hole,
    // despite being in the same page.
    //
    // We might otherwise expect that the algorithm treats this case as just one page-sized hole.
    //
    //         [         ][         ][         ][         ][         ]  <-- Page boundaries
    // Input:  [        FreeRam        ] [K] [        FreeRam        ]
    // Output: [          [  Book   ]                                ]
    // Wasted: 96
    #[test]
    fn selection_test_multiple_subpage_holes_in_same_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0000, 0x2200, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2400, 0x0400, memalloc::Type::Kernel),
            memalloc::Range::new(0x2a00, 0x2600, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x0000, 0x5000),
                Range::new(0x1000, 0x1000),
                2 * BOOKKEEPING_BYTES_PER_PAGE,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // The following three tests verify that the arena selection algorithm handles the first range
    // differently than the rest, especially if it is a small free ram range.

    // An initial free ram range smaller than a page followed by an allocation.
    //
    // In this case, the selection algorithm records the sub-page initial free ram range as valid
    // bookkeeping for itself (since `bookkeeping_size` for < 1 page is 0), then splits when
    // the subsequent kernel range requires a page of bookkeeping, yielding `TooSmall` on the
    // initial range and starting the arena at the next page boundary.
    //
    // We might otherwise expect the algorithm to recognize that the sub-page initial range cannot
    // hold bookkeeping and continue scanning to find bookkeeping in the later free ram range,
    // selecting a single 5 page arena.
    //
    //         [       ][       ][       ][       ]  <-- Page boundaries
    // Input:  [ F ][   Kernel  ][     FreeRam    ]
    // Output: Err      [                 [ Book  ]
    #[test]
    fn selection_test_subpage_initial_free_ram_bookkeeping() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1800, 0x1800, memalloc::Type::Kernel),
            memalloc::Range::new(0x3000, 0x2000, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::TooSmall(memalloc::Range::new(
                0x1000,
                0x0800,
                memalloc::Type::FreeRam,
            ))))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(Range::new(0x2000, 0x3000), Range::new(0x4000, 0x1000), 0)))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // A sub-page initial range strictly between two page boundaries.
    //
    // In this case, rounding the start of the initial range up to the next page boundary pushes
    // `start` past `range.end()`, causing `range.end() - start` to panic with an underflow.
    //
    // We might otherwise expect the algorithm to yield `TooSmall` on the initial sub-page range and
    // start the next arena at the following page boundary.
    //
    //         [       ][       ][       ][       ][       ]  <-- Page boundaries
    // Input:            [F] [       FreeRam      ]
    // Output:          Panic
    #[test]
    #[should_panic]
    fn selection_test_unaligned_subpage_initial_range() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1200, 0x0600, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1900, 0x2700, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        selected_arenas.next();
    }

    // A leading sub-page free ram range starting on a page boundary before a hole.
    //
    // In this case, the selection algorithm starts the arena at 0x1000 and bridges the hole in
    // [0x1800, 0x2000) to merge with the following free RAM range, prepending the unusable
    // [0x1000, 0x2000) page to the arena and counting 1 page of wasted bytes.
    //
    // We might otherwise expect the algorithm to reject the initial sub-page range with `TooSmall`
    // and start the arena at 0x2000 with 0 wasted bytes.
    //
    //         [    ][    ][    ]  <-- Page boundaries
    // Input:  [F ]  [ FreeRam  ]
    // Output: [           [Book]
    // Wasted: 48
    #[test]
    fn selection_test_leading_subpage_free_ram_before_hole() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x0800, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2000, 0x2000, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x1000, 0x3000),
                Range::new(0x3000, 0x1000),
                BOOKKEEPING_BYTES_PER_PAGE,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // The following four tests verify that the arena selection algorithm does not round down the
    // end of later free ram ranges or require them to contain a full page before merging them into
    // an arena, deferring end alignment until `commit_arena`.

    // An unaligned tail on a later free ram range.
    //
    // Here we have two free ram ranges, both spanning two pages, but with one having a small tail
    // that eeks out past the page boundary.  In this case, the selection algorithm will take into
    // account the extra bytes from this tail when doing its bookkeeping capacity calculations, and
    // will therefore prefer the free ram range with a tail.  This is despite the fact each range
    // only has two usable pages of free ram.
    //
    // We might otherwise expect that the extra bytes beyond the page boundary do not contribute to
    // the calculation of bookkeeping capacity of the range, keeping bookkeeping in the first
    // range.
    //
    //         [    ][    ][    ][    ][    ][    ]  <-- Page boundaries
    // Input:  [ FreeRam  ][Kern][   FreeRam   ]
    // Output: [                       [Book]  ]
    #[test]
    fn selection_test_bookkeeping_ignores_unaligned_tail() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x2000, memalloc::Type::FreeRam), // 2 usable pages
            memalloc::Range::new(0x3000, 0x1000, memalloc::Type::Kernel),
            // 2 usable pages + 0x800 tail.
            memalloc::Range::new(0x4000, 0x2800, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(Range::new(0x1000, 0x5000), Range::new(0x5000, 0x1000), 0)))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // A sub-page free ram range strictly between two page boundaries.
    //
    // In this case, the selection algorithm skips the sub-page range as a bookkeeping candidate
    // (since `round_up(addr) >= end`) without underflowing, and merges it into the arena.
    //
    //         [       ][       ][       ][       ]  <-- Page boundaries
    // Input:  [      FreeRam   ]  [F][  Kernel   ]
    // Output: [         [ Book ]                 ]
    // Wasted: 48
    #[test]
    fn selection_test_unaligned_subpage_free_ram_range() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x2000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x3200, 0x0600, memalloc::Type::FreeRam),
            memalloc::Range::new(0x3800, 0x1800, memalloc::Type::Kernel),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x1000, 0x4000),
                Range::new(0x2000, 0x1000),
                BOOKKEEPING_BYTES_PER_PAGE,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // A trailing sub-page free ram range after a hole.
    //
    // In this case, the selection algorithm bridges the hole to incorporate the trailing sub-page
    // range and then rounds the arena size down in `commit_arena`, leaving the hole page at the
    // end of the committed arena.
    //
    // We might otherwise expect the algorithm not to merge the trailing sub-page range across the
    // hole, ending the first arena at 0x3000 with 0 wasted bytes and yielding `TooSmall` on
    // the trailing sub-page range.
    //
    //         [    ][    ][    ][    ]  <-- Page boundaries
    // Input:  [ FreeRam  ]      [F ]
    // Output: [     [Book]     ]
    // Wasted: 48
    #[test]
    fn selection_test_trailing_subpage_free_ram_after_hole() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x2000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x4000, 0x0800, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x1000, 0x3000),
                Range::new(0x2000, 0x1000),
                BOOKKEEPING_BYTES_PER_PAGE,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // An arena consisting only of 1 page of bookkeeping and 1 unusable hole page.
    //
    // In this case, because the second sub-page range ends on a page boundary, the total arena
    // span is 2 pages, so `commit_arena` commits the arena even though its first page is dedicated
    // to bookkeeping and its second page is rendered unusable by the hole.
    //
    // We might otherwise expect the algorithm to reject both ranges with `TooSmall` since the
    // resulting arena contains 0 usable pages beyond bookkeeping.
    //
    //         [       ][       ]  <-- Page boundaries
    // Input:  [ FreeRam ]   [F ]
    // Output: [  Book  ]       ]
    // Wasted: 48
    #[test]
    fn selection_test_arena_only_bookkeeping_and_hole_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x1000, 0x1400, memalloc::Type::FreeRam),
            memalloc::Range::new(0x2c00, 0x0400, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(0x1000, 0x2000),
                Range::new(0x1000, 0x1000),
                BOOKKEEPING_BYTES_PER_PAGE,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // The following four tests verify that the arena selection algorithm uses a greedy single-pass
    // scan that commits or backtracks as soon as bookkeeping capacity or wastage limits are
    // exceeded, without looking ahead to later free ram ranges.

    // The selection algorithm will properly backtrack if it fails deeper into an iteration.
    //
    // If an arena attempt spans multiple allocated ranges across a bridgeable hole before hitting
    // a second hole that pushes cumulative waste over the limit, the first range yields a
    // `NoBookKeepingSpace` error and selection retries starting at the second range.
    //
    //             1           50             22         50         22
    //            |-|--------------------|----------|----------|----------|
    // Input:     [Z]                    [  Kernel  ]          [ FreeRam  ]
    // Output:    BookErr                [                              [B]
    #[test]
    fn selection_test_error_no_bookkeeping_space_across_multiple_ranges() {
        const MAX_UNUSABLE_PAGES: u64 = MAX_WASTED_BYTES / BOOKKEEPING_BYTES_PER_PAGE; // 85 pages
        // Each hole alone (50 pages) is within the 85-page limit, but together (100 pages) they
        // exceed it.
        const HOLE_PAGES: u64 = (MAX_UNUSABLE_PAGES / 2) + 8; // 50 pages

        const FIRST_START: u64 = 1;
        const FIRST_END: u64 = 2;

        const SECOND_START: u64 = FIRST_END + HOLE_PAGES; // 52
        const SECOND_END: u64 = SECOND_START + 22; // 74

        const THIRD_START: u64 = SECOND_END + HOLE_PAGES; // 124
        const THIRD_END: u64 = THIRD_START + 22; // 146

        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(
                FIRST_START * PAGE_SIZE,               // addr: 1
                (FIRST_END - FIRST_START) * PAGE_SIZE, // size: 1
                memalloc::Type::DataZbi,
            ),
            memalloc::Range::new(
                SECOND_START * PAGE_SIZE,                // addr: 52
                (SECOND_END - SECOND_START) * PAGE_SIZE, // size: 22
                memalloc::Type::Kernel,
            ),
            memalloc::Range::new(
                THIRD_START * PAGE_SIZE,               // addr: 124
                (THIRD_END - THIRD_START) * PAGE_SIZE, // size: 22
                memalloc::Type::FreeRam,
            ),
        ]);

        // The second and third range should comprise the only arena.
        const BOOKKEEPING_PAGES: u64 = num_bookkeeping_pages(THIRD_END - SECOND_START);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::NoBookKeepingSpace(memalloc::Range::new(
                FIRST_START * PAGE_SIZE,               // addr: 1
                (FIRST_END - FIRST_START) * PAGE_SIZE, // size: 1
                memalloc::Type::DataZbi,
            ))))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(
                    SECOND_START * PAGE_SIZE,               // base: 52
                    (THIRD_END - SECOND_START) * PAGE_SIZE, // size: 94
                ),
                Range::new(
                    (THIRD_END - BOOKKEEPING_PAGES) * PAGE_SIZE, // base: 144
                    BOOKKEEPING_PAGES * PAGE_SIZE,               // size: 2
                ),
                HOLE_PAGES * BOOKKEEPING_BYTES_PER_PAGE, // 2400 bytes
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Bookkeeping requirement exceeds capacity.
    //
    // An arena splits when incorporating the next range would require more bookkeeping pages
    // than any free RAM range in the arena can hold.
    //
    //            1         84         1   2
    //           |-|------------------|-|-----|
    // Input:    [F][     Kernel     ][Z][Free]
    // Output:   [B]                 ][     [B]
    #[test]
    fn selection_test_split_arena_on_insufficient_bookkeeping() {
        const MAX_UNUSABLE_PAGES: u64 = PAGE_SIZE / BOOKKEEPING_BYTES_PER_PAGE; // 85 pages

        const FIRST_START: u64 = 0;
        const FIRST_END: u64 = 1;

        const SECOND_START: u64 = FIRST_END; // 1
        const SECOND_END: u64 = MAX_UNUSABLE_PAGES; // 85

        const THIRD_START: u64 = SECOND_END; // 85
        const THIRD_END: u64 = THIRD_START + 1; // 86

        const FOURTH_START: u64 = THIRD_END; // 86
        const FOURTH_END: u64 = FOURTH_START + 2; // 88

        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(
                FIRST_START * PAGE_SIZE,
                (FIRST_END - FIRST_START) * PAGE_SIZE,
                memalloc::Type::FreeRam,
            ),
            memalloc::Range::new(
                SECOND_START * PAGE_SIZE,
                (SECOND_END - SECOND_START) * PAGE_SIZE,
                memalloc::Type::Kernel,
            ),
            memalloc::Range::new(
                THIRD_START * PAGE_SIZE,
                (THIRD_END - THIRD_START) * PAGE_SIZE,
                memalloc::Type::DataZbi,
            ),
            memalloc::Range::new(
                FOURTH_START * PAGE_SIZE,
                (FOURTH_END - FOURTH_START) * PAGE_SIZE,
                memalloc::Type::FreeRam,
            ),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(FIRST_START * PAGE_SIZE, (SECOND_END - FIRST_START) * PAGE_SIZE),
                Range::new(FIRST_START * PAGE_SIZE, PAGE_SIZE),
                0,
            )))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(THIRD_START * PAGE_SIZE, (FOURTH_END - THIRD_START) * PAGE_SIZE),
                Range::new((FOURTH_END - 1) * PAGE_SIZE, PAGE_SIZE),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // Delayed bookkeeping assignment.
    //
    // When the arena grows past what the initial free RAM bookkeeping range can hold, a
    // later larger free RAM range rescues the arena and becomes the new bookkeeping range.
    //
    //            1          84           2
    //           |-|--------------------|----|
    // Input:    [F][     Kernel       ][Free]
    // Output:   [                      [Book]
    #[test]
    fn selection_test_later_free_ram_rescues_outgrown_bookkeeping() {
        const MAX_UNUSABLE_PAGES: u64 = PAGE_SIZE / BOOKKEEPING_BYTES_PER_PAGE; // 85 pages

        const FIRST_START: u64 = 0;
        const FIRST_END: u64 = 1;

        const SECOND_START: u64 = FIRST_END; // 1
        const SECOND_END: u64 = MAX_UNUSABLE_PAGES; // 85

        const THIRD_START: u64 = SECOND_END; // 85
        const THIRD_END: u64 = THIRD_START + 2; // 87 (requires 2 pages of bookkeeping)

        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(
                FIRST_START * PAGE_SIZE,
                (FIRST_END - FIRST_START) * PAGE_SIZE,
                memalloc::Type::FreeRam,
            ),
            memalloc::Range::new(
                SECOND_START * PAGE_SIZE,
                (SECOND_END - SECOND_START) * PAGE_SIZE,
                memalloc::Type::Kernel,
            ),
            memalloc::Range::new(
                THIRD_START * PAGE_SIZE,
                (THIRD_END - THIRD_START) * PAGE_SIZE,
                memalloc::Type::FreeRam,
            ),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(FIRST_START * PAGE_SIZE, (THIRD_END - FIRST_START) * PAGE_SIZE),
                Range::new(THIRD_START * PAGE_SIZE, 2 * PAGE_SIZE),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // A 1-page initial free ram range outgrown by an allocated range.
    //
    // In this case, because the 1-page initial free ram range is recorded as valid bookkeeping for
    // itself, encountering an 85-page kernel range (requiring 2 pages of bookkeeping for the
    // combined 86 pages) triggers an arena split before the later 2-page free ram range can rescue
    // it, and committing the 1-page initial range fails with `TooSmall`.
    //
    // We might otherwise expect the algorithm to continue scanning past the kernel range and use
    // the later 2-page free ram range for bookkeeping across a single `[0, 88 * PAGE_SIZE)` arena.
    //
    //            1          85           2
    //           |-|--------------------|----|
    // Input:    [F][     Kernel       ][Free]
    // Output:   Err[                   [Book]
    #[test]
    fn selection_test_one_page_initial_free_ram_outgrown_by_allocation() {
        const MAX_UNUSABLE_PAGES: u64 = PAGE_SIZE / BOOKKEEPING_BYTES_PER_PAGE; // 85 pages

        const FIRST_START: u64 = 0;
        const FIRST_END: u64 = 1;

        const SECOND_START: u64 = FIRST_END; // 1
        const SECOND_END: u64 = SECOND_START + MAX_UNUSABLE_PAGES;

        const THIRD_START: u64 = SECOND_END; // 86
        const THIRD_END: u64 = THIRD_START + 2; // 88

        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(
                FIRST_START * PAGE_SIZE,
                (FIRST_END - FIRST_START) * PAGE_SIZE,
                memalloc::Type::FreeRam,
            ),
            memalloc::Range::new(
                SECOND_START * PAGE_SIZE,
                (SECOND_END - SECOND_START) * PAGE_SIZE,
                memalloc::Type::Kernel,
            ),
            memalloc::Range::new(
                THIRD_START * PAGE_SIZE,
                (THIRD_END - THIRD_START) * PAGE_SIZE,
                memalloc::Type::FreeRam,
            ),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Err(SelectionError::TooSmall(memalloc::Range::new(
                FIRST_START * PAGE_SIZE,
                (FIRST_END - FIRST_START) * PAGE_SIZE,
                memalloc::Type::FreeRam,
            ))))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(
                Range::new(SECOND_START * PAGE_SIZE, (THIRD_END - SECOND_START) * PAGE_SIZE),
                Range::new(THIRD_START * PAGE_SIZE, 2 * PAGE_SIZE),
                0,
            )))
        );
        assert_eq!(selected_arenas.next(), None);
    }

    // The following test verifies that the arena selection algorithm rounds arena start and end
    // boundaries inward to page alignment even when splitting across unaligned allocated ranges.

    // An arena splits at an unaligned range boundary.
    //
    // In this case, when an arena splits after a range with an unaligned end, the first arena's
    // end is rounded down while the second arena's start is rounded up, leaving the shared page
    // excluded from both arenas.
    //
    // We might otherwise expect the shared page to be covered by one of the two arenas.
    //
    //            1         84.5        1.5    1
    //         |----|----------------|------|-----|
    // Input:  [Free][    Kernel     ][ ZBI ][Free]
    // Output: [Book]              ]    [    [Book]
    #[test]
    fn selection_test_unaligned_split_drops_shared_page() {
        let ranges = memalloc::testing::NormalizedRangeCheckingIterator::new([
            memalloc::Range::new(0x0, 0x1000, memalloc::Type::FreeRam),
            memalloc::Range::new(0x1000, 0x54800, memalloc::Type::Kernel),
            memalloc::Range::new(0x55800, 0x1800, memalloc::Type::DataZbi),
            memalloc::Range::new(0x57000, 0x1000, memalloc::Type::FreeRam),
        ]);

        let mut selected_arenas = SelectionIterator::<_, PAGE_SIZE>::new(ranges, MAX_WASTED_BYTES);

        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(Range::new(0x0, 0x55000), Range::new(0x0, 0x1000), 0)))
        );
        assert_eq!(
            selected_arenas.next(),
            Some(Ok(Selection::new(Range::new(0x56000, 0x2000), Range::new(0x57000, 0x1000), 0)))
        );
        assert_eq!(selected_arenas.next(), None);
    }

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
