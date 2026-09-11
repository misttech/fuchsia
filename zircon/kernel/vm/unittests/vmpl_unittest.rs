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
    use crate::vm::pmm;
    use crate::vm::vm_page_list::{
        BatchInserter, IntervalHandling, ReferenceValue, VmPageList, VmPageListNode, VmPageOrMarker,
    };
    use page::SIZE as PAGE_SIZE_USIZE;
    use unittest::{expect_eq, expect_false, expect_ok, expect_true, unwrap_ok};
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
        let (slot, is_interval) = pl.lookup_or_allocate(offset, IntervalHandling::NoIntervals);
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
        let (slot, is_interval) = pl.lookup_or_allocate(offset, IntervalHandling::NoIntervals);
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
        let (slot, is_interval) = pl.lookup_or_allocate(offset, IntervalHandling::NoIntervals);
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
}
