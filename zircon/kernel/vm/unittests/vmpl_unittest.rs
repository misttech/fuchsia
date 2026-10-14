// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// VmPageList tests duplicated from vmpl_unittest.cc.
#[cfg(ktest)]
#[unittest::suite]
mod vmpl_rs {
    use crate::vm::page::VmPagePtr;
    use crate::vm::page_state::{VmPageState, bindings as page_bindings};
    use crate::vm::pmm;
    use crate::vm::vm_page_list::{
        BatchInserter, IntervalHandling, ReferenceValue, VmPageList, VmPageListNode,
        VmPageOrMarker, VmPageSpliceList, ZeroRangeDirtyState,
    };
    use page::SIZE as PAGE_SIZE_USIZE;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_ne, expect_eq, expect_err, expect_false, expect_gt, expect_ok, expect_true,
        unwrap_ok,
    };
    use zx_status::Status;

    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    fn get_pages<const N: usize>() -> [VmPagePtr; N] {
        core::array::from_fn(|_| pmm::alloc_page(0).expect("pmm_alloc_page failed").0)
    }

    fn free_pages<const N: usize>(pages: [VmPagePtr; N]) {
        for page in pages {
            // SAFETY: `page` was allocated by `pmm::alloc_page` in `get_pages`.
            unsafe { pmm::free_page(page) };
        }
    }

    fn add_page(pl: &mut VmPageList, page: VmPagePtr, offset: u64) -> bool {
        let (slot, is_interval) = pl.lookup_or_allocate(offset, IntervalHandling::SplitInterval);
        let Some(slot) = slot else {
            return false;
        };
        if !slot.is_empty() && !slot.is_interval_slot() {
            return false;
        }
        assert!(slot.is_empty() || is_interval);
        *slot = VmPageOrMarker::from_page(page);
        true
    }

    fn add_marker(pl: &mut VmPageList, offset: u64) -> bool {
        let (slot, is_interval) = pl.lookup_or_allocate(offset, IntervalHandling::SplitInterval);
        let Some(slot) = slot else {
            return false;
        };
        if !slot.is_empty() && !slot.is_interval_slot() {
            return false;
        }
        assert!(slot.is_empty() || is_interval);
        *slot = VmPageOrMarker::marker();
        true
    }

    fn add_reference(pl: &mut VmPageList, ref_val: ReferenceValue, offset: u64) -> bool {
        let (slot, is_interval) = pl.lookup_or_allocate(offset, IntervalHandling::SplitInterval);
        let Some(slot) = slot else {
            return false;
        };
        if !slot.is_empty() && !slot.is_interval_slot() {
            return false;
        }
        assert!(slot.is_empty() || is_interval);
        *slot = VmPageOrMarker::from_reference(ref_val);
        true
    }

    const fn test_reference(v: u32) -> u32 {
        v << ReferenceValue::ALIGN_BITS
    }

    /// Basic test that checks adding and removing a page.
    #[test]
    fn vmpl_add_remove_page_test() {
        let mut pl = VmPageList::new();

        let (test_page, _paddr) = unwrap_ok!(pmm::alloc_page(0), "pmm alloc page failed");

        expect_true!(add_page(&mut pl, test_page, 0));

        expect_true!(pl.lookup(0).is_some_and(|p| p.page() == test_page));
        expect_false!(pl.is_empty());
        expect_false!(pl.has_no_page_or_ref());

        let mut removed = pl.remove_content(0);
        expect_true!(removed.is_page());
        let remove_page = removed.release_page();
        expect_true!(test_page == remove_page);
        expect_true!(pl.remove_content(0).is_empty());

        expect_true!(pl.is_empty());
        expect_true!(pl.has_no_page_or_ref());

        // SAFETY: `test_page` was allocated above and removed from `pl`.
        unsafe { pmm::free_page(test_page) };
    }

    /// Basic test of setting and getting markers.
    #[test]
    fn vmpl_basic_marker_test() {
        let mut pl = VmPageList::new();

        expect_true!(pl.is_empty());
        expect_true!(pl.has_no_page_or_ref());

        expect_true!(add_marker(&mut pl, 0));

        expect_true!(pl.lookup(0).expect("lookup marker").is_marker());

        expect_false!(pl.is_empty());
        expect_true!(pl.has_no_page_or_ref());

        let removed = pl.remove_content(0);
        expect_true!(removed.is_marker());

        expect_true!(pl.has_no_page_or_ref());
        expect_true!(pl.is_empty());
    }

    /// Basic test of setting and getting references.
    #[test]
    fn vmpl_basic_reference_test() {
        let mut pl = VmPageList::new();

        expect_true!(pl.is_empty());
        expect_true!(pl.has_no_page_or_ref());

        // The zero ref is valid.
        let ref0 = ReferenceValue::new(0);
        expect_true!(add_reference(&mut pl, ref0, 0));

        expect_false!(pl.is_empty());
        expect_false!(pl.has_no_page_or_ref());

        // A non-zero ref.
        let ref1 = ReferenceValue::new(test_reference(1));
        expect_true!(add_reference(&mut pl, ref1, PAGE_SIZE));

        let mut removed0 = pl.remove_content(0);
        expect_true!(removed0.is_reference());
        expect_eq!(removed0.release_reference().value(), ref0.value());

        expect_false!(pl.is_empty());
        expect_false!(pl.has_no_page_ref_or_marker());

        let mut removed1 = pl.remove_content(PAGE_SIZE);
        expect_true!(removed1.is_reference());
        expect_eq!(removed1.release_reference().value(), ref1.value());

        expect_true!(pl.is_empty());
        expect_true!(pl.has_no_page_ref_or_marker());
    }

    /// Test for freeing a range of pages.
    #[test]
    fn vmpl_free_pages_test() {
        let mut pl = VmPageList::new();
        const COUNT: usize = 3 * VmPageListNode::PAGE_FAN_OUT;

        let test_pages = get_pages::<COUNT>();

        // Install alternating pages and markers.
        for (i, &page) in test_pages.iter().enumerate() {
            expect_true!(add_page(&mut pl, page, (i as u64) * 2 * PAGE_SIZE));
            expect_true!(add_marker(&mut pl, ((i as u64) * 2 + 1) * PAGE_SIZE));
        }

        let mut list = fbl::Vector::<VmPagePtr>::new();
        let res =
            pl.remove_pages(PAGE_SIZE * 2, ((COUNT as u64) - 1) * 2 * PAGE_SIZE, |slot, _off| {
                if slot.is_page() {
                    let p = slot.release_page();
                    list.push_back(p).expect("vector push");
                }
                *slot = VmPageOrMarker::empty();
                Status::NEXT
            });
        expect_ok!(res);

        for &page in &test_pages[1..COUNT - 1] {
            expect_true!(list.contains(&page), "Not in free list");
        }

        for (i, &page) in test_pages.iter().enumerate() {
            let mut remove_page = pl.remove_content((i as u64) * 2 * PAGE_SIZE);
            let remove_marker = pl.remove_content(((i as u64) * 2 + 1) * PAGE_SIZE);
            if i == 0 || i == COUNT - 1 {
                expect_true!(remove_page.is_page());
                expect_true!(remove_marker.is_marker());
                expect_true!(page == remove_page.release_page());
            } else {
                expect_true!(remove_page.is_empty());
                expect_true!(remove_marker.is_empty());
            }
        }

        free_pages(test_pages);
    }

    /// Tests freeing the last page in a list.
    #[test]
    fn vmpl_free_pages_last_page_test() {
        let (page, _paddr) = unwrap_ok!(pmm::alloc_page(0), "pmm alloc page");

        let mut pl = VmPageList::new();
        expect_true!(add_page(&mut pl, page, 0));

        expect_true!(pl.lookup(0).is_some_and(|p| p.page() == page));

        let mut list = fbl::Vector::<VmPagePtr>::new();
        pl.remove_all_content(|mut p| {
            if p.is_page() {
                list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_true!(pl.is_empty());

        expect_eq!(list.len(), 1);
        expect_true!(list[0] == page);

        // SAFETY: `page` was allocated above and removed from `pl`.
        unsafe { pmm::free_page(page) };
    }

    /// Tests allocation and freeing near the u64::MAX boundary.
    #[test]
    fn vmpl_near_last_offset_free() {
        let (page, _paddr) = unwrap_ok!(pmm::alloc_page(0), "pmm alloc page");

        let mut at_least_one = false;
        let mut addr = 0xffff_ffff_fff0_0000u64;
        while addr != 0 {
            let mut pl = VmPageList::new();
            if add_page(&mut pl, page, addr) {
                at_least_one = true;
                expect_true!(pl.lookup(addr).is_some_and(|p| p.page() == page));

                let mut list = fbl::Vector::<VmPagePtr>::new();
                pl.remove_all_content(|mut p| {
                    if p.is_page() {
                        list.push_back(p.release_page()).expect("vector push");
                    }
                });

                expect_eq!(list.len(), 1);
                expect_true!(list[0] == page);
                expect_true!(pl.is_empty());
            }
            addr = addr.wrapping_add(PAGE_SIZE);
        }
        expect_true!(at_least_one);

        let mut pl2 = VmPageList::new();
        expect_true!(
            pl2.lookup_or_allocate(0xffff_ffff_ffff_0000, IntervalHandling::NoIntervals)
                .0
                .is_none()
        );

        // SAFETY: `page` was allocated above and removed from all lists.
        unsafe { pmm::free_page(page) };
    }

    /// Tests for_every_page and for_every_page_in_range traversals.
    #[test]
    fn vmpl_for_every_page_test() {
        let mut list = VmPageList::new();

        const COUNT: usize = 5;
        let test_pages = get_pages::<COUNT>();

        let offsets: [u64; COUNT] = [
            0,
            PAGE_SIZE,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE - PAGE_SIZE,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE + PAGE_SIZE,
        ];

        for (i, &page) in test_pages.iter().enumerate() {
            if i % 2 != 0 {
                expect_true!(add_page(&mut list, page, offsets[i]));
            } else {
                expect_true!(add_marker(&mut list, offsets[i]));
            }
        }

        let mut matched = true;
        let mut idx = 0;
        let res = list.for_every_page(|p, off| {
            if off != offsets[idx] {
                matched = false;
            }
            if idx % 2 != 0 {
                if !p.is_page() || p.page() != test_pages[idx] {
                    matched = false;
                }
            } else if !p.is_marker() {
                matched = false;
            }
            idx += 1;
            Status::NEXT
        });
        expect_ok!(res);
        expect_true!(matched);
        expect_eq!(idx, offsets.len());

        let mut matched_range = true;
        idx = 1;
        let res = list.for_every_page_in_range(offsets[1], offsets[COUNT - 1], |p, off| {
            if off != offsets[idx] {
                matched_range = false;
            }
            if idx % 2 != 0 {
                if !p.is_page() || p.page() != test_pages[idx] {
                    matched_range = false;
                }
            } else if !p.is_marker() {
                matched_range = false;
            }
            idx += 1;
            Status::NEXT
        });
        expect_ok!(res);
        expect_true!(matched_range);
        expect_eq!(idx, offsets.len() - 1);

        list.remove_all_content(|mut p| {
            if p.is_page() {
                let _ = p.release_page();
            }
        });

        free_pages(test_pages);
    }

    fn vmpl_page_gap_iter_test_body(pages: &[Option<VmPagePtr>], count: usize, stop_idx: usize) {
        let mut list = VmPageList::new();
        for (i, page_opt) in pages.iter().enumerate().take(count) {
            if let Some(page) = page_opt {
                assert!(add_page(&mut list, *page, (i as u64) * PAGE_SIZE));
            }
        }

        let idx = core::cell::Cell::new(0usize);
        let s = list.for_every_page_and_gap_in_range(
            0,
            (count as u64) * PAGE_SIZE,
            |p, off| {
                let cur = idx.get();
                if off != (cur as u64) * PAGE_SIZE || !p.is_page() || pages[cur] != Some(p.page()) {
                    return Status::BAD_STATE;
                }
                if cur == stop_idx {
                    return Status::STOP;
                }
                idx.set(cur + 1);
                Status::NEXT
            },
            |gap_start, gap_end| {
                let mut o = gap_start;
                while o < gap_end {
                    let cur = idx.get();
                    if o != (cur as u64) * PAGE_SIZE || pages[cur].is_some() {
                        return Status::BAD_STATE;
                    }
                    if cur == stop_idx {
                        return Status::STOP;
                    }
                    idx.set(cur + 1);
                    o += PAGE_SIZE;
                }
                Status::NEXT
            },
        );
        assert_eq!(s, Ok(()));
        assert_eq!(stop_idx, idx.get());

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        assert!(list.is_empty());
    }

    /// Tests `for_every_page_and_gap_in_range` against all lists of size 4.
    #[test]
    fn vmpl_page_gap_iter_test() {
        const COUNT: usize = 4;
        let pages = get_pages::<COUNT>();

        let mut page_list: [Option<VmPagePtr>; COUNT] = [None; COUNT];
        for i in 0..COUNT {
            for j in 0..(1 << COUNT) {
                for k in 0..COUNT {
                    if (j & (1 << k)) != 0 {
                        page_list[k] = Some(pages[k]);
                    } else {
                        page_list[k] = None;
                    }
                }
                vmpl_page_gap_iter_test_body(&page_list, COUNT, i);
            }
        }

        free_pages(pages);
    }

    /// Tests that stopping early in `for_every_page_and_gap_in_range` skips trailing gaps.
    #[test]
    fn vmpl_skip_last_gap_test() {
        let mut list = VmPageList::new();
        let (test_page, _paddr) = unwrap_ok!(pmm::alloc_page(0), "pmm alloc page");

        expect_true!(add_page(&mut list, test_page, PAGE_SIZE));

        let mut saw_gap_start = 0;
        let mut saw_gap_end = 0;
        let mut gaps_seen = 0;
        let res = list.for_every_page_and_gap_in_range(
            0,
            PAGE_SIZE * 3,
            |_slot, _offset| Status::STOP,
            |gap_start, gap_end| {
                saw_gap_start = gap_start;
                saw_gap_end = gap_end;
                gaps_seen += 1;
                Status::NEXT
            },
        );
        expect_ok!(res);

        // Validate we saw one gap, and it was the correct gap.
        expect_eq!(1, gaps_seen);
        expect_eq!(0, saw_gap_start);
        expect_eq!(PAGE_SIZE, saw_gap_end);

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_true!(list.is_empty());
        // SAFETY: `test_page` was removed from `list`.
        unsafe { pmm::free_page(test_page) };
    }

    /// Tests BatchInserter sequential and out-of-order allocations.
    #[test]
    fn vmpl_batch_inserter_test() {
        let mut pl = VmPageList::new();
        {
            let mut inserter = BatchInserter::new(&mut pl);
            // Sequentially insert pages across 3 nodes (48 pages)
            for i in 0..(VmPageListNode::PAGE_FAN_OUT * 3) {
                let offset = (i as u64) * PAGE_SIZE;
                let slot = inserter.lookup_or_allocate(offset).unwrap();
                expect_true!(slot.is_empty());
                *slot = VmPageOrMarker::marker();
            }
        }

        // Verify all pages are populated
        for i in 0..(VmPageListNode::PAGE_FAN_OUT * 3) {
            let offset = (i as u64) * PAGE_SIZE;
            let slot = pl.lookup(offset).unwrap();
            expect_true!(slot.is_marker());
        }

        pl.remove_all_content(|_| {});

        // Test non-sequential insertions and reset
        {
            let mut inserter = BatchInserter::new(&mut pl);
            let high_offset = VmPageListNode::NODE_SPAN_BYTES * 10;
            let slot = inserter.lookup_or_allocate(high_offset).unwrap();
            expect_true!(slot.is_empty());
            *slot = VmPageOrMarker::marker();

            let slot_low = inserter.lookup_or_allocate(PAGE_SIZE).unwrap();
            expect_true!(slot_low.is_empty());
            *slot_low = VmPageOrMarker::marker();

            inserter.reset();
        }

        pl.remove_all_content(|_| {});
    }

    /// Tests any_pages_or_intervals_in_range and any_owned_pages_or_intervals_in_range.
    #[test]
    fn vmpl_any_pages_in_range_test() {
        let mut pl = VmPageList::new();

        expect_false!(pl.any_pages_or_intervals_in_range(0, PAGE_SIZE));
        *pl.lookup_or_allocate(0, IntervalHandling::NoIntervals).0.unwrap() =
            VmPageOrMarker::marker();
        expect_true!(pl.any_pages_or_intervals_in_range(0, PAGE_SIZE));
        expect_true!(pl.any_owned_pages_or_intervals_in_range(0, PAGE_SIZE));

        pl.remove_all_content(|_| {});
    }

    /// Tests adding a single page zero interval.
    #[test]
    fn vmpl_add_zero_interval_single_page_test() {
        let mut pl = VmPageList::new();

        expect_false!(pl.is_offset_in_zero_interval(0));
        let res = pl.add_zero_interval(0, PAGE_SIZE, ZeroRangeDirtyState::Dirty);
        expect_ok!(res);

        expect_true!(pl.is_offset_in_zero_interval(0));
        expect_false!(pl.is_offset_in_zero_interval(PAGE_SIZE));

        let slot = pl.lookup(0).unwrap();
        expect_true!(slot.is_interval_slot());
        expect_true!(slot.is_interval_zero());
        expect_true!(slot.zero_interval_dirty_state() == ZeroRangeDirtyState::Dirty);

        pl.remove_all_content(|_| {});
    }

    /// Tests adding adjacent zero intervals and automatic range coalescing.
    #[test]
    fn vmpl_add_zero_interval_coalesce_test() {
        let mut pl = VmPageList::new();
        let span = VmPageListNode::NODE_SPAN_BYTES;

        // Add left interval [0, 64 KiB)
        expect_ok!(pl.add_zero_interval(0, span, ZeroRangeDirtyState::Dirty));
        // Add right interval [128 KiB, 192 KiB)
        expect_ok!(pl.add_zero_interval(span * 2, span * 3, ZeroRangeDirtyState::Dirty));

        expect_true!(pl.lookup(0).unwrap().is_interval_start());
        expect_true!(pl.lookup(span - PAGE_SIZE).unwrap().is_interval_end());
        expect_true!(pl.lookup(span * 2).unwrap().is_interval_start());
        expect_true!(pl.lookup(span * 3 - PAGE_SIZE).unwrap().is_interval_end());

        // Add middle bridging interval [64 KiB, 128 KiB)
        expect_ok!(pl.add_zero_interval(span, span * 2, ZeroRangeDirtyState::Dirty));

        // Now intervals should be coalesced into a single interval [0, 192 KiB)
        expect_true!(pl.lookup(0).unwrap().is_interval_start());
        expect_true!(pl.lookup(span * 3 - PAGE_SIZE).unwrap().is_interval_end());
        // The old boundary sentinels should have been cleaned up
        expect_true!(pl.lookup(span - PAGE_SIZE).is_none_or(|p| p.is_empty()));
        expect_true!(pl.lookup(span).is_none_or(|p| p.is_empty()));
        expect_true!(pl.lookup(span * 2 - PAGE_SIZE).is_none_or(|p| p.is_empty()));
        expect_true!(pl.lookup(span * 2).is_none_or(|p| p.is_empty()));

        // Check query methods across the entire coalesced range
        for off in [0, 4096, span, span + 4096, span * 2, span * 3 - PAGE_SIZE] {
            expect_true!(pl.is_offset_in_zero_interval(off));
        }
        expect_false!(pl.is_offset_in_zero_interval(span * 3));

        pl.remove_all_content(|_| {});
    }

    /// Tests multi-node zero interval creation and boundary sentinels.
    #[test]
    fn vmpl_multi_node_zero_interval_test() {
        let mut pl = VmPageList::new();
        let span = VmPageListNode::NODE_SPAN_BYTES;

        // Multi-node interval spanning [0, 128 KiB) (2 nodes)
        expect_ok!(pl.add_zero_interval(0, span * 2, ZeroRangeDirtyState::Dirty));
        let start_off = 0;
        let end_off = span * 2 - PAGE_SIZE;

        let start_slot = pl.lookup(start_off).unwrap();
        expect_true!(start_slot.is_interval_start());
        expect_true!(start_slot.is_interval_zero());

        let end_slot = pl.lookup(end_off).unwrap();
        expect_true!(end_slot.is_interval_end());
        expect_true!(end_slot.is_interval_zero());

        // Offsets within the range are in the interval
        expect_true!(pl.is_offset_in_zero_interval(start_off));
        expect_true!(pl.is_offset_in_zero_interval(span));
        expect_true!(pl.is_offset_in_zero_interval(end_off));
        expect_false!(pl.is_offset_in_zero_interval(span * 2));

        pl.remove_all_content(|_| {});
    }

    /// Tests `for_every_page_and_contiguous_run_in_range`.
    #[test]
    fn vmpl_contiguous_run_test() {
        let mut list = VmPageList::new();

        const COUNT: usize = 6;
        let test_pages = get_pages::<COUNT>();

        const FAN_OUT: u64 = VmPageListNode::PAGE_FAN_OUT as u64;
        // Add test pages, some in the same node, and some in different nodes. This is so that the
        // code below adds pages in new nodes as expected.
        expect_gt!(FAN_OUT, 4);
        // single page, then gap
        expect_true!(add_page(&mut list, test_pages[0], 0));
        // gap in the same node, then two pages
        expect_true!(add_page(&mut list, test_pages[1], 2 * PAGE_SIZE));
        expect_true!(add_page(&mut list, test_pages[2], 3 * PAGE_SIZE));
        // gap moving to the next node, then three pages spanning the node boundary
        expect_true!(add_page(&mut list, test_pages[3], (FAN_OUT * 2 - 1) * PAGE_SIZE));
        expect_true!(add_page(&mut list, test_pages[4], FAN_OUT * 2 * PAGE_SIZE));
        expect_true!(add_page(&mut list, test_pages[5], (FAN_OUT * 2 + 1) * PAGE_SIZE));

        // Perform a basic iteration to see if we can list the ranges correctly.
        let mut range_offsets = [0u64; COUNT];
        let expected_offsets: [u64; COUNT] = [0, 1, 2, 4, FAN_OUT * 2 - 1, FAN_OUT * 2 + 2];
        let mut index = 0usize;
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            FAN_OUT * 3 * PAGE_SIZE,
            |_p, _off| true,
            |_p, _off| Status::NEXT,
            |start, end, is_interval| {
                if is_interval {
                    return Status::BAD_STATE;
                }
                range_offsets[index] = start;
                range_offsets[index + 1] = end;
                index += 2;
                Status::NEXT
            },
        );

        expect_ok!(status);
        expect_eq!(6, index);
        for i in 0..COUNT {
            expect_eq!(expected_offsets[i] * PAGE_SIZE, range_offsets[i]);
        }

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_eq!(6, free_list.len());

        free_pages(test_pages);
    }

    /// Tests that the contiguous run traversal splits runs on the compare function.
    #[test]
    fn vmpl_contiguous_run_compare_test() {
        let mut list = VmPageList::new();

        const COUNT: usize = 5;
        let test_pages = get_pages::<COUNT>();

        // Add 5 consecutive pages. The ranges will be divided up based on the compare function.
        for (i, page) in test_pages.iter().enumerate() {
            expect_true!(add_page(&mut list, *page, (i as u64) * PAGE_SIZE));
        }

        // Random bools to use as results of comparison for each page.
        let compare_results = [false, true, true, false, true];
        let mut page_visited = [false; COUNT];
        // Expected ranges based on the compare function.
        let expected_offsets: [u64; 4] = [1, 3, 4, 5];
        let mut range_offsets = [0u64; 4];
        let mut index = 0usize;

        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE,
            |_p, off| compare_results[(off / PAGE_SIZE) as usize],
            |_p, off| {
                page_visited[(off / PAGE_SIZE) as usize] = true;
                Status::NEXT
            },
            |start, end, is_interval| {
                if is_interval {
                    return Status::BAD_STATE;
                }
                range_offsets[index] = start;
                range_offsets[index + 1] = end;
                index += 2;
                Status::NEXT
            },
        );

        expect_ok!(status);

        for i in 0..COUNT {
            expect_eq!(compare_results[i], page_visited[i]);
        }
        expect_eq!(4, index);
        for i in 0..4 {
            expect_eq!(expected_offsets[i] * PAGE_SIZE, range_offsets[i]);
        }

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_eq!(5, free_list.len());

        free_pages(test_pages);
    }

    /// Tests graceful early termination of `for_every_page_and_contiguous_run_in_range`.
    #[test]
    fn vmpl_contiguous_traversal_end_test() {
        let mut list = VmPageList::new();

        const COUNT: usize = 3;
        let test_pages = get_pages::<COUNT>();

        // Add 3 consecutive pages.
        for (i, page) in test_pages.iter().enumerate() {
            expect_true!(add_page(&mut list, *page, (i as u64) * PAGE_SIZE));
        }

        let mut page_visited = [false; COUNT];
        let mut range_offsets = [0u64; 2];
        let mut index = 0usize;
        // The compare function evaluates to true for all pages, but the traversal ends early due to
        // Status::STOP in the per-page function.
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE,
            |_p, _off| true,
            |_p, off| {
                page_visited[(off / PAGE_SIZE) as usize] = true;
                // Stop the traversal at page 1. This means the last page processed should be page 1
                // and should be included in the contiguous range. Traversal will stop *after* this
                // page.
                if off / PAGE_SIZE < 1 { Status::NEXT } else { Status::STOP }
            },
            |start, end, is_interval| {
                if is_interval {
                    return Status::BAD_STATE;
                }
                range_offsets[index] = start;
                range_offsets[index + 1] = end;
                index += 2;
                Status::NEXT
            },
        );

        expect_ok!(status);
        // Should have visited the first two pages.
        expect_true!(page_visited[0]);
        expect_true!(page_visited[1]);
        expect_false!(page_visited[2]);

        expect_eq!(2, index);
        let expected_offsets: [u64; 2] = [0, 2];
        for i in 0..2 {
            expect_eq!(expected_offsets[i] * PAGE_SIZE, range_offsets[i]);
        }

        // Attempt another traversal. This time it ends early because of Status::STOP in the
        // contiguous range function.
        index = 0;
        page_visited = [false; COUNT];
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE,
            // Include even indexed pages in the range.
            |_p, off| (off / PAGE_SIZE).is_multiple_of(2),
            |_p, off| {
                page_visited[(off / PAGE_SIZE) as usize] = true;
                Status::NEXT
            },
            |start, end, is_interval| {
                if is_interval {
                    return Status::BAD_STATE;
                }
                range_offsets[index] = start;
                range_offsets[index + 1] = end;
                index += 2;
                // End traversal after the first range.
                Status::STOP
            },
        );

        expect_ok!(status);
        // Should only have visited the first page.
        expect_true!(page_visited[0]);
        expect_false!(page_visited[1]);
        expect_false!(page_visited[2]);

        expect_eq!(2, index);
        let expected_offsets: [u64; 2] = [0, 1];
        for i in 0..2 {
            expect_eq!(expected_offsets[i] * PAGE_SIZE, range_offsets[i]);
        }

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_eq!(3, free_list.len());

        free_pages(test_pages);
    }

    /// Tests error propagation out of `for_every_page_and_contiguous_run_in_range`.
    #[test]
    fn vmpl_contiguous_traversal_error_test() {
        let mut list = VmPageList::new();

        const COUNT: usize = 3;
        let test_pages = get_pages::<COUNT>();

        // Add 3 consecutive pages.
        for (i, page) in test_pages.iter().enumerate() {
            expect_true!(add_page(&mut list, *page, (i as u64) * PAGE_SIZE));
        }

        let mut page_visited = [false; COUNT];
        let mut range_offsets = [0u64; 2];
        let mut index = 0usize;
        // The compare function evaluates to true for all pages, but the traversal ends early due to
        // an error returned by the per-page function.
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE,
            |_p, _off| true,
            |_p, off| {
                page_visited[(off / PAGE_SIZE) as usize] = true;
                // Only page 0 returns success.
                if off / PAGE_SIZE < 1 { Status::NEXT } else { Status::BAD_STATE }
            },
            |start, end, is_interval| {
                if is_interval {
                    return Status::BAD_STATE;
                }
                range_offsets[index] = start;
                range_offsets[index + 1] = end;
                index += 2;
                Status::NEXT
            },
        );

        expect_err!(status, Status::BAD_STATE);
        // Should have visited the first two pages.
        expect_true!(page_visited[0]);
        expect_true!(page_visited[1]);
        expect_false!(page_visited[2]);

        expect_eq!(2, index);
        // Should have been able to process the contiguous range till right before the page that
        // failed.
        let expected_offsets: [u64; 2] = [0, 1];
        for i in 0..2 {
            expect_eq!(expected_offsets[i] * PAGE_SIZE, range_offsets[i]);
        }

        // Attempt another traversal. This time it ends early because of an error returned by the
        // contiguous range function.
        index = 0;
        page_visited = [false; COUNT];
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE,
            // Include even indexed pages in the range.
            |_p, off| (off / PAGE_SIZE).is_multiple_of(2),
            |_p, off| {
                page_visited[(off / PAGE_SIZE) as usize] = true;
                Status::NEXT
            },
            |start, end, is_interval| {
                if is_interval {
                    return Status::BAD_STATE;
                }
                range_offsets[index] = start;
                range_offsets[index + 1] = end;
                index += 2;
                // Error after the first range.
                Status::BAD_STATE
            },
        );

        expect_err!(status, Status::BAD_STATE);
        // Should only have visited the first page.
        expect_true!(page_visited[0]);
        expect_false!(page_visited[1]);
        expect_false!(page_visited[2]);

        expect_eq!(2, index);
        let expected_offsets: [u64; 2] = [0, 1];
        for i in 0..2 {
            expect_eq!(expected_offsets[i] * PAGE_SIZE, range_offsets[i]);
        }

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_eq!(3, free_list.len());

        free_pages(test_pages);
    }

    /// Tests iterating over contiguous entries in a page list with a cursor.
    #[test]
    fn vmpl_cursor_test() {
        let mut pl = VmPageList::new();

        const FAN_OUT: u64 = VmPageListNode::PAGE_FAN_OUT as u64;

        // Add some entries to produce some contiguous and non-contiguous nodes.
        const OFF1: u64 = FAN_OUT * 3 + 4;
        const OFF2: u64 = FAN_OUT * 5 + 4;
        const OFF3: u64 = FAN_OUT * 6 + 1;
        const OFF4: u64 = FAN_OUT * 6 + 2;
        const OFF5: u64 = FAN_OUT * 8 + 1;

        expect_true!(add_marker(&mut pl, OFF1 * PAGE_SIZE));
        expect_true!(add_marker(&mut pl, OFF2 * PAGE_SIZE));
        expect_true!(add_marker(&mut pl, OFF3 * PAGE_SIZE));
        expect_true!(add_marker(&mut pl, OFF4 * PAGE_SIZE));
        expect_true!(add_marker(&mut pl, OFF5 * PAGE_SIZE));

        // Looking up offsets that fall completely out of a node should yield an invalid cursor.
        let cursor = pl.lookup_mut_cursor((OFF1 - FAN_OUT) * PAGE_SIZE);
        expect_true!(cursor.current().is_none());
        let cursor = pl.lookup_mut_cursor((OFF1 + FAN_OUT) * PAGE_SIZE);
        expect_true!(cursor.current().is_none());

        // Looking up in a node should yield a cursor, even if nothing at the exact entry.
        let mut cursor = pl.lookup_mut_cursor((OFF1 - 1) * PAGE_SIZE);
        expect_true!(cursor.current().is_some());
        expect_true!(cursor.current().is_some_and(|slot| slot.is_empty()));

        // Cursor should iterate into the marker though.
        cursor.step();
        expect_true!(cursor.current().is_some());
        expect_true!(cursor.current().is_some_and(|slot| slot.is_marker()));

        // Further iteration should terminate at the end of this node, as the next node is not
        // contiguous.
        cursor.step();
        while let Some(is_empty) = cursor.current().map(|slot| slot.is_empty()) {
            expect_true!(is_empty);
            cursor.step();
        }

        // Should be able to iterate across contiguous nodes.
        let mut cursor = pl.lookup_mut_cursor(OFF2 * PAGE_SIZE);
        expect_true!(cursor.current().is_some());
        expect_true!(cursor.current().is_some_and(|slot| slot.is_marker()));
        cursor.step();

        // Iterate to the next marker, which is in a different node, and count the number of items.
        let mut items: u64 = 0;
        expect_ok!(cursor.for_every_contiguous(|page_or_marker| {
            items += 1;
            if page_or_marker.is_marker() { Status::STOP } else { Status::NEXT }
        }));
        expect_eq!(OFF3 - OFF2, items);

        // `for_every_contiguous` will have left us at OFF3 when we stopped, so the next item should
        // be OFF4, which is also a marker.
        cursor.step();
        expect_true!(cursor.current().is_some());
        expect_true!(cursor.current().is_some_and(|slot| slot.is_marker()));

        // Attempting to do this again should fail as the next item is in the next node.
        items = 0;
        cursor.step();
        expect_ok!(cursor.for_every_contiguous(|page_or_marker| {
            items += 1;
            if page_or_marker.is_marker() { Status::STOP } else { Status::NEXT }
        }));
        expect_true!(cursor.current().is_none());
        // Should have iterated the remaining items in a node after OFF4.
        expect_eq!(FAN_OUT - (OFF4 % FAN_OUT) - 1, items);

        pl.remove_all_content(|_| {});
    }

    /// Interval [1, 3] in a single page list node.
    #[test]
    fn vmpl_interval_single_node_test() {
        let mut list = VmPageList::new();

        let expected_start = 1;
        let expected_end = 3;
        let size = VmPageListNode::PAGE_FAN_OUT as u64;
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        let mut start = 0;
        let mut end = 0;
        let res = list.for_every_page(|p, off| {
            if !(p.is_interval_start() || p.is_interval_end()) {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() {
                start = off;
            } else if p.is_interval_end() {
                end = off;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(expected_start * PAGE_SIZE, start);
        expect_eq!(expected_end * PAGE_SIZE, end);

        let expected_gaps = [0, expected_start, expected_end + 1, size];
        let mut gaps = [0u64; 4];
        let mut index = 0;
        start = 0;
        end = 0;
        let res = list.for_every_page_and_gap_in_range(
            0,
            size * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() {
                    start = off;
                } else if p.is_interval_end() {
                    end = off;
                }
                Status::NEXT
            },
            |begin, end_gap| {
                if index < 4 {
                    gaps[index] = begin;
                    gaps[index + 1] = end_gap;
                    index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);

        expect_eq!(expected_start * PAGE_SIZE, start);
        expect_eq!(expected_end * PAGE_SIZE, end);

        expect_eq!(4, index);
        for i in 0..index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests multi-node interval spanning across 3 nodes with unpopulated middle node.
    #[test]
    fn vmpl_interval_multiple_nodes_test() {
        let mut list = VmPageList::new();

        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);

        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        let mut start = 0;
        let mut end = 0;
        let mut valid = true;
        let res = list.for_every_page(|p, off| {
            if !(p.is_interval_start() || p.is_interval_end()) {
                valid = false;
            }
            if !p.is_zero_interval_dirty() {
                valid = false;
            }
            if p.is_interval_start() {
                start = off;
            } else if p.is_interval_end() {
                end = off;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_true!(valid);
        expect_eq!(expected_start * PAGE_SIZE, start);
        expect_eq!(expected_end * PAGE_SIZE, end);

        let expected_gaps = [0, expected_start, expected_end + 1, size];
        let mut gaps = [0u64; 4];
        let mut index = 0;
        start = 0;
        end = 0;
        let res = list.for_every_page_and_gap_in_range(
            0,
            size * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() {
                    start = off;
                } else if p.is_interval_end() {
                    end = off;
                }
                Status::NEXT
            },
            |begin, end_gap| {
                if index < 4 {
                    gaps[index] = begin;
                    gaps[index + 1] = end_gap;
                    index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);

        expect_eq!(expected_start * PAGE_SIZE, start);
        expect_eq!(expected_end * PAGE_SIZE, end);

        expect_eq!(4, index);
        for i in 0..index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests `for_every_page_and_gap_in_range` with partial interval range traversals.
    #[test]
    fn vmpl_interval_traversal_test() {
        let mut list = VmPageList::new();

        // Interval spanning across 3 nodes, with the middle one unpopulated.
        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // End traversal partway into the interval.
        // Should only see the gap before the interval start.
        let expected_gaps = [0, expected_start];
        let mut gaps = [0u64; 2];
        let mut index = 0;
        let mut start = 0;
        let mut end = 0;
        let res = list.for_every_page_and_gap_in_range(
            0,
            (expected_end - 1) * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() {
                    start = off;
                } else if p.is_interval_end() {
                    end = off;
                }
                Status::NEXT
            },
            |begin, end_gap| {
                if index < 2 {
                    gaps[index] = begin;
                    gaps[index + 1] = end_gap;
                    index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);

        expect_eq!(expected_start * PAGE_SIZE, start);
        // We should not have seen the end of the interval.
        expect_eq!(0, end);

        expect_eq!(2, index);
        for i in 0..index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        // Start traversal partway into the interval.
        // Should only see the gap after the interval end.
        let expected_gaps2 = [expected_end + 1, size];
        index = 0;
        start = 0;
        end = 0;
        let res = list.for_every_page_and_gap_in_range(
            (expected_start + 1) * PAGE_SIZE,
            size * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() {
                    start = off;
                } else if p.is_interval_end() {
                    end = off;
                }
                Status::NEXT
            },
            |begin, end_gap| {
                if index < 2 {
                    gaps[index] = begin;
                    gaps[index + 1] = end_gap;
                    index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);

        // We should not have seen the start of the interval.
        expect_eq!(0, start);
        expect_eq!(expected_end * PAGE_SIZE, end);

        expect_eq!(2, index);
        for i in 0..index {
            expect_eq!(expected_gaps2[i] * PAGE_SIZE, gaps[i]);
        }

        // Start traversal partway into the interval, and also end before the interval end.
        // Should not see any gaps or pages either.
        index = 0;
        start = 0;
        end = 0;
        let res = list.for_every_page_and_gap_in_range(
            (expected_start + 1) * PAGE_SIZE,
            (expected_end - 1) * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() {
                    start = off;
                } else if p.is_interval_end() {
                    end = off;
                }
                Status::NEXT
            },
            |begin, end_gap| {
                if index < 2 {
                    gaps[index] = begin;
                    gaps[index + 1] = end_gap;
                    index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);

        expect_eq!(0, start);
        expect_eq!(0, end);
        expect_eq!(0, index);

        list.remove_all_content(|_| {});
    }

    /// Tests adding intervals to the left and right of an existing interval and merging them.
    #[test]
    fn vmpl_interval_merge_test() {
        let mut list = VmPageList::new();

        // Interval [7, 12].
        let expected_start = 7;
        let expected_end = 12;
        let size = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Add intervals to the left and right of the existing interval and verify that they are
        // merged into a single interval.
        let new_expected_start = 3;
        let new_expected_end = 20;
        expect_gt!(size, new_expected_end);
        // Interval [3, 6].
        expect_ok!(list.add_zero_interval(
            new_expected_start * PAGE_SIZE,
            expected_start * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));
        // Interval [13, 20].
        expect_ok!(list.add_zero_interval(
            (expected_end + 1) * PAGE_SIZE,
            (new_expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        let mut start = 0;
        let mut end = 0;
        let res = list.for_every_page(|p, off| {
            if !(p.is_interval_start() || p.is_interval_end()) {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() {
                start = off;
            } else if p.is_interval_end() {
                end = off;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(new_expected_start * PAGE_SIZE, start);
        expect_eq!(new_expected_end * PAGE_SIZE, end);

        let expected_gaps = [0, new_expected_start, new_expected_end + 1, size];
        let mut gaps = [0u64; 4];
        let mut index = 0;
        start = 0;
        end = 0;
        let res = list.for_every_page_and_gap_in_range(
            0,
            size * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() {
                    start = off;
                } else if p.is_interval_end() {
                    end = off;
                }
                Status::NEXT
            },
            |begin, end_gap| {
                if index < 4 {
                    gaps[index] = begin;
                    gaps[index + 1] = end_gap;
                    index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);

        expect_eq!(new_expected_start * PAGE_SIZE, start);
        expect_eq!(new_expected_end * PAGE_SIZE, end);

        expect_eq!(4, index);
        for i in 0..index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Adding a page in the interval should split the interval.
    #[test]
    fn vmpl_interval_add_page_test() {
        let mut list = VmPageList::new();
        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        let (page, _paddr) = unwrap_ok!(pmm::alloc_page(0), "pmm alloc page");
        let page_offset = VmPageListNode::PAGE_FAN_OUT as u64;
        expect_true!(add_page(&mut list, page, page_offset * PAGE_SIZE));

        let expected_intervals = [
            expected_start * PAGE_SIZE,
            (page_offset - 1) * PAGE_SIZE,
            (page_offset + 1) * PAGE_SIZE,
            expected_end * PAGE_SIZE,
        ];
        let mut intervals = [0u64; 4];
        let mut interval_index = 0;
        let mut page_off = 0;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !(p.is_interval_start() || p.is_interval_end() || p.is_page()) {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() {
                if interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
            } else if p.is_interval_end() {
                if interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
            } else if p.is_page() {
                page_off = off;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(4, interval_index);
        expect_true!(expected_intervals == intervals);
        expect_eq!(page_offset * PAGE_SIZE, page_off);

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_eq!(1, free_list.len());

        // SAFETY: `page` was removed from `list`.
        unsafe { pmm::free_page(page) };
    }

    /// 3 page interval such that adding a page in the middle creates two distinct slots.
    #[test]
    fn vmpl_interval_add_page_slots_test() {
        let mut list = VmPageList::new();
        let expected_start = 0;
        let expected_end = 2;
        let size = VmPageListNode::PAGE_FAN_OUT as u64;
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        let (page, _paddr) = unwrap_ok!(pmm::alloc_page(0), "pmm alloc page");
        let page_offset = 1;
        expect_true!(add_page(&mut list, page, page_offset * PAGE_SIZE));

        let expected_intervals = [expected_start * PAGE_SIZE, expected_end * PAGE_SIZE];
        let mut intervals = [0u64; 2];
        let mut interval_index = 0;
        let mut page_off = 0;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !(p.is_interval_slot() || p.is_page()) {
                return Status::BAD_STATE;
            }
            if p.is_interval_slot() {
                intervals[interval_index] = off;
                interval_index += 1;
            } else if p.is_page() {
                page_off = off;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(2, interval_index);
        expect_true!(expected_intervals == intervals);
        expect_eq!(page_offset * PAGE_SIZE, page_off);

        let mut free_list = fbl::Vector::<VmPagePtr>::new();
        list.remove_all_content(|mut p| {
            if p.is_page() {
                free_list.push_back(p.release_page()).expect("vector push");
            }
        });
        expect_eq!(1, free_list.len());

        // SAFETY: `page` was removed from `list`.
        unsafe { pmm::free_page(page) };
    }

    /// Tests adding pages at the start of an interval.
    #[test]
    fn vmpl_interval_add_page_start_test() {
        let mut list = VmPageList::new();
        let expected_start = 0;
        let expected_end = 2;
        let size = VmPageListNode::PAGE_FAN_OUT as u64;
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        let pages = get_pages::<2>();

        // Add a page at the start of the interval.
        expect_true!(add_page(&mut list, pages[0], expected_start * PAGE_SIZE));

        let expected_intervals = [(expected_start + 1) * PAGE_SIZE, expected_end * PAGE_SIZE];
        let mut intervals = [0u64; 2];
        let mut interval_index = 0;
        let mut page_off = size * PAGE_SIZE;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !(p.is_interval_start() || p.is_interval_end() || p.is_page()) {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() {
                if interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
            } else if p.is_interval_end() {
                if interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
            } else if p.is_page() {
                page_off = off;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(2, interval_index);
        expect_true!(expected_intervals == intervals);
        expect_eq!(expected_start * PAGE_SIZE, page_off);

        // Add another page at the start of the new interval.
        expect_true!(add_page(&mut list, pages[1], (expected_start + 1) * PAGE_SIZE));

        let expected_page_offsets = [expected_start * PAGE_SIZE, (expected_start + 1) * PAGE_SIZE];
        let mut page_offsets = [0u64; 2];
        let mut page_index = 0;
        interval_index = 0;
        let mut interval_slot = 0;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !(p.is_interval_slot() || p.is_page()) {
                return Status::BAD_STATE;
            }
            if p.is_interval_slot() {
                interval_slot = off;
                interval_index += 1;
            } else if p.is_page() {
                page_offsets[page_index] = off;
                page_index += 1;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(1, interval_index);
        expect_eq!(expected_end * PAGE_SIZE, interval_slot);
        expect_eq!(2, page_index);
        expect_true!(expected_page_offsets == page_offsets);

        list.remove_all_content(|mut p| {
            if p.is_page() {
                let _ = p.release_page();
            }
        });
        free_pages(pages);
    }

    /// Tests adding pages at the end of an interval.
    #[test]
    fn vmpl_interval_add_page_end_test() {
        let mut list = VmPageList::new();
        let expected_start = 0;
        let expected_end = 2;
        let size = VmPageListNode::PAGE_FAN_OUT as u64;
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        let pages = get_pages::<2>();

        // Add a page at the end of the interval.
        expect_true!(add_page(&mut list, pages[0], expected_end * PAGE_SIZE));

        let expected_intervals = [expected_start * PAGE_SIZE, (expected_end - 1) * PAGE_SIZE];
        let mut intervals = [0u64; 2];
        let mut interval_index = 0;
        let mut page_off = 0;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !(p.is_interval_start() || p.is_interval_end() || p.is_page()) {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() {
                if interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
            } else if p.is_interval_end() {
                if interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
            } else if p.is_page() {
                page_off = off;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(2, interval_index);
        expect_true!(expected_intervals == intervals);
        expect_eq!(expected_end * PAGE_SIZE, page_off);

        // Add another page at the end of the new interval.
        expect_true!(add_page(&mut list, pages[1], (expected_end - 1) * PAGE_SIZE));

        let expected_page_offsets = [(expected_end - 1) * PAGE_SIZE, expected_end * PAGE_SIZE];
        let mut page_offsets = [0u64; 2];
        let mut page_index = 0;
        interval_index = 0;
        let mut interval_slot = 0;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !(p.is_interval_slot() || p.is_page()) {
                return Status::BAD_STATE;
            }
            if p.is_interval_slot() {
                interval_slot = off;
                interval_index += 1;
            } else if p.is_page() {
                page_offsets[page_index] = off;
                page_index += 1;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(1, interval_index);
        expect_eq!(expected_start * PAGE_SIZE, interval_slot);
        expect_eq!(2, page_index);
        expect_true!(expected_page_offsets == page_offsets);

        list.remove_all_content(|mut p| {
            if p.is_page() {
                let _ = p.release_page();
            }
        });
        free_pages(pages);
    }

    /// Tests replacing a single-page interval slot with a page.
    #[test]
    fn vmpl_interval_replace_slot_test() {
        let mut list = VmPageList::new();
        let expected_interval = 0;
        let size = VmPageListNode::PAGE_FAN_OUT as u64;
        expect_ok!(list.add_zero_interval(
            expected_interval * PAGE_SIZE,
            (expected_interval + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        let mut interval_off = size * PAGE_SIZE;
        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_interval_slot() {
                return Status::BAD_STATE;
            }
            interval_off = off;
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(expected_interval * PAGE_SIZE, interval_off);

        // Add a page in the interval slot.
        let (page, _paddr) = unwrap_ok!(pmm::alloc_page(0), "pmm alloc page");
        expect_true!(add_page(&mut list, page, expected_interval * PAGE_SIZE));

        let mut page_off = size * PAGE_SIZE;
        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_page() {
                return Status::BAD_STATE;
            }
            page_off = off;
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(expected_interval * PAGE_SIZE, page_off);

        list.remove_all_content(|mut p| {
            if p.is_page() {
                let _ = p.release_page();
            }
        });
        // SAFETY: `page` was removed from `list`.
        unsafe { pmm::free_page(page) };
    }

    /// Contiguous run traversal over an interval that spans multiple nodes.
    #[test]
    fn vmpl_interval_contig_full_test() {
        let mut list = VmPageList::new();

        const FAN_OUT: u64 = VmPageListNode::PAGE_FAN_OUT as u64;
        // Interval spanning across 3 nodes, with the middle one unpopulated.
        const EXPECTED_START: u64 = 1;
        const EXPECTED_END: u64 = 2 * FAN_OUT;
        const SIZE: u64 = 3 * FAN_OUT;
        expect_gt!(SIZE, EXPECTED_END);
        expect_ok!(list.add_zero_interval(
            EXPECTED_START * PAGE_SIZE,
            (EXPECTED_END + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, SIZE * PAGE_SIZE));

        let expected_pages: [u64; 2] = [EXPECTED_START, EXPECTED_END];
        let expected_contig: [u64; 2] = [EXPECTED_START, EXPECTED_END + 1];
        let mut pages = [0u64; 2];
        let mut contig = [0u64; 2];
        let mut page_index = 0usize;
        let mut contig_index = 0usize;
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            SIZE * PAGE_SIZE,
            |_p, _off| true,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() {
                    if !page_index.is_multiple_of(2) {
                        return Status::BAD_STATE;
                    }
                } else if page_index.is_multiple_of(2) {
                    return Status::BAD_STATE;
                }
                pages[page_index] = off;
                page_index += 1;
                Status::NEXT
            },
            |begin, end, is_interval| {
                if !is_interval {
                    return Status::BAD_STATE;
                }
                contig[contig_index] = begin;
                contig[contig_index + 1] = end;
                contig_index += 2;
                Status::NEXT
            },
        );
        expect_ok!(status);

        expect_eq!(2, page_index);
        for i in 0..page_index {
            expect_eq!(expected_pages[i] * PAGE_SIZE, pages[i]);
        }

        expect_eq!(2, contig_index);
        for i in 0..contig_index {
            expect_eq!(expected_contig[i] * PAGE_SIZE, contig[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Contiguous run traversal that starts and/or ends partway inside an interval.
    #[test]
    fn vmpl_interval_contig_partial_test() {
        let mut list = VmPageList::new();

        const FAN_OUT: u64 = VmPageListNode::PAGE_FAN_OUT as u64;
        // Interval spanning across 3 nodes, with the middle one unpopulated.
        const EXPECTED_START: u64 = 1;
        const EXPECTED_END: u64 = 2 * FAN_OUT;
        const SIZE: u64 = 3 * FAN_OUT;
        expect_gt!(SIZE, EXPECTED_END);
        expect_ok!(list.add_zero_interval(
            EXPECTED_START * PAGE_SIZE,
            (EXPECTED_END + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, SIZE * PAGE_SIZE));

        let mut page = 0u64;
        let mut contig = [0u64; 2];
        let mut contig_index = 0usize;
        // Start the traversal partway into the interval.
        let status = list.for_every_page_and_contiguous_run_in_range(
            (EXPECTED_START + 1) * PAGE_SIZE,
            SIZE * PAGE_SIZE,
            |_p, _off| true,
            |p, off| {
                if !p.is_interval_end() {
                    return Status::BAD_STATE;
                }
                page = off;
                Status::NEXT
            },
            |begin, end, is_interval| {
                if !is_interval {
                    return Status::BAD_STATE;
                }
                contig[contig_index] = begin;
                contig[contig_index + 1] = end;
                contig_index += 2;
                Status::NEXT
            },
        );
        expect_ok!(status);

        // Should only have visited the end.
        expect_eq!(EXPECTED_END * PAGE_SIZE, page);
        let expected_contig: [u64; 2] = [EXPECTED_START + 1, EXPECTED_END + 1];
        expect_eq!(2, contig_index);
        for i in 0..contig_index {
            expect_eq!(expected_contig[i] * PAGE_SIZE, contig[i]);
        }

        contig_index = 0;
        // End the traversal partway into the interval.
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            (EXPECTED_END - 1) * PAGE_SIZE,
            |_p, _off| true,
            |p, off| {
                if !p.is_interval_start() {
                    return Status::BAD_STATE;
                }
                page = off;
                Status::NEXT
            },
            |begin, end, is_interval| {
                if !is_interval {
                    return Status::BAD_STATE;
                }
                contig[contig_index] = begin;
                contig[contig_index + 1] = end;
                contig_index += 2;
                Status::NEXT
            },
        );
        expect_ok!(status);

        // Should only have visited the start.
        expect_eq!(EXPECTED_START * PAGE_SIZE, page);
        let expected_contig: [u64; 2] = [EXPECTED_START, EXPECTED_END - 1];
        expect_eq!(2, contig_index);
        for i in 0..contig_index {
            expect_eq!(expected_contig[i] * PAGE_SIZE, contig[i]);
        }

        contig_index = 0;
        // Start and end the traversal partway into the interval.
        let status = list.for_every_page_and_contiguous_run_in_range(
            (EXPECTED_START + 1) * PAGE_SIZE,
            (EXPECTED_END - 1) * PAGE_SIZE,
            |_p, _off| true,
            // Should not visit any slot.
            |_p, _off| Status::BAD_STATE,
            |begin, end, is_interval| {
                if !is_interval {
                    return Status::BAD_STATE;
                }
                contig[contig_index] = begin;
                contig[contig_index + 1] = end;
                contig_index += 2;
                Status::NEXT
            },
        );
        expect_ok!(status);

        // Should have seen the requested contiguous range, even though neither the start nor the
        // end was visited.
        let expected_contig: [u64; 2] = [EXPECTED_START + 1, EXPECTED_END - 1];
        expect_eq!(2, contig_index);
        for i in 0..contig_index {
            expect_eq!(expected_contig[i] * PAGE_SIZE, contig[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Contiguous run traversal where only one of the interval sentinels passes the comparison.
    #[test]
    fn vmpl_interval_contig_compare_test() {
        let mut list = VmPageList::new();

        const FAN_OUT: u64 = VmPageListNode::PAGE_FAN_OUT as u64;
        // Interval spanning across 3 nodes, with the middle one unpopulated.
        const EXPECTED_START: u64 = 1;
        const EXPECTED_END: u64 = 2 * FAN_OUT;
        const SIZE: u64 = 3 * FAN_OUT;
        expect_gt!(SIZE, EXPECTED_END);
        expect_ok!(list.add_zero_interval(
            EXPECTED_START * PAGE_SIZE,
            (EXPECTED_END + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, SIZE * PAGE_SIZE));

        let mut page = 0u64;
        // Start the traversal partway into the interval.
        let status = list.for_every_page_and_contiguous_run_in_range(
            (EXPECTED_START + 1) * PAGE_SIZE,
            SIZE * PAGE_SIZE,
            // Interval start evaluates to false.
            |p, _off| !p.is_interval_start(),
            |p, off| {
                if !p.is_interval_end() {
                    return Status::BAD_STATE;
                }
                page = off;
                Status::NEXT
            },
            // The start does not fulfill the condition, so we should not find a valid contiguous
            // run.
            |_begin, _end, _is_interval| Status::INVALID_ARGS,
        );
        expect_ok!(status);

        // Should only have visited the end.
        expect_eq!(EXPECTED_END * PAGE_SIZE, page);

        // End the traversal partway into the interval.
        let status = list.for_every_page_and_contiguous_run_in_range(
            0,
            (EXPECTED_END - 1) * PAGE_SIZE,
            // Interval end evaluates to false.
            |p, _off| !p.is_interval_end(),
            |p, off| {
                if !p.is_interval_start() {
                    return Status::BAD_STATE;
                }
                page = off;
                Status::NEXT
            },
            // The end does not fulfill the condition, so we should not find a valid contiguous run.
            |_begin, _end, _is_interval| Status::INVALID_ARGS,
        );
        expect_ok!(status);

        // Should only have visited the start.
        expect_eq!(EXPECTED_START * PAGE_SIZE, page);

        list.remove_all_content(|_| {});
    }

    /// Tests populating all slots across a 5-node interval.
    #[test]
    fn vmpl_interval_populate_full_test() {
        let mut list = VmPageList::new();
        let expected_start = 1;
        let expected_end = 4 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 5 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Populate the entire interval.
        expect_ok!(list.populate_slots_in_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE
        ));

        let mut next_off = expected_start * PAGE_SIZE;
        // We should only see interval slots.
        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_interval_slot() {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if off != next_off {
                return Status::OUT_OF_RANGE;
            }
            next_off += PAGE_SIZE;
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!((expected_end + 1) * PAGE_SIZE, next_off);

        list.remove_all_content(|_| {});
    }

    /// Tests populating slots in the middle of a multi-node interval.
    #[test]
    fn vmpl_interval_populate_partial_test() {
        let mut list = VmPageList::new();
        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Populate some slots in the middle of the interval.
        let slot_start = expected_start + 2;
        let slot_end = expected_end - 2;
        expect_gt!(slot_end, slot_start);
        expect_ok!(
            list.populate_slots_in_interval(slot_start * PAGE_SIZE, (slot_end + 1) * PAGE_SIZE)
        );

        let expected_intervals = [
            expected_start * PAGE_SIZE,
            (slot_start - 1) * PAGE_SIZE,
            (slot_end + 1) * PAGE_SIZE,
            expected_end * PAGE_SIZE,
        ];
        let mut intervals = [0u64; 4];
        let mut interval_index = 0;
        let mut slot = slot_start * PAGE_SIZE;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_interval() {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() || p.is_interval_end() {
                if p.is_interval_start() && interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                if p.is_interval_end() && interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
                return Status::NEXT;
            }
            if off != slot {
                return Status::BAD_STATE;
            }
            slot += PAGE_SIZE;
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!((slot_end + 1) * PAGE_SIZE, slot);
        expect_eq!(4, interval_index);
        expect_true!(expected_intervals == intervals);

        list.remove_all_content(|_| {});
    }

    /// Tests populating slots beginning at the start of an interval.
    #[test]
    fn vmpl_interval_populate_start_test() {
        let mut list = VmPageList::new();
        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Populate some slots beginning at the start of the interval.
        let slot_start = expected_start;
        let slot_end = expected_end - 2;
        expect_gt!(slot_end, slot_start);
        expect_ok!(
            list.populate_slots_in_interval(slot_start * PAGE_SIZE, (slot_end + 1) * PAGE_SIZE)
        );

        let expected_intervals = [(slot_end + 1) * PAGE_SIZE, expected_end * PAGE_SIZE];
        let mut intervals = [0u64; 2];
        let mut interval_index = 0;
        let mut slot = slot_start * PAGE_SIZE;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_interval() {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() || p.is_interval_end() {
                if p.is_interval_start() && interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                if p.is_interval_end() && interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
                return Status::NEXT;
            }
            if off != slot {
                return Status::BAD_STATE;
            }
            slot += PAGE_SIZE;
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!((slot_end + 1) * PAGE_SIZE, slot);
        expect_eq!(2, interval_index);
        expect_true!(expected_intervals == intervals);

        list.remove_all_content(|_| {});
    }

    /// Tests populating slots ending at the end of an interval.
    #[test]
    fn vmpl_interval_populate_end_test() {
        let mut list = VmPageList::new();
        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Populate some slots ending at the end of the interval.
        let slot_start = expected_start + 2;
        let slot_end = expected_end;
        expect_gt!(slot_end, slot_start);
        expect_ok!(
            list.populate_slots_in_interval(slot_start * PAGE_SIZE, (slot_end + 1) * PAGE_SIZE)
        );

        let expected_intervals = [expected_start * PAGE_SIZE, (slot_start - 1) * PAGE_SIZE];
        let mut intervals = [0u64; 2];
        let mut interval_index = 0;
        let mut slot = slot_start * PAGE_SIZE;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_interval() {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() || p.is_interval_end() {
                if p.is_interval_start() && interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                if p.is_interval_end() && interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
                return Status::NEXT;
            }
            if off != slot {
                return Status::BAD_STATE;
            }
            slot += PAGE_SIZE;
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!((slot_end + 1) * PAGE_SIZE, slot);
        expect_eq!(2, interval_index);
        expect_true!(expected_intervals == intervals);

        list.remove_all_content(|_| {});
    }

    /// Tests populating a single slot, idempotency, and returning it with return_interval_slot.
    #[test]
    fn vmpl_interval_populate_slot_test() {
        let mut list = VmPageList::new();
        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Populate a single slot in the interval.
        let single_slot = expected_end - 3;
        expect_ok!(
            list.populate_slots_in_interval(single_slot * PAGE_SIZE, (single_slot + 1) * PAGE_SIZE)
        );

        let expected_intervals = [
            expected_start * PAGE_SIZE,
            (single_slot - 1) * PAGE_SIZE,
            (single_slot + 1) * PAGE_SIZE,
            expected_end * PAGE_SIZE,
        ];
        let mut intervals = [0u64; 4];
        let mut interval_index = 0;

        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_interval() {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() || p.is_interval_end() {
                if p.is_interval_start() && interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                if p.is_interval_end() && interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
                return Status::NEXT;
            }
            if off != single_slot * PAGE_SIZE {
                return Status::BAD_STATE;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(4, interval_index);
        expect_true!(expected_intervals == intervals);

        // Try to populate a slot over a single sentinel. This should be a no-op.
        expect_ok!(
            list.populate_slots_in_interval(single_slot * PAGE_SIZE, (single_slot + 1) * PAGE_SIZE)
        );
        interval_index = 0;
        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !p.is_interval() {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() || p.is_interval_end() {
                if p.is_interval_start() && interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                if p.is_interval_end() && interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                intervals[interval_index] = off;
                interval_index += 1;
                return Status::NEXT;
            }
            if off != single_slot * PAGE_SIZE {
                return Status::BAD_STATE;
            }
            Status::NEXT
        });
        expect_ok!(res);
        expect_eq!(4, interval_index);
        expect_true!(expected_intervals == intervals);

        // Try to return the single slot that we populated. This should return the interval to its
        // original state.
        list.return_interval_slot(single_slot * PAGE_SIZE);
        let res = list.for_every_page_in_range(0, size * PAGE_SIZE, |p, off| {
            if !(p.is_interval_start() || p.is_interval_end()) {
                return Status::BAD_STATE;
            }
            if !p.is_zero_interval_dirty() {
                return Status::BAD_STATE;
            }
            if p.is_interval_start() && off != expected_start * PAGE_SIZE {
                return Status::BAD_STATE;
            }
            if p.is_interval_end() && off != expected_end * PAGE_SIZE {
                return Status::BAD_STATE;
            }
            Status::NEXT
        });
        expect_ok!(res);

        list.remove_all_content(|_| {});
    }

    /// Tests fully overwriting an interval with a new dirty state.
    #[test]
    fn vmpl_interval_overwrite_full_test() {
        let mut list = VmPageList::new();

        // Interval spanning across 3 nodes, with the middle one unpopulated.
        let expected_start = 1;
        let expected_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, expected_end);
        expect_ok!(list.add_zero_interval(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));
        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Untracked interval overwrites old dirty interval.
        expect_ok!(list.overwrite_zero_interval(
            Some(expected_start * PAGE_SIZE),
            Some(expected_end * PAGE_SIZE),
            expected_start * PAGE_SIZE,
            expected_end * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));

        // Start and end remain the same but the dirty state changes.
        let res = list.for_every_page_and_gap_in_range(
            expected_start * PAGE_SIZE,
            (expected_end + 1) * PAGE_SIZE,
            |p, off| {
                if p.is_interval_start() && off == expected_start * PAGE_SIZE {
                    if !p.is_zero_interval_untracked() {
                        return Status::BAD_STATE;
                    }
                    return Status::NEXT;
                }
                if p.is_interval_end() && off == expected_end * PAGE_SIZE {
                    if !p.is_zero_interval_untracked() {
                        return Status::BAD_STATE;
                    }
                    return Status::NEXT;
                }
                Status::BAD_STATE
            },
            |_, _| Status::BAD_STATE,
        );
        expect_ok!(res);

        list.remove_all_content(|_| {});
    }

    /// Tests breaking off the start of an interval into a new interval.
    #[test]
    fn vmpl_interval_overwrite_start_test() {
        let mut list = VmPageList::new();

        // Interval spanning across 3 nodes, with the middle one unpopulated.
        let old_start = 1;
        let old_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, old_end);
        expect_ok!(list.add_zero_interval(
            old_start * PAGE_SIZE,
            (old_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));
        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Break off the start of the untracked interval into a new dirty interval.
        let new_end = old_end - 5;
        expect_ok!(list.overwrite_zero_interval(
            Some(old_start * PAGE_SIZE),
            None,
            old_start * PAGE_SIZE,
            new_end * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        let expected_intervals = [old_start, new_end, new_end + 1, old_end];
        let expected_state = [
            ZeroRangeDirtyState::Dirty,
            ZeroRangeDirtyState::Dirty,
            ZeroRangeDirtyState::Untracked,
            ZeroRangeDirtyState::Untracked,
        ];
        let mut intervals = [0u64; 4];
        let mut interval_state = [ZeroRangeDirtyState::Untracked; 4];
        let mut index = 0;
        let res = list.for_every_page_and_gap_in_range(
            old_start * PAGE_SIZE,
            (old_end + 1) * PAGE_SIZE,
            |p, off| {
                if p.is_interval()
                    && index < 4
                    && ((p.is_interval_start() && index % 2 == 0)
                        || (p.is_interval_end() && index % 2 == 1))
                {
                    intervals[index] = off / PAGE_SIZE;
                    interval_state[index] = p.zero_interval_dirty_state();
                    index += 1;
                    return Status::NEXT;
                }
                Status::BAD_STATE
            },
            |_, _| Status::BAD_STATE,
        );
        expect_ok!(res);
        expect_eq!(4, index);
        for i in 0..index {
            expect_eq!(expected_intervals[i], intervals[i]);
            expect_eq!(expected_state[i], interval_state[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests breaking off the end of an interval into a new interval.
    #[test]
    fn vmpl_interval_overwrite_end_test() {
        let mut list = VmPageList::new();

        // Interval spanning across 3 nodes, with the middle one unpopulated.
        let old_start = 1;
        let old_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, old_end);
        expect_ok!(list.add_zero_interval(
            old_start * PAGE_SIZE,
            (old_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));
        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Break off the end of the untracked interval into a new dirty interval.
        let new_start = old_start + 5;
        expect_ok!(list.overwrite_zero_interval(
            None,
            Some(old_end * PAGE_SIZE),
            new_start * PAGE_SIZE,
            old_end * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        let expected_intervals = [old_start, new_start - 1, new_start, old_end];
        let expected_state = [
            ZeroRangeDirtyState::Untracked,
            ZeroRangeDirtyState::Untracked,
            ZeroRangeDirtyState::Dirty,
            ZeroRangeDirtyState::Dirty,
        ];
        let mut intervals = [0u64; 4];
        let mut interval_state = [ZeroRangeDirtyState::Untracked; 4];
        let mut index = 0;
        let res = list.for_every_page_and_gap_in_range(
            old_start * PAGE_SIZE,
            (old_end + 1) * PAGE_SIZE,
            |p, off| {
                if p.is_interval()
                    && index < 4
                    && ((p.is_interval_start() && index % 2 == 0)
                        || (p.is_interval_end() && index % 2 == 1))
                {
                    intervals[index] = off / PAGE_SIZE;
                    interval_state[index] = p.zero_interval_dirty_state();
                    index += 1;
                    return Status::NEXT;
                }
                Status::BAD_STATE
            },
            |_, _| Status::BAD_STATE,
        );
        expect_ok!(res);
        expect_eq!(4, index);
        for i in 0..index {
            expect_eq!(expected_intervals[i], intervals[i]);
            expect_eq!(expected_state[i], interval_state[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests overwriting an interval spanning a single slot.
    #[test]
    fn vmpl_interval_overwrite_slot_test() {
        let mut list = VmPageList::new();

        // Interval spanning a single slot.
        let expected_slot = 1;
        expect_ok!(list.add_zero_interval(
            expected_slot * PAGE_SIZE,
            (expected_slot + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));
        expect_true!(list.any_pages_or_intervals_in_range(
            expected_slot * PAGE_SIZE,
            (expected_slot + 1) * PAGE_SIZE
        ));

        // Untracked interval overwrites old dirty interval.
        expect_ok!(list.overwrite_zero_interval(
            Some(expected_slot * PAGE_SIZE),
            Some(expected_slot * PAGE_SIZE),
            expected_slot * PAGE_SIZE,
            expected_slot * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));

        // Start and end remain the same but the dirty state changes.
        let res = list.for_every_page_and_gap_in_range(
            expected_slot * PAGE_SIZE,
            (expected_slot + 1) * PAGE_SIZE,
            |p, off| {
                if p.is_interval_slot() && off == expected_slot * PAGE_SIZE {
                    if !p.is_zero_interval_untracked() {
                        return Status::BAD_STATE;
                    }
                    return Status::NEXT;
                }
                Status::BAD_STATE
            },
            |_, _| Status::BAD_STATE,
        );
        expect_ok!(res);

        list.remove_all_content(|_| {});
    }

    /// Tests an overwrite that merges the new interval with the one to the left.
    #[test]
    fn vmpl_interval_overwrite_merge_left_test() {
        let mut list = VmPageList::new();

        // Two intervals next to each other with different dirty states.
        let left_start = 1;
        let left_end = 4;
        let right_start = left_end + 1;
        let right_end = 10;
        expect_ok!(list.add_zero_interval(
            left_start * PAGE_SIZE,
            (left_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));
        expect_ok!(list.add_zero_interval(
            right_start * PAGE_SIZE,
            (right_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));
        expect_true!(
            list.any_pages_or_intervals_in_range(
                left_start * PAGE_SIZE,
                (right_end + 1) * PAGE_SIZE
            )
        );

        // Break off the start of the right interval so that it merges with the left interval.
        let new_end = right_start + 2;
        expect_ok!(list.overwrite_zero_interval(
            Some(right_start * PAGE_SIZE),
            None,
            right_start * PAGE_SIZE,
            new_end * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        let expected_intervals = [left_start, new_end, new_end + 1, right_end];
        let expected_state = [
            ZeroRangeDirtyState::Dirty,
            ZeroRangeDirtyState::Dirty,
            ZeroRangeDirtyState::Untracked,
            ZeroRangeDirtyState::Untracked,
        ];
        let mut intervals = [0u64; 4];
        let mut interval_state = [ZeroRangeDirtyState::Untracked; 4];
        let mut index = 0;
        let res = list.for_every_page_and_gap_in_range(
            left_start * PAGE_SIZE,
            (right_end + 1) * PAGE_SIZE,
            |p, off| {
                if p.is_interval()
                    && index < 4
                    && ((p.is_interval_start() && index % 2 == 0)
                        || (p.is_interval_end() && index % 2 == 1))
                {
                    intervals[index] = off / PAGE_SIZE;
                    interval_state[index] = p.zero_interval_dirty_state();
                    index += 1;
                    return Status::NEXT;
                }
                Status::BAD_STATE
            },
            |_, _| Status::BAD_STATE,
        );
        expect_ok!(res);
        expect_eq!(4, index);
        for i in 0..index {
            expect_eq!(expected_intervals[i], intervals[i]);
            expect_eq!(expected_state[i], interval_state[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests an overwrite that merges the new interval with the one to the right.
    #[test]
    fn vmpl_interval_overwrite_merge_right_test() {
        let mut list = VmPageList::new();

        // Two intervals next to each other with different dirty states.
        let left_start = 1;
        let left_end = 6;
        let right_start = left_end + 1;
        let right_end = 10;
        expect_ok!(list.add_zero_interval(
            left_start * PAGE_SIZE,
            (left_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));
        expect_ok!(list.add_zero_interval(
            right_start * PAGE_SIZE,
            (right_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));
        expect_true!(
            list.any_pages_or_intervals_in_range(
                left_start * PAGE_SIZE,
                (right_end + 1) * PAGE_SIZE
            )
        );

        // Break off the end of the left interval so that it merges with the right interval.
        let new_start = left_end - 2;
        expect_ok!(list.overwrite_zero_interval(
            None,
            Some(left_end * PAGE_SIZE),
            new_start * PAGE_SIZE,
            left_end * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));

        let expected_intervals = [left_start, new_start - 1, new_start, right_end];
        let expected_state = [
            ZeroRangeDirtyState::Dirty,
            ZeroRangeDirtyState::Dirty,
            ZeroRangeDirtyState::Untracked,
            ZeroRangeDirtyState::Untracked,
        ];
        let mut intervals = [0u64; 4];
        let mut interval_state = [ZeroRangeDirtyState::Untracked; 4];
        let mut index = 0;
        let res = list.for_every_page_and_gap_in_range(
            left_start * PAGE_SIZE,
            (right_end + 1) * PAGE_SIZE,
            |p, off| {
                if p.is_interval()
                    && index < 4
                    && ((p.is_interval_start() && index % 2 == 0)
                        || (p.is_interval_end() && index % 2 == 1))
                {
                    intervals[index] = off / PAGE_SIZE;
                    interval_state[index] = p.zero_interval_dirty_state();
                    index += 1;
                    return Status::NEXT;
                }
                Status::BAD_STATE
            },
            |_, _| Status::BAD_STATE,
        );
        expect_ok!(res);
        expect_eq!(4, index);
        for i in 0..index {
            expect_eq!(expected_intervals[i], intervals[i]);
            expect_eq!(expected_state[i], interval_state[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests an overwrite of a single slot that merges on both sides.
    #[test]
    fn vmpl_interval_overwrite_merge_slots_test() {
        let mut list = VmPageList::new();

        // Three interval slots with alternating dirty states.
        let left = 3;
        let mid = 4;
        let right = 5;
        expect_ok!(list.add_zero_interval(
            left * PAGE_SIZE,
            (left + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));
        expect_ok!(list.add_zero_interval(
            mid * PAGE_SIZE,
            (mid + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));
        expect_ok!(list.add_zero_interval(
            right * PAGE_SIZE,
            (right + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));
        expect_true!(
            list.any_pages_or_intervals_in_range(left * PAGE_SIZE, (right + 1) * PAGE_SIZE)
        );

        // Overwrite the center so that it merges on both sides.
        expect_ok!(list.overwrite_zero_interval(
            Some(mid * PAGE_SIZE),
            Some(mid * PAGE_SIZE),
            mid * PAGE_SIZE,
            mid * PAGE_SIZE,
            ZeroRangeDirtyState::Untracked
        ));

        let expected_intervals = [left, right];
        let expected_state = [ZeroRangeDirtyState::Untracked, ZeroRangeDirtyState::Untracked];
        let mut intervals = [0u64; 2];
        let mut interval_state = [ZeroRangeDirtyState::Dirty; 2];
        let mut index = 0;
        let res = list.for_every_page_and_gap_in_range(
            left * PAGE_SIZE,
            (right + 1) * PAGE_SIZE,
            |p, off| {
                if p.is_interval()
                    && index < 2
                    && ((p.is_interval_start() && index % 2 == 0)
                        || (p.is_interval_end() && index % 2 == 1))
                {
                    intervals[index] = off / PAGE_SIZE;
                    interval_state[index] = p.zero_interval_dirty_state();
                    index += 1;
                    return Status::NEXT;
                }
                Status::BAD_STATE
            },
            |_, _| Status::BAD_STATE,
        );
        expect_ok!(res);
        expect_eq!(2, index);
        for i in 0..index {
            expect_eq!(expected_intervals[i], intervals[i]);
            expect_eq!(expected_state[i], interval_state[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests clipping an interval at the start.
    #[test]
    fn vmpl_interval_clip_start_test() {
        let mut list = VmPageList::new();

        // Interval spanning across 3 nodes, with the middle one unpopulated.
        let old_start = 1;
        let old_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, old_end);
        expect_ok!(list.add_zero_interval(
            old_start * PAGE_SIZE,
            (old_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Clip the start such that the interval still spans multiple pages.
        let new_start = old_end - 3;
        expect_ok!(
            list.clip_interval_start(old_start * PAGE_SIZE, (new_start - old_start) * PAGE_SIZE)
        );

        let expected_intervals = [new_start, old_end];
        let mut expected_gaps = [0, new_start, old_end + 1, size];
        let mut intervals = [0u64; 4];
        let mut gaps = [0u64; 4];
        let mut interval_index = 0;
        let mut gap_index = 0;
        let res = list.for_every_page_and_gap_in_range(
            0,
            size * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() && interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                if p.is_interval_end() && interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                if interval_index < 4 {
                    intervals[interval_index] = off;
                    interval_index += 1;
                }
                Status::NEXT
            },
            |begin, end| {
                if gap_index < 4 {
                    gaps[gap_index] = begin;
                    gaps[gap_index + 1] = end;
                    gap_index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);
        expect_eq!(2, interval_index);
        for i in 0..interval_index {
            expect_eq!(expected_intervals[i] * PAGE_SIZE, intervals[i]);
        }
        expect_eq!(4, gap_index);
        for i in 0..gap_index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        // Clip the start again, leaving behind just a single interval slot.
        expect_ok!(
            list.clip_interval_start(new_start * PAGE_SIZE, (old_end - new_start) * PAGE_SIZE)
        );
        expected_gaps[1] = old_end;
        gap_index = 0;
        // We should see a single interval slot.
        let res = list.for_every_page_and_gap_in_range(
            0,
            size * PAGE_SIZE,
            |p, off| {
                if !p.is_interval_slot() {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if off != old_end * PAGE_SIZE {
                    return Status::BAD_STATE;
                }
                Status::NEXT
            },
            |begin, end| {
                if gap_index < 4 {
                    gaps[gap_index] = begin;
                    gaps[gap_index + 1] = end;
                    gap_index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);
        expect_eq!(4, gap_index);
        for i in 0..gap_index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests clipping an interval at the end.
    #[test]
    fn vmpl_interval_clip_end_test() {
        let mut list = VmPageList::new();

        // Interval spanning across 3 nodes, with the middle one unpopulated.
        let old_start = 1;
        let old_end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64);
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64);
        expect_gt!(size, old_end);
        expect_ok!(list.add_zero_interval(
            old_start * PAGE_SIZE,
            (old_end + 1) * PAGE_SIZE,
            ZeroRangeDirtyState::Dirty
        ));

        expect_true!(list.any_pages_or_intervals_in_range(0, size * PAGE_SIZE));

        // Clip the end such that the interval still spans multiple pages.
        let new_end = old_start + 3;
        expect_ok!(list.clip_interval_end(old_end * PAGE_SIZE, (old_end - new_end) * PAGE_SIZE));

        let expected_intervals = [old_start, new_end];
        let mut expected_gaps = [0, old_start, new_end + 1, size];
        let mut intervals = [0u64; 4];
        let mut gaps = [0u64; 4];
        let mut interval_index = 0;
        let mut gap_index = 0;
        let res = list.for_every_page_and_gap_in_range(
            0,
            size * PAGE_SIZE,
            |p, off| {
                if !(p.is_interval_start() || p.is_interval_end()) {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if p.is_interval_start() && interval_index % 2 == 1 {
                    return Status::BAD_STATE;
                }
                if p.is_interval_end() && interval_index % 2 == 0 {
                    return Status::BAD_STATE;
                }
                if interval_index < 4 {
                    intervals[interval_index] = off;
                    interval_index += 1;
                }
                Status::NEXT
            },
            |begin, end| {
                if gap_index < 4 {
                    gaps[gap_index] = begin;
                    gaps[gap_index + 1] = end;
                    gap_index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);
        expect_eq!(2, interval_index);
        for i in 0..interval_index {
            expect_eq!(expected_intervals[i] * PAGE_SIZE, intervals[i]);
        }
        expect_eq!(4, gap_index);
        for i in 0..gap_index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        // Clip the end again, leaving behind just a single interval slot.
        expect_ok!(list.clip_interval_end(new_end * PAGE_SIZE, (new_end - old_start) * PAGE_SIZE));
        expected_gaps[2] = old_start + 1;
        gap_index = 0;
        // We should see a single interval slot.
        let res = list.for_every_page_and_gap_in_range(
            0,
            size * PAGE_SIZE,
            |p, off| {
                if !p.is_interval_slot() {
                    return Status::BAD_STATE;
                }
                if !p.is_zero_interval_dirty() {
                    return Status::BAD_STATE;
                }
                if off != old_start * PAGE_SIZE {
                    return Status::BAD_STATE;
                }
                Status::NEXT
            },
            |begin, end| {
                if gap_index < 4 {
                    gaps[gap_index] = begin;
                    gaps[gap_index + 1] = end;
                    gap_index += 2;
                }
                Status::NEXT
            },
        );
        expect_ok!(res);
        expect_eq!(4, gap_index);
        for i in 0..gap_index {
            expect_eq!(expected_gaps[i] * PAGE_SIZE, gaps[i]);
        }

        list.remove_all_content(|_| {});
    }

    /// Tests replacing an existing page with a zero interval.
    #[test]
    fn vmpl_replace_page_with_zero_interval_test() {
        let mut list = VmPageList::new();
        let [test_page] = get_pages::<1>();
        let offset = PAGE_SIZE;

        expect_true!(add_page(&mut list, test_page, offset));
        expect_true!(list.lookup(offset).is_some_and(|p| p.page() == test_page));

        // The released page is handed back to the caller and the slot becomes a zero interval.
        let released = list.replace_page_with_zero_interval(offset, ZeroRangeDirtyState::Dirty);
        expect_true!(released == test_page);
        expect_true!(list.has_no_page_or_ref());

        let slot = list.lookup(offset).unwrap();
        expect_true!(slot.is_interval_slot());
        expect_true!(slot.is_zero_interval_dirty());
        expect_true!(list.is_offset_in_zero_interval(offset));

        list.remove_all_content(|_| {});
        free_pages([released]);
    }

    /// Tests awaiting clean length handling when splitting an interval.
    #[test]
    fn vmpl_awaiting_clean_split_test() {
        let mut list = VmPageList::new();
        let start = PAGE_SIZE;
        let end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        expect_gt!(size, end);
        expect_ok!(list.add_zero_interval(start, end + PAGE_SIZE, ZeroRangeDirtyState::Dirty));

        expect_true!(list.any_pages_or_intervals_in_range(0, size));

        // Set awaiting clean length.
        let expected_len = end - start + PAGE_SIZE;
        list.lookup_mut(start).unwrap().set_zero_interval_awaiting_clean_length(expected_len);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Split the interval in the middle.
        let mid = end - 2 * PAGE_SIZE;
        expect_ok!(list.populate_slots_in_interval(mid, mid + PAGE_SIZE));

        // Awaiting clean length remains unchanged.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(0, list.lookup(mid).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(0, list.lookup(mid + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length());

        // Split the interval at the end.
        expect_ok!(list.populate_slots_in_interval(end, end + PAGE_SIZE));

        // Awaiting clean length remains unchanged.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(0, list.lookup(mid).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(0, list.lookup(mid + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(0, list.lookup(end).unwrap().zero_interval_awaiting_clean_length());

        // Split the interval at the start.
        expect_ok!(list.populate_slots_in_interval(start, start + PAGE_SIZE));

        // Awaiting clean length now moves to the new start.
        expect_eq!(PAGE_SIZE, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            expected_len - PAGE_SIZE,
            list.lookup(start + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(0, list.lookup(mid).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(0, list.lookup(mid + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(0, list.lookup(end).unwrap().zero_interval_awaiting_clean_length());

        list.remove_all_content(|_| {});
    }

    /// Tests awaiting clean length handling when clipping an interval.
    #[test]
    fn vmpl_awaiting_clean_clip_test() {
        let mut list = VmPageList::new();
        let start = PAGE_SIZE;
        let end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        expect_gt!(size, end);
        expect_ok!(list.add_zero_interval(start, end + PAGE_SIZE, ZeroRangeDirtyState::Dirty));

        expect_true!(list.any_pages_or_intervals_in_range(0, size));

        // Set awaiting clean length.
        let expected_len = end - start + PAGE_SIZE;
        list.lookup_mut(start).unwrap().set_zero_interval_awaiting_clean_length(expected_len);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Clip the interval at the end.
        expect_ok!(list.clip_interval_end(end, 2 * PAGE_SIZE));

        // Awaiting clean length is unchanged.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Clip the interval at the start.
        expect_ok!(list.clip_interval_start(start, 2 * PAGE_SIZE));

        // Awaiting clean length is clipped too.
        expect_eq!(
            expected_len - 2 * PAGE_SIZE,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        list.remove_all_content(|_| {});
    }

    /// Tests restoring awaiting clean length when returning a split slot.
    #[test]
    fn vmpl_awaiting_clean_return_slot_test() {
        let mut list = VmPageList::new();
        let start = PAGE_SIZE;
        let end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        expect_gt!(size, end);
        expect_ok!(list.add_zero_interval(start, end + PAGE_SIZE, ZeroRangeDirtyState::Dirty));

        expect_true!(list.any_pages_or_intervals_in_range(0, size));

        // Set awaiting clean length.
        let expected_len = end - start + PAGE_SIZE;
        list.lookup_mut(start).unwrap().set_zero_interval_awaiting_clean_length(expected_len);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Split the interval at the start.
        expect_ok!(list.populate_slots_in_interval(start, start + PAGE_SIZE));

        // Awaiting clean length now moves to the new start.
        expect_eq!(PAGE_SIZE, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            expected_len - PAGE_SIZE,
            list.lookup(start + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the populated slot.
        list.return_interval_slot(start);

        // Awaiting clean length is now restored.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        list.remove_all_content(|_| {});
    }

    /// Tests returning multiple split slots and verifying awaiting clean length merges.
    #[test]
    fn vmpl_awaiting_clean_return_slots_test() {
        let mut list = VmPageList::new();
        let start = PAGE_SIZE;
        let end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        expect_gt!(size, end);
        expect_ok!(list.add_zero_interval(start, end + PAGE_SIZE, ZeroRangeDirtyState::Dirty));

        expect_true!(list.any_pages_or_intervals_in_range(0, size));

        // Set awaiting clean length.
        let expected_len = end - start + PAGE_SIZE;
        list.lookup_mut(start).unwrap().set_zero_interval_awaiting_clean_length(expected_len);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Split the start multiple times, so that all the resultant slots have non-zero awaiting
        // clean lengths.
        expect_ok!(list.populate_slots_in_interval(start, start + PAGE_SIZE));
        expect_ok!(list.populate_slots_in_interval(start + PAGE_SIZE, start + 2 * PAGE_SIZE));
        expect_ok!(list.populate_slots_in_interval(start + 2 * PAGE_SIZE, start + 3 * PAGE_SIZE));

        // Verify awaiting clean lengths.
        expect_eq!(PAGE_SIZE, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            PAGE_SIZE,
            list.lookup(start + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            PAGE_SIZE,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            expected_len - 3 * PAGE_SIZE,
            list.lookup(start + 3 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the first slot. This will combine the first two slots into an interval.
        list.return_interval_slot(start);
        expect_true!(list.lookup(start).unwrap().is_interval_start());
        expect_true!(list.lookup(start + PAGE_SIZE).unwrap().is_interval_end());

        // Verify awaiting clean lengths.
        expect_eq!(
            2 * PAGE_SIZE,
            list.lookup(start).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            PAGE_SIZE,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            expected_len - 3 * PAGE_SIZE,
            list.lookup(start + 3 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the third slot. This will merge all the intervals and return everything to the
        // original state.
        list.return_interval_slot(start + 2 * PAGE_SIZE);
        // Awaiting clean length is restored.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        let res = list.for_every_page(|p, off| {
            if p.is_interval_start() {
                if off != start {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            if p.is_interval_end() {
                if off != end {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            Status::BAD_STATE
        });
        expect_ok!(res);

        list.remove_all_content(|_| {});
    }

    /// Tests populate_slots_in_interval with awaiting clean length and restoring slots.
    #[test]
    fn vmpl_awaiting_clean_populate_slots_test() {
        let mut list = VmPageList::new();
        let start = PAGE_SIZE;
        let end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        expect_gt!(size, end);
        expect_ok!(list.add_zero_interval(start, end + PAGE_SIZE, ZeroRangeDirtyState::Dirty));

        expect_true!(list.any_pages_or_intervals_in_range(0, size));

        // Set awaiting clean length.
        let expected_len = end - start + PAGE_SIZE;
        list.lookup_mut(start).unwrap().set_zero_interval_awaiting_clean_length(expected_len);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Populate some slots at the start.
        expect_ok!(list.populate_slots_in_interval(start, start + 3 * PAGE_SIZE));

        // Verify awaiting clean lengths.
        expect_eq!(PAGE_SIZE, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            PAGE_SIZE,
            list.lookup(start + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            PAGE_SIZE,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            expected_len - 3 * PAGE_SIZE,
            list.lookup(start + 3 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the first slot. This will combine the first two slots into an interval.
        list.return_interval_slot(start);
        expect_true!(list.lookup(start).unwrap().is_interval_start());
        expect_true!(list.lookup(start + PAGE_SIZE).unwrap().is_interval_end());

        // Verify awaiting clean lengths.
        expect_eq!(
            2 * PAGE_SIZE,
            list.lookup(start).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            PAGE_SIZE,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            expected_len - 3 * PAGE_SIZE,
            list.lookup(start + 3 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the third slot. This will merge all the intervals and return everything to the
        // original state.
        list.return_interval_slot(start + 2 * PAGE_SIZE);
        // Awaiting clean length is restored.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        let res = list.for_every_page(|p, off| {
            if p.is_interval_start() {
                if off != start {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            if p.is_interval_end() {
                if off != end {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            Status::BAD_STATE
        });
        expect_ok!(res);

        list.remove_all_content(|_| {});
    }

    /// Tests intersecting awaiting clean length with slot populate and return.
    #[test]
    fn vmpl_awaiting_clean_intersecting_test() {
        let mut list = VmPageList::new();
        let start = PAGE_SIZE;
        let end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        expect_gt!(size, end);
        expect_ok!(list.add_zero_interval(start, end + PAGE_SIZE, ZeroRangeDirtyState::Dirty));

        expect_true!(list.any_pages_or_intervals_in_range(0, size));

        // Set awaiting clean length to only a portion of the interval.
        let expected_len = 2 * PAGE_SIZE;
        list.lookup_mut(start).unwrap().set_zero_interval_awaiting_clean_length(expected_len);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Populate some slots at the start, some of them within the awaiting clean length, and some
        // outside.
        expect_ok!(list.populate_slots_in_interval(start, start + 3 * PAGE_SIZE));

        // Verify awaiting clean lengths.
        expect_eq!(PAGE_SIZE, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            PAGE_SIZE,
            list.lookup(start + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + 3 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the first slot. This will combine the first two slots into an interval.
        list.return_interval_slot(start);
        expect_true!(list.lookup(start).unwrap().is_interval_start());
        expect_true!(list.lookup(start + PAGE_SIZE).unwrap().is_interval_end());

        // Verify awaiting clean lengths.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            0,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + 3 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the third slot. This will merge all the intervals and return everything to the
        // original state.
        list.return_interval_slot(start + 2 * PAGE_SIZE);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        let res = list.for_every_page(|p, off| {
            if p.is_interval_start() {
                if off != start {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            if p.is_interval_end() {
                if off != end {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            Status::BAD_STATE
        });
        expect_ok!(res);

        // Populate a slot again, but starting partway into the interval.
        expect_ok!(list.populate_slots_in_interval(start + PAGE_SIZE, start + 2 * PAGE_SIZE));

        // The start's awaiting clean length should remain unchanged.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        // The awaiting clean length for the populated slot and the remaining interval is 0.
        expect_eq!(
            0,
            list.lookup(start + PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + 2 * PAGE_SIZE).unwrap().zero_interval_awaiting_clean_length()
        );

        // Return the slot. This should return to the original state.
        list.return_interval_slot(start + PAGE_SIZE);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        let res = list.for_every_page(|p, off| {
            if p.is_interval_start() {
                if off != start {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            if p.is_interval_end() {
                if off != end {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            Status::BAD_STATE
        });
        expect_ok!(res);

        list.remove_all_content(|_| {});
    }

    /// Tests non-intersecting awaiting clean length with slot populate and return.
    #[test]
    fn vmpl_awaiting_clean_non_intersecting_test() {
        let mut list = VmPageList::new();
        let start = PAGE_SIZE;
        let end = 2 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        let size = 3 * (VmPageListNode::PAGE_FAN_OUT as u64) * PAGE_SIZE;
        expect_gt!(size, end);
        expect_ok!(list.add_zero_interval(start, end + PAGE_SIZE, ZeroRangeDirtyState::Dirty));

        expect_true!(list.any_pages_or_intervals_in_range(0, size));

        // Set awaiting clean length to only a portion of the interval.
        let expected_len = 2 * PAGE_SIZE;
        list.lookup_mut(start).unwrap().set_zero_interval_awaiting_clean_length(expected_len);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());

        // Populate some slots that do not intersect with the awaiting clean length.
        expect_ok!(list.populate_slots_in_interval(
            start + expected_len,
            start + expected_len + 3 * PAGE_SIZE
        ));

        // Verify awaiting clean lengths.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            0,
            list.lookup(start + expected_len).unwrap().zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + expected_len + PAGE_SIZE)
                .unwrap()
                .zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + expected_len + 2 * PAGE_SIZE)
                .unwrap()
                .zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + expected_len + 3 * PAGE_SIZE)
                .unwrap()
                .zero_interval_awaiting_clean_length()
        );

        // Return the first slot. This will merge the first two slots back into the interval.
        list.return_interval_slot(start + expected_len);
        expect_true!(list.lookup(start + expected_len + PAGE_SIZE).unwrap().is_interval_end());

        // Verify awaiting clean lengths.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        expect_eq!(
            0,
            list.lookup(start + expected_len + 2 * PAGE_SIZE)
                .unwrap()
                .zero_interval_awaiting_clean_length()
        );
        expect_eq!(
            0,
            list.lookup(start + expected_len + 3 * PAGE_SIZE)
                .unwrap()
                .zero_interval_awaiting_clean_length()
        );

        // Return the third slot. This will merge all the intervals and return everything to the
        // original state.
        list.return_interval_slot(start + expected_len + 2 * PAGE_SIZE);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        let res = list.for_every_page(|p, off| {
            if p.is_interval_start() {
                if off != start {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            if p.is_interval_end() {
                if off != end {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            Status::BAD_STATE
        });
        expect_ok!(res);

        // Populate a slot again, this time at the end.
        expect_ok!(list.populate_slots_in_interval(end, end + PAGE_SIZE));

        // The start's awaiting clean length should remain unchanged.
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        // The awaiting clean length for the populated slot is 0.
        expect_eq!(0, list.lookup(end).unwrap().zero_interval_awaiting_clean_length());

        // Return the slot. This should return to the original state.
        list.return_interval_slot(end);
        expect_eq!(expected_len, list.lookup(start).unwrap().zero_interval_awaiting_clean_length());
        let res = list.for_every_page(|p, off| {
            if p.is_interval_start() {
                if off != start {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            if p.is_interval_end() {
                if off != end {
                    return Status::BAD_STATE;
                }
                return Status::NEXT;
            }
            Status::BAD_STATE
        });
        expect_ok!(res);

        list.remove_all_content(|_| {});
    }

    /// Moves the content of `src` into `dst`.
    fn migrate_content(src: &mut VmPageOrMarker, dst: &mut VmPageOrMarker, _offset: u64) {
        *dst = src.swap(VmPageOrMarker::empty());
    }

    /// Tests taking a page from the start of a VmPageListNode.
    #[test]
    fn vmpl_take_single_page_even_test() {
        let mut pl = VmPageList::new();

        let (test_page, _) = unwrap_ok!(pmm::alloc_page(0));
        let (test_page2, _) = unwrap_ok!(pmm::alloc_page(0));

        expect_true!(add_page(&mut pl, test_page, 0));
        expect_true!(add_page(&mut pl, test_page2, PAGE_SIZE));

        stack_pin_init!(let splice = VmPageSpliceList::with_length(PAGE_SIZE));
        expect_ok!(splice.as_mut().add_pages_from(migrate_content, &mut pl, 0));

        expect_true!(splice.is_finalized());
        let mut popped = splice.as_mut().pop();
        expect_true!(popped.release_page() == test_page, "wrong page");
        expect_true!(splice.is_processed(), "extra page");
        expect_true!(pl.lookup(0).is_none_or(|p| p.is_empty()), "duplicate page");

        expect_true!(pl.remove_content(PAGE_SIZE).release_page() == test_page2, "remove failure");

        free_pages([test_page, test_page2]);
    }

    /// Tests taking a page from the middle of a VmPageListNode.
    #[test]
    fn vmpl_take_single_page_odd_test() {
        let mut pl = VmPageList::new();

        let (test_page, _) = unwrap_ok!(pmm::alloc_page(0));
        let (test_page2, _) = unwrap_ok!(pmm::alloc_page(0));

        expect_true!(add_page(&mut pl, test_page, 0));
        expect_true!(add_page(&mut pl, test_page2, PAGE_SIZE));

        stack_pin_init!(let splice = VmPageSpliceList::with_length(PAGE_SIZE));
        expect_ok!(splice.as_mut().add_pages_from(migrate_content, &mut pl, PAGE_SIZE));

        expect_true!(splice.is_finalized());
        let mut popped = splice.as_mut().pop();
        expect_true!(popped.release_page() == test_page2, "wrong page");
        expect_true!(splice.is_processed(), "extra page");
        expect_true!(pl.lookup(PAGE_SIZE).is_none_or(|p| p.is_empty()), "duplicate page");

        expect_true!(pl.remove_content(0).release_page() == test_page, "remove failure");

        free_pages([test_page, test_page2]);
    }

    /// Tests taking all the pages from a range of VmPageListNodes.
    #[test]
    fn vmpl_take_all_pages_test() {
        let mut pl = VmPageList::new();
        const COUNT: usize = 3 * VmPageListNode::PAGE_FAN_OUT;
        let test_pages = get_pages::<COUNT>();

        for (i, page) in test_pages.iter().enumerate() {
            expect_true!(add_page(&mut pl, *page, (i as u64) * 2 * PAGE_SIZE));
            expect_true!(add_marker(&mut pl, ((i as u64) * 2 + 1) * PAGE_SIZE));
        }

        stack_pin_init!(let splice =
            VmPageSpliceList::with_length((COUNT as u64) * 2 * PAGE_SIZE));
        expect_ok!(splice.as_mut().add_pages_from(migrate_content, &mut pl, 0));
        expect_true!(splice.is_finalized());
        expect_true!(pl.is_empty(), "non-empty list");

        for page in test_pages.iter() {
            let mut popped = splice.as_mut().pop();
            expect_true!(popped.release_page() == *page, "wrong page");
            expect_true!(splice.as_mut().pop().is_marker(), "expected marker");
        }
        expect_true!(splice.is_processed(), "extra pages");

        free_pages(test_pages);
    }

    /// Tests taking the middle pages from a range of VmPageListNodes.
    #[test]
    fn vmpl_take_middle_pages_test() {
        let mut pl = VmPageList::new();
        const COUNT: usize = 3 * VmPageListNode::PAGE_FAN_OUT;
        let test_pages = get_pages::<COUNT>();

        for (i, page) in test_pages.iter().enumerate() {
            expect_true!(add_page(&mut pl, *page, (i as u64) * PAGE_SIZE));
        }

        const TAKE_OFFSET: usize = VmPageListNode::PAGE_FAN_OUT - 1;
        const TAKE_COUNT: usize = VmPageListNode::PAGE_FAN_OUT + 2;
        stack_pin_init!(let splice =
            VmPageSpliceList::with_length((TAKE_COUNT as u64) * PAGE_SIZE));
        expect_ok!(splice.as_mut().add_pages_from(
            migrate_content,
            &mut pl,
            (TAKE_OFFSET as u64) * PAGE_SIZE
        ));

        expect_true!(splice.is_finalized());
        expect_false!(pl.is_empty(), "non-empty list");

        for (i, page) in test_pages.iter().enumerate() {
            if (TAKE_OFFSET..TAKE_OFFSET + TAKE_COUNT).contains(&i) {
                let mut popped = splice.as_mut().pop();
                expect_true!(popped.release_page() == *page, "wrong page");
            } else {
                expect_true!(
                    pl.remove_content((i as u64) * PAGE_SIZE).release_page() == *page,
                    "remove failure"
                );
            }
        }
        expect_true!(splice.is_processed(), "extra pages");

        free_pages(test_pages);
    }

    /// Tests that gaps are preserved in the list.
    #[test]
    fn vmpl_take_gap_test() {
        let mut pl = VmPageList::new();
        const COUNT: usize = VmPageListNode::PAGE_FAN_OUT;
        const GAP_SIZE: u64 = 2;

        let test_pages = get_pages::<COUNT>();

        for (i, page) in test_pages.iter().enumerate() {
            let offset = (i as u64) * (GAP_SIZE + 1) * PAGE_SIZE;
            expect_true!(add_page(&mut pl, *page, offset));
        }

        const LIST_START: u64 = PAGE_SIZE;
        const LIST_LEN: u64 = ((COUNT as u64) * (GAP_SIZE + 1) - 2) * PAGE_SIZE;
        stack_pin_init!(let splice = VmPageSpliceList::with_length(LIST_LEN));
        expect_ok!(splice.as_mut().add_pages_from(migrate_content, &mut pl, LIST_START));

        expect_true!(splice.is_finalized());
        expect_true!(pl.remove_content(0).release_page() == test_pages[0], "wrong page");
        expect_true!(pl.lookup(LIST_LEN).is_none_or(|p| p.is_empty()), "wrong page");

        let mut offset = LIST_START;
        while offset < LIST_START + LIST_LEN {
            let page_idx = offset / PAGE_SIZE;
            if page_idx.is_multiple_of(GAP_SIZE + 1) {
                let mut popped = splice.as_mut().pop();
                expect_true!(
                    popped.release_page() == test_pages[(page_idx / (GAP_SIZE + 1)) as usize],
                    "wrong page"
                );
            } else {
                expect_true!(splice.as_mut().pop().is_empty(), "wrong page");
            }
            offset += PAGE_SIZE;
        }
        expect_true!(splice.is_processed(), "extra pages");

        free_pages(test_pages);
    }

    /// Tests that an empty page splice list can be created.
    #[test]
    fn vmpl_take_empty_test() {
        let mut pl = VmPageList::new();

        stack_pin_init!(let splice = VmPageSpliceList::with_length(PAGE_SIZE));
        expect_ok!(splice.as_mut().add_pages_from(migrate_content, &mut pl, PAGE_SIZE));

        expect_true!(splice.is_finalized());
        expect_false!(splice.is_processed());
        expect_true!(splice.as_mut().pop().is_empty());
        expect_true!(splice.is_processed());
    }

    /// Tests that appending to a splice list works.
    #[test]
    fn vmpl_append_to_splice_list_test() {
        const NUM_PAGES: usize = 5;
        stack_pin_init!(let splice =
            VmPageSpliceList::with_length((NUM_PAGES as u64) * PAGE_SIZE));

        // Append NUM_PAGES to the splice list.
        let mut pages: [Option<VmPagePtr>; NUM_PAGES] = [None; NUM_PAGES];
        for (i, slot) in pages.iter_mut().enumerate() {
            let (page, _pa) = unwrap_ok!(pmm::alloc_page(0));
            *slot = Some(page);
            expect_ok!(
                splice.as_mut().insert((i as u64) * PAGE_SIZE, VmPageOrMarker::from_page(page))
            );
        }

        // Finalize the splice list and verify that it worked.
        splice.as_mut().finalize();
        expect_true!(splice.is_finalized());

        // Pop all of the pages out of the splice list and validate that it contains the expected
        // pages.
        for page in pages {
            let page = page.expect("page allocated above");
            let mut popped = splice.as_mut().pop();
            expect_true!(popped.page() == page);
            let p = popped.release_page();
            // SAFETY: `p` was allocated above and is owned here.
            unsafe { pmm::free_page(p) };
        }
    }

    /// Tests that cleaning up a splice list doesn't blow up.
    #[test]
    fn vmpl_take_cleanup_test() {
        let (page, pa) = unwrap_ok!(pmm::alloc_page(0), "pmm_alloc single page");
        assert_ne!(0, pa.0, "pmm_alloc single page");

        // SAFETY: The page was just allocated and is exclusively owned by this test.
        unsafe {
            page.set_state(VmPageState(page_bindings::vm_page_state::OBJECT));
            page.set_pin_count(0);
        }

        let mut pl = VmPageList::new();
        expect_true!(add_page(&mut pl, page, 0));

        stack_pin_init!(let splice = VmPageSpliceList::with_length(PAGE_SIZE));
        expect_ok!(splice.as_mut().add_pages_from(migrate_content, &mut pl, 0));

        expect_true!(splice.is_finalized());
        expect_false!(splice.is_processed(), "missing page");
    }
}
