// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

/// VMO tests duplicated from vmo_unittest.cc.
#[cfg(ktest)]
#[unittest::suite]
mod vmo_rs {
    use crate::vm::arch_vm_aspace::ARCH_MMU_FLAG_UNCACHED;
    use crate::vm::fault;
    use crate::vm::page::VmPagePtr;
    use crate::vm::physical_page_borrowing_config::ScopedLoaningEnabled;
    use crate::vm::pinned_vm_object::PinnedVmObject;
    use crate::vm::pmm::{self, ALLOC_FLAG_ANY, PmmOptDelayReuse, paddr_to_vm_page};
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_object::{EvictionHint, Resizability, SnapshotType, VmObject};
    use crate::vm::vm_object_paged::VmObjectPaged;
    use crate::vm::vm_object_physical::VmObjectPhysical;
    use crate::vm_unittests::test_helper::make_committed_pager_vmo;
    use page::SIZE as PAGE_SIZE_USIZE;
    use unittest::{
        assert_eq, assert_false, assert_ok, expect_eq, expect_false, expect_ok, expect_true,
        unwrap_ok,
    };
    use zx_status::Status;

    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    /// Helper that tests if all pages in a VMO in the specified range pass the given predicate.
    ///
    /// # Safety
    ///
    /// The caller must ensure that pages attached to the VM object remain attached for the duration
    /// of the callback.
    unsafe fn all_pages_match<F>(vmo: &VmObject, offset: u64, len: u64, mut pred: F) -> bool
    where
        F: FnMut(VmPagePtr) -> bool,
    {
        let mut pred_matches = true;
        let res = vmo.lookup(
            offset,
            len,
            &mut (&mut pred, &mut pred_matches),
            |_, pa, (pred, pred_matches)| {
                let page = paddr_to_vm_page(pa).expect("paddr must map to vm_page");
                if !pred(page) {
                    **pred_matches = false;
                    Err(Status::STOP)
                } else {
                    Err(Status::NEXT)
                }
            },
        );
        res.is_ok() && pred_matches
    }

    /// # Safety
    ///
    /// The caller must ensure that pages attached to `vmo` remain attached during the check.
    unsafe fn pages_in_wired_queue(vmo: &VmObject, offset: u64, len: u64) -> bool {
        // SAFETY: Caller guarantees pages stay attached to the VMO.
        unsafe {
            all_pages_match(vmo, offset, len, |page| pmm::page_queues().debug_page_is_wired(page))
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that pages attached to `vmo` remain attached during the check.
    unsafe fn pages_in_any_anonymous_queue(vmo: &VmObject, offset: u64, len: u64) -> bool {
        // SAFETY: Caller guarantees pages stay attached to the VMO.
        unsafe {
            all_pages_match(vmo, offset, len, |page| {
                pmm::page_queues().debug_page_is_any_anonymous(page)
            })
        }
    }

    /// Creates a vm object.
    #[test]
    fn vmo_create_test() {
        // Creates a vm object.
        let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
        // vmo is not contig
        expect_false!(vmo.is_contiguous());
        // vmo is not resizable
        expect_false!(vmo.is_resizable());
    }

    /// Tests creating a VMO with maximum size and larger than maximum size.
    #[test]
    fn vmo_create_maximum_size() {
        let vmo = VmObjectPaged::create(ALLOC_FLAG_ANY, 0, VmObject::MAX_SIZE);
        // should be ok
        expect_ok!(vmo.map(|_| ()));

        let vmo = VmObjectPaged::create(ALLOC_FLAG_ANY, 0, VmObject::MAX_SIZE + PAGE_SIZE);
        // should be too large
        expect_eq!(Status::result_into_raw(vmo.map(|_| ())), Status::OUT_OF_RANGE.into_raw());
    }

    /// Checks that VMOs must be page aligned sizes.
    #[test]
    fn vmo_unaligned_size_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let alloc_size = 15;
        let result = VmObjectPaged::create(ALLOC_FLAG_ANY, 0, alloc_size);
        assert_eq!(Status::result_into_raw(result.map(|_| ())), Status::INVALID_ARGS.into_raw());
    }

    /// Tests creating a physical VMO and checking its initial properties.
    #[test]
    fn vmo_create_physical_test() {
        // vm page allocation
        let (vm_page, pa) = unwrap_ok!(pmm::alloc_page(0));

        // vmobject creation
        let vmo = unwrap_ok!(VmObjectPhysical::create(pa, PAGE_SIZE_USIZE));
        let cache_policy = vmo.get_mapping_cache_policy();
        // check initial cache policy
        expect_eq!(ARCH_MMU_FLAG_UNCACHED, cache_policy);
        // check contiguous
        expect_true!(vmo.is_contiguous());

        drop(vmo);
        // SAFETY: vm_page was allocated via alloc_page above and is no longer referenced by vmo.
        unsafe { pmm::free_page(vm_page) };
    }

    /// Tests pinning ranges in a physical VMO.
    #[test]
    fn vmo_physical_pin_test() {
        let (vm_page, pa) = unwrap_ok!(pmm::alloc_page(0));

        let vmo = unwrap_ok!(VmObjectPhysical::create(pa, PAGE_SIZE_USIZE));

        // Validate we can pin the range.
        expect_ok!(vmo.commit_range_pinned(0, PAGE_SIZE, false));

        // Pinning out side should fail.
        expect_eq!(
            Status::result_into_raw(vmo.commit_range_pinned(PAGE_SIZE, PAGE_SIZE, false)),
            Status::OUT_OF_RANGE.into_raw()
        );

        // Unpin for physical VMOs does not currently do anything, but still call it to be API correct.
        vmo.unpin(0, PAGE_SIZE);

        drop(vmo);
        // SAFETY: vm_page was allocated via alloc_page above and is no longer referenced by vmo.
        unsafe { pmm::free_page(vm_page) };
    }

    /// Tests pinning and decommitting ranges in a Paged VMO.
    #[test]
    fn vmo_pin_test() {
        // Creates paged VMOs, pins them, and tries operations that should unpin.
        let _scanner_disable = AutoVmScannerDisable::new();

        let alloc_size = PAGE_SIZE * 16;
        for is_loaning_enabled in [false, true] {
            let _loaning_guard = ScopedLoaningEnabled::new(is_loaning_enabled);

            // vmobject creation
            let vmo = unwrap_ok!(VmObjectPaged::create(
                ALLOC_FLAG_ANY,
                VmObjectPaged::RESIZABLE,
                alloc_size
            ));

            // pinning out of range
            expect_eq!(
                Status::result_into_raw(vmo.commit_range_pinned(PAGE_SIZE, alloc_size, false)),
                Status::OUT_OF_RANGE.into_raw()
            );
            // pinning range of len 0
            expect_eq!(
                Status::result_into_raw(vmo.commit_range_pinned(PAGE_SIZE, 0, false)),
                Status::INVALID_ARGS.into_raw()
            );

            // pinning range
            expect_ok!(vmo.commit_range_pinned(PAGE_SIZE, 3 * PAGE_SIZE, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, PAGE_SIZE, 3 * PAGE_SIZE) });

            // decommitting pinned range
            expect_eq!(
                Status::result_into_raw(vmo.decommit_range(PAGE_SIZE, 3 * PAGE_SIZE)),
                Status::BAD_STATE.into_raw()
            );
            // decommitting pinned range
            expect_eq!(
                Status::result_into_raw(vmo.decommit_range(PAGE_SIZE, PAGE_SIZE)),
                Status::BAD_STATE.into_raw()
            );
            // decommitting pinned range
            expect_eq!(
                Status::result_into_raw(vmo.decommit_range(3 * PAGE_SIZE, PAGE_SIZE)),
                Status::BAD_STATE.into_raw()
            );

            vmo.unpin(PAGE_SIZE, 3 * PAGE_SIZE);
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_any_anonymous_queue(&vmo, PAGE_SIZE, 3 * PAGE_SIZE) });

            // decommitting unpinned range
            expect_ok!(vmo.decommit_range(PAGE_SIZE, 3 * PAGE_SIZE));

            // pinning range after decommit
            expect_ok!(vmo.commit_range_pinned(PAGE_SIZE, 3 * PAGE_SIZE, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, PAGE_SIZE, 3 * PAGE_SIZE) });

            // resizing pinned range
            expect_eq!(Status::result_into_raw(vmo.resize(0)), Status::BAD_STATE.into_raw());

            vmo.unpin(PAGE_SIZE, 3 * PAGE_SIZE);

            // resizing unpinned range
            expect_ok!(vmo.resize(0));
        }
    }

    /// Creates contiguous VMOs, pins them, and tries operations that should unpin.
    #[test]
    fn vmo_pin_contiguous_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let alloc_size = PAGE_SIZE * 16;
        for is_loaning_enabled in [false, true] {
            let _loaning_guard = ScopedLoaningEnabled::new(is_loaning_enabled);

            // vmobject creation
            let vmo = unwrap_ok!(VmObjectPaged::create_contiguous(ALLOC_FLAG_ANY, alloc_size, 0));

            // pinning out of range
            expect_eq!(
                Status::result_into_raw(vmo.commit_range_pinned(PAGE_SIZE, alloc_size, false)),
                Status::OUT_OF_RANGE.into_raw()
            );
            // pinning range of len 0
            expect_eq!(
                Status::result_into_raw(vmo.commit_range_pinned(PAGE_SIZE, 0, false)),
                Status::INVALID_ARGS.into_raw()
            );

            // pinning range
            expect_ok!(vmo.commit_range_pinned(PAGE_SIZE, 3 * PAGE_SIZE, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, PAGE_SIZE, 3 * PAGE_SIZE) });

            // decommitting pinned range
            let status = vmo.decommit_range(PAGE_SIZE, 3 * PAGE_SIZE);
            if !is_loaning_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_eq!(Status::result_into_raw(status), Status::BAD_STATE.into_raw());
            }
            let status = vmo.decommit_range(PAGE_SIZE, PAGE_SIZE);
            if !is_loaning_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_eq!(Status::result_into_raw(status), Status::BAD_STATE.into_raw());
            }
            let status = vmo.decommit_range(3 * PAGE_SIZE, PAGE_SIZE);
            if !is_loaning_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_eq!(Status::result_into_raw(status), Status::BAD_STATE.into_raw());
            }

            vmo.unpin(PAGE_SIZE, 3 * PAGE_SIZE);
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, PAGE_SIZE, 3 * PAGE_SIZE) });

            // decommitting unpinned range
            let status = vmo.decommit_range(PAGE_SIZE, 3 * PAGE_SIZE);
            if !is_loaning_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_ok!(status);
            }

            // pinning range after decommit
            expect_ok!(vmo.commit_range_pinned(PAGE_SIZE, 3 * PAGE_SIZE, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, PAGE_SIZE, 3 * PAGE_SIZE) });

            vmo.unpin(PAGE_SIZE, 3 * PAGE_SIZE);
        }
    }

    /// Tests multiple pin calls on the same pages up to the maximum pin count.
    #[test]
    fn vmo_multiple_pin_test() {
        // Creates a page VMO and pins the same pages multiple times.
        let _scanner_disable = AutoVmScannerDisable::new();

        let alloc_size = PAGE_SIZE * 16;
        for is_ppb_enabled in [false, true] {
            let _loaning_guard = ScopedLoaningEnabled::new(is_ppb_enabled);

            // vmobject creation
            let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, alloc_size));

            // pinning whole range
            expect_ok!(vmo.commit_range_pinned(0, alloc_size, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, 0, alloc_size) });
            // pinning subrange
            expect_ok!(vmo.commit_range_pinned(PAGE_SIZE, 4 * PAGE_SIZE, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, 0, alloc_size) });

            for _ in 1..crate::vm::page::OBJECT_MAX_PIN_COUNT {
                // pinning first page max times
                expect_ok!(vmo.commit_range_pinned(0, PAGE_SIZE, false));
            }
            // page is pinned too much
            expect_eq!(
                Status::result_into_raw(vmo.commit_range_pinned(0, PAGE_SIZE, false)),
                Status::UNAVAILABLE.into_raw()
            );

            vmo.unpin(0, alloc_size);
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, PAGE_SIZE, 4 * PAGE_SIZE) });
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe {
                pages_in_any_anonymous_queue(&vmo, 5 * PAGE_SIZE, alloc_size - 5 * PAGE_SIZE)
            });
            // decommitting pinned range
            expect_eq!(
                Status::result_into_raw(vmo.decommit_range(PAGE_SIZE, 4 * PAGE_SIZE)),
                Status::BAD_STATE.into_raw()
            );
            // decommitting unpinned range
            expect_ok!(vmo.decommit_range(5 * PAGE_SIZE, alloc_size - 5 * PAGE_SIZE));

            vmo.unpin(PAGE_SIZE, 4 * PAGE_SIZE);
            // decommitting unpinned range
            expect_ok!(vmo.decommit_range(PAGE_SIZE, 4 * PAGE_SIZE));

            for _ in 2..crate::vm::page::OBJECT_MAX_PIN_COUNT {
                vmo.unpin(0, PAGE_SIZE);
            }
            // decommitting unpinned range
            expect_eq!(
                Status::result_into_raw(vmo.decommit_range(0, PAGE_SIZE)),
                Status::BAD_STATE.into_raw()
            );

            vmo.unpin(0, PAGE_SIZE);
            // decommitting unpinned range
            expect_ok!(vmo.decommit_range(0, PAGE_SIZE));
        }
    }

    /// Creates a contiguous VMO and pins the same pages multiple times.
    #[test]
    fn vmo_multiple_pin_contiguous_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let alloc_size = PAGE_SIZE * 16;
        for is_ppb_enabled in [false, true] {
            let _loaning_guard = ScopedLoaningEnabled::new(is_ppb_enabled);

            // vmobject creation
            let vmo = unwrap_ok!(VmObjectPaged::create_contiguous(ALLOC_FLAG_ANY, alloc_size, 0));

            // pinning whole range
            expect_ok!(vmo.commit_range_pinned(0, alloc_size, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, 0, alloc_size) });
            // pinning subrange
            expect_ok!(vmo.commit_range_pinned(PAGE_SIZE, 4 * PAGE_SIZE, false));
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, 0, alloc_size) });

            for _ in 1..crate::vm::page::OBJECT_MAX_PIN_COUNT {
                // pinning first page max times
                expect_ok!(vmo.commit_range_pinned(0, PAGE_SIZE, false));
            }
            // page is pinned too much
            expect_eq!(
                Status::result_into_raw(vmo.commit_range_pinned(0, PAGE_SIZE, false)),
                Status::UNAVAILABLE.into_raw()
            );

            vmo.unpin(0, alloc_size);
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe { pages_in_wired_queue(&vmo, PAGE_SIZE, 4 * PAGE_SIZE) });
            // SAFETY: Test owns vmo and pages remain attached during check.
            expect_true!(unsafe {
                pages_in_wired_queue(&vmo, 5 * PAGE_SIZE, alloc_size - 5 * PAGE_SIZE)
            });

            let status = vmo.decommit_range(PAGE_SIZE, 4 * PAGE_SIZE);
            if !is_ppb_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_eq!(Status::result_into_raw(status), Status::BAD_STATE.into_raw());
            }
            let status = vmo.decommit_range(5 * PAGE_SIZE, alloc_size - 5 * PAGE_SIZE);
            if !is_ppb_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_ok!(status);
            }

            vmo.unpin(PAGE_SIZE, 4 * PAGE_SIZE);
            let status = vmo.decommit_range(PAGE_SIZE, 4 * PAGE_SIZE);
            if !is_ppb_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_ok!(status);
            }

            for _ in 2..crate::vm::page::OBJECT_MAX_PIN_COUNT {
                vmo.unpin(0, PAGE_SIZE);
            }
            let status = vmo.decommit_range(0, PAGE_SIZE);
            if !is_ppb_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_eq!(Status::result_into_raw(status), Status::BAD_STATE.into_raw());
            }

            vmo.unpin(0, PAGE_SIZE);
            let status = vmo.decommit_range(0, PAGE_SIZE);
            if !is_ppb_enabled {
                expect_eq!(Status::result_into_raw(status), Status::NOT_SUPPORTED.into_raw());
            } else {
                expect_ok!(status);
            }
        }
    }

    /// Verifies that accessing a page in a pager-backed VMO promotes its LRU position.
    #[test]
    fn vmo_move_pages_on_access_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let (vmo, [page]) = unwrap_ok!(make_committed_pager_vmo(false, false));

        // SAFETY: `page` is attached to `vmo`.
        expect_true!(unsafe { pmm::page_queues().debug_page_is_reclaim(page) }.is_some());

        // If we lookup the page then it should be moved to specifically the first page queue.
        let status = vmo.get_page_blocking(0, fault::flag::SW_FAULT);
        expect_ok!(status);
        // SAFETY: `page` is attached to `vmo`.
        let queue = unsafe { pmm::page_queues().debug_page_is_reclaim(page) }
            .expect("page is in reclaim queue");
        expect_eq!(0, queue.0);

        // Rotate the queues and check the page moves.
        pmm::page_queues().rotate_reclaim_queues();
        // SAFETY: `page` is attached to `vmo`.
        let queue = unsafe { pmm::page_queues().debug_page_is_reclaim(page) }
            .expect("page is in reclaim queue");
        expect_eq!(1, queue.0);

        // Touching the page should move it back to the first queue.
        let status = vmo.get_page_blocking(0, fault::flag::SW_FAULT);
        expect_ok!(status);
        // SAFETY: `page` is attached to `vmo`.
        let queue = unsafe { pmm::page_queues().debug_page_is_reclaim(page) }
            .expect("page is in reclaim queue");
        expect_eq!(0, queue.0);

        // Touching pages in a child should also move the page to the front of the queues.
        let child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::OnWrite,
            0,
            PAGE_SIZE,
            true
        ));

        let status = child.get_page_blocking(0, fault::flag::SW_FAULT);
        expect_ok!(status);
        // SAFETY: `page` is attached to `vmo`.
        let queue = unsafe { pmm::page_queues().debug_page_is_reclaim(page) }
            .expect("page is in reclaim queue");
        expect_eq!(0, queue.0);
        pmm::page_queues().rotate_reclaim_queues();
        // SAFETY: `page` is attached to `vmo`.
        let queue = unsafe { pmm::page_queues().debug_page_is_reclaim(page) }
            .expect("page is in reclaim queue");
        expect_eq!(1, queue.0);
        let status = child.get_page_blocking(0, fault::flag::SW_FAULT);
        expect_ok!(status);
        // SAFETY: `page` is attached to `vmo`.
        let queue = unsafe { pmm::page_queues().debug_page_is_reclaim(page) }
            .expect("page is in reclaim queue");
        expect_eq!(0, queue.0);
    }

    /// Tests parent merging and user ID updates when VMO hierarchies collapse.
    #[test]
    fn vmo_parent_merge_test() {
        // Test that a VmObjectPaged that is only referenced by its children gets removed by effectively
        // merging into its parent and re-homing all the children. This should also drop any VmCowPages
        // being held open.
        let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
        // Set a user ID for testing.
        vmo.set_user_id(42);

        let child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            PAGE_SIZE,
            false
        ));
        child.set_user_id(43);

        expect_eq!(vmo.parent_user_id(), 0);
        expect_eq!(vmo.user_id(), 42);
        expect_eq!(child.user_id(), 43);
        expect_eq!(child.parent_user_id(), 42);

        // Dropping the parent should re-home the child to an empty parent.
        drop(vmo);
        expect_eq!(child.user_id(), 43);
        expect_eq!(child.parent_user_id(), 0);

        drop(child);

        // Recreate a more interesting 3 level hierarchy with vmo->child->(child2,child3)

        let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
        vmo.set_user_id(42);
        let child = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            PAGE_SIZE,
            false
        ));
        child.set_user_id(43);
        let child2 = unwrap_ok!(child.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            PAGE_SIZE,
            false
        ));
        child2.set_user_id(44);
        let child3 = unwrap_ok!(child.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            PAGE_SIZE,
            false
        ));
        child3.set_user_id(45);

        expect_eq!(vmo.parent_user_id(), 0);
        expect_eq!(child.parent_user_id(), 42);
        expect_eq!(child2.parent_user_id(), 43);
        expect_eq!(child3.parent_user_id(), 43);

        // Drop the intermediate child, child2+3 should get re-homed to vmo
        drop(child);
        expect_eq!(child2.parent_user_id(), 42);
        expect_eq!(child3.parent_user_id(), 42);
    }

    /// Tests that pinning pager-backed pages retains backlink information.
    #[test]
    fn vmo_pinning_backlink_test() {
        // Disable the page scanner as this test would be flaky if our pages get
        // evicted by someone else.
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a pager-backed VMO with two pages, so we can verify a non-zero offset value.
        let (vmo, [page0, page1]) = unwrap_ok!(make_committed_pager_vmo(false, false));

        // SAFETY: `page0` and `page1` are attached to `vmo`.
        expect_true!(unsafe { pmm::page_queues().debug_page_is_reclaim(page0) }.is_some());
        expect_true!(unsafe { pmm::page_queues().debug_page_is_reclaim(page1) }.is_some());

        // Verify backlink information.
        let cow = vmo.debug_get_cow_pages().expect("cow pages must exist");
        let cow_ptr = cow.as_raw().cast::<core::ffi::c_void>();
        // SAFETY: `page0` and `page1` are attached to `vmo`.
        expect_eq!(unsafe { page0.get_object() }, cow_ptr);
        expect_eq!(unsafe { page0.get_page_offset() }, 0);
        expect_eq!(unsafe { page1.get_object() }, cow_ptr);
        expect_eq!(unsafe { page1.get_page_offset() }, PAGE_SIZE);

        // Pin the pages.
        let status = vmo.commit_range_pinned(0, 2 * PAGE_SIZE, false);
        expect_ok!(status);

        // Pages might get swapped out on pinning if they were loaned. Look them up again.
        let page0 = vmo.debug_get_page(0).expect("page 0 must exist");
        let page1 = vmo.debug_get_page(PAGE_SIZE).expect("page 1 must exist");

        // SAFETY: `page0` and `page1` are attached to `vmo`.
        expect_true!(unsafe { pmm::page_queues().debug_page_is_wired(page0) });
        expect_true!(unsafe { pmm::page_queues().debug_page_is_wired(page1) });
        expect_false!(unsafe { pmm::page_queues().debug_page_is_reclaim(page0) }.is_some());
        expect_false!(unsafe { pmm::page_queues().debug_page_is_reclaim(page1) }.is_some());

        // Moving to the wired queue should retain backlink information.
        // SAFETY: `page0` and `page1` are attached to `vmo`.
        expect_eq!(unsafe { page0.get_object() }, cow_ptr);
        expect_eq!(unsafe { page0.get_page_offset() }, 0);
        expect_eq!(unsafe { page1.get_object() }, cow_ptr);
        expect_eq!(unsafe { page1.get_page_offset() }, PAGE_SIZE);

        // Unpin the pages.
        vmo.unpin(0, 2 * PAGE_SIZE);

        // Pages should be back in the pager queue.
        // SAFETY: `page0` and `page1` are attached to `vmo`.
        expect_false!(unsafe { pmm::page_queues().debug_page_is_wired(page0) });
        expect_false!(unsafe { pmm::page_queues().debug_page_is_wired(page1) });
        expect_true!(unsafe { pmm::page_queues().debug_page_is_reclaim(page0) }.is_some());
        expect_true!(unsafe { pmm::page_queues().debug_page_is_reclaim(page1) }.is_some());

        // Verify backlink information again.
        // SAFETY: `page0` and `page1` are attached to `vmo`.
        expect_eq!(unsafe { page0.get_object() }, cow_ptr);
        expect_eq!(unsafe { page0.get_page_offset() }, 0);
        expect_eq!(unsafe { page1.get_object() }, cow_ptr);
        expect_eq!(unsafe { page1.get_page_offset() }, PAGE_SIZE);
    }

    /// Tests that writing to a VMO does not commit pages in its clone.
    #[test]
    fn vmo_write_does_not_commit_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Create a vmo and commit a page to it.
        let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));

        let val: u64 = 42;
        expect_ok!(vmo.write(0, &val.to_le_bytes()));

        // Create a CoW clone of the vmo.
        let clone = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            PAGE_SIZE,
            false
        ));

        // Querying the page for read in the clone should return it.
        expect_ok!(clone.get_page_blocking(0, 0));

        // Querying for write, without any fault flags, should not work as the page is not committed in
        // the clone.
        expect_eq!(
            Status::result_into_raw(clone.get_page_blocking(0, fault::flag::WRITE)),
            Status::NOT_FOUND.into_raw()
        );

        // Adding a fault flag should cause the lookup to succeed.
        expect_ok!(clone.get_page_blocking(0, fault::flag::WRITE | fault::flag::SW_FAULT));
    }

    /// Tests that decommitting from a contiguous VMO fails when loaning is disabled.
    #[test]
    fn vmo_contiguous_decommit_disabled_test() {
        let _enable_loaning = ScopedLoaningEnabled::new(false);

        let alloc_size = PAGE_SIZE * 16;
        let vmo = unwrap_ok!(VmObjectPaged::create_contiguous(
            ALLOC_FLAG_ANY,
            alloc_size,
            /*alignment_log2*/ 0
        ));

        // decommit fails as expected
        assert_eq!(
            Status::result_into_raw(vmo.decommit_range(PAGE_SIZE, 4 * PAGE_SIZE)),
            Status::NOT_SUPPORTED.into_raw()
        );
        // decommit fails as expected
        assert_eq!(
            Status::result_into_raw(vmo.decommit_range(0, 4 * PAGE_SIZE)),
            Status::NOT_SUPPORTED.into_raw()
        );
        // decommit fails as expected
        assert_eq!(
            Status::result_into_raw(vmo.decommit_range(alloc_size - PAGE_SIZE, PAGE_SIZE)),
            Status::NOT_SUPPORTED.into_raw()
        );
    }

    /// Tests HintRange(AlwaysNeed) evicts loaned pages.
    #[test]
    fn vmo_always_need_evicts_loaned_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        // Depending on which loaned page we get, it may not still be loaned at
        // the time HintRange() is called, so try a few times and make sure we
        // see non-loaned after HintRange() for all the tries.
        let try_count = 30;
        for _try_ordinal in 0..try_count {
            let _enable_loaning = ScopedLoaningEnabled::new(true);

            // create a contiguous VMO so that we are guaranteed to have a place to borrow from
            let contiguous_vmo = unwrap_ok!(VmObjectPaged::create_contiguous(
                ALLOC_FLAG_ANY,
                PAGE_SIZE,
                /*alignment_log2*/ 0
            ));
            assert_ok!(contiguous_vmo.decommit_range(0, PAGE_SIZE));

            // we will replace the only page in vmo with a loaned page
            let (vmo, [before_page]) = unwrap_ok!(make_committed_pager_vmo(
                /*trap_dirty*/ false, /*resizable*/ false
            ));
            let offset = 0;
            let cow_pages = vmo.debug_get_cow_pages().expect("paged VMO has backing cow pages");
            assert_ok!(cow_pages.replace_page_with_loaned(before_page, offset));
            // The call to ReplacePageWithLoaned may loan vmo's page to a VMO that's
            // not contiguous_vmo. So, it might get called back, and the rest of the
            // test must tolerate the vmo's page becoming unloaned at any time.

            // Hint that the page is always needed.
            assert_ok!(vmo.hint_range(0, PAGE_SIZE, EvictionHint::AlwaysNeed));

            // If the page was still loaned, it will be replaced with a non-loaned page now.
            let page =
                vmo.debug_get_page(0).expect("vmo should have a page at offset 0 after hint_range");

            assert_false!(unsafe { page.is_loaned() });
        }
    }

    /// Tests PinnedVmObject creation, move semantics, and RAII unpinning.
    #[test]
    fn vmo_pinned_wrapper_test() {
        // Porting note: This is a verbatim conversion of the C++ test `vmo_pinned_wrapper_test`.
        // It preserves move and assignment constructs from the C++ source that are trivial in Rust.

        {
            let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
            let vmo = VmObjectPaged::into_vm_object(vmo);

            let mut pinned = unwrap_ok!(PinnedVmObject::create(vmo.clone(), 0, PAGE_SIZE, true));
            pinned = unwrap_ok!(PinnedVmObject::create(vmo, 0, PAGE_SIZE, true));
            drop(pinned);
        }

        {
            let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
            let vmo = VmObjectPaged::into_vm_object(vmo);

            let mut pinned = Some(unwrap_ok!(PinnedVmObject::create(vmo, 0, PAGE_SIZE, true)));
            assert!(pinned.is_some());
            let empty: Option<PinnedVmObject> = None;
            pinned = empty;
            drop(pinned);
        }

        {
            let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
            let vmo = VmObjectPaged::into_vm_object(vmo);

            let pinned = unwrap_ok!(PinnedVmObject::create(vmo, 0, PAGE_SIZE, true));
            let mut empty: Option<PinnedVmObject> = None;
            assert!(empty.is_none());
            empty = Some(pinned);
            drop(empty);
        }

        {
            let vmo = unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, PAGE_SIZE));
            let vmo = VmObjectPaged::into_vm_object(vmo);

            let mut pinned1 = unwrap_ok!(PinnedVmObject::create(vmo.clone(), 0, PAGE_SIZE, true));
            let pinned2 = unwrap_ok!(PinnedVmObject::create(vmo, 0, PAGE_SIZE, true));
            pinned1 = pinned2;
            drop(pinned1);
        }
    }

    /// Tests creating a zero-sized always-pinned VMO fails gracefully.
    #[test]
    fn vmo_always_pinned_with_no_pages_test() {
        // Verify that we don't trigger a panic during destruction of an always-pinned, but empty VMO.
        //
        // This is a regression test for https://fxbug.dev/511552403.

        let vmo = VmObjectPaged::create(ALLOC_FLAG_ANY, VmObjectPaged::ALWAYS_PINNED, 0);
        // Note that this call will fail.  That's because we've requested a zero-sized always-pinned
        // VMO, which is not a valid request.  However, under the hood, we'll make it far enough to create
        // the VMO even thought it will be destroyed before the call returns.
        assert_eq!(Status::result_into_raw(vmo.map(|_| ())), Status::INVALID_ARGS.into_raw());
    }

    /// Tests that snapshot creation inherits ever_pinned_ into the hidden parent.
    #[test]
    fn vmo_ever_pinned_hidden_parent_creation_test() {
        // Tests that when creating a bidirectional clone (snapshot) of a once-pinned VMO,
        // the newly created hidden parent correctly inherits the `ever_pinned_` flag.
        let _scanner_disable = AutoVmScannerDisable::new();

        let vmo_size = PAGE_SIZE;

        // Create root VMO.
        let vmo = unwrap_ok!(VmObjectPaged::create(0, 0, vmo_size));

        // Commit a page at offset 0.
        let val: u32 = 0x42;
        assert_ok!(vmo.write(0, &val.to_le_bytes()));

        let cow = vmo.debug_get_cow_pages().expect("vmo has cow pages");

        // Initially, ever_pinned_ should be false.
        expect_true!(cow.should_delay_reuse_on_free() == PmmOptDelayReuse::Default);

        // Pin the page.
        assert_ok!(vmo.commit_range_pinned(0, vmo_size, true));

        // ever_pinned_ should be true.
        expect_true!(cow.should_delay_reuse_on_free() == PmmOptDelayReuse::Yes);

        // Unpin the page.
        vmo.unpin(0, vmo_size);

        // ever_pinned_ should still be true.
        expect_true!(cow.should_delay_reuse_on_free() == PmmOptDelayReuse::Yes);

        // Create a bidirectional clone (snapshot) of the root VMO.
        let _clone = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            vmo_size,
            true
        ));

        // Retrieve the hidden parent.
        let h_cow = cow.debug_get_parent().expect("cow has parent");

        expect_eq!(PmmOptDelayReuse::Yes, h_cow.should_delay_reuse_on_free());
        expect_eq!(PmmOptDelayReuse::Default, cow.should_delay_reuse_on_free());
    }

    /// Tests that page migration into a sibling clone inherits the ever_pinned_ flag.
    #[test]
    fn vmo_ever_pinned_page_migration_test() {
        // Tests that when a once-pinned page is migrated into a sibling clone during copy-on-write
        // page migration, the sibling clone correctly inherits the `ever_pinned_` flag.
        let _scanner_disable = AutoVmScannerDisable::new();

        let vmo_size = PAGE_SIZE;

        // Create root VMO.
        let vmo = unwrap_ok!(VmObjectPaged::create(0, 0, vmo_size));

        // Commit a page at offset 0.
        let val: u32 = 0x42;
        assert_ok!(vmo.write(0, &val.to_le_bytes()));

        let _cow = vmo.debug_get_cow_pages().expect("vmo has cow pages");

        // Pin and unpin the page.
        assert_ok!(vmo.commit_range_pinned(0, vmo_size, true));
        vmo.unpin(0, vmo_size);

        // Create a bidirectional clone (snapshot) of the root VMO.
        let clone = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            vmo_size,
            true
        ));

        let c_cow = VmObject::downcast_paged(clone.clone())
            .expect("is paged")
            .debug_get_cow_pages()
            .expect("clone has cow pages");

        // The sibling clone is created with ever_pinned_ = false.
        expect_true!(c_cow.should_delay_reuse_on_free() == PmmOptDelayReuse::Default);

        // Write to the root VMO to fork the page. The original once-pinned page in the hidden
        // parent is now only visible to the clone.
        let val2: u32 = 0x43;
        assert_ok!(vmo.write(0, &val2.to_le_bytes()));

        // Write to the clone to trigger page migration from the hidden parent to the clone.
        assert_ok!(clone.write(0, &val.to_le_bytes()));

        // The clone should now have ever_pinned_ = true since the once-pinned page was migrated
        // into it.
        expect_eq!(PmmOptDelayReuse::Yes, c_cow.should_delay_reuse_on_free());
    }

    /// Tests that hidden parent collapse and page merge into a child clone inherits ever_pinned_.
    #[test]
    fn vmo_ever_pinned_parent_merge_test() {
        // Tests that when a hidden parent collapses and merges its pages into a child clone, the
        // child clone correctly inherits the `ever_pinned_` flag.
        let _scanner_disable = AutoVmScannerDisable::new();

        let vmo_size = PAGE_SIZE;

        // Create root VMO.
        let vmo = unwrap_ok!(VmObjectPaged::create(0, 0, vmo_size));

        // Commit a page at offset 0.
        let val: u32 = 0x42;
        assert_ok!(vmo.write(0, &val.to_le_bytes()));

        let _cow = vmo.debug_get_cow_pages().expect("vmo has cow pages");

        // Pin and unpin the page.
        assert_ok!(vmo.commit_range_pinned(0, vmo_size, true));
        vmo.unpin(0, vmo_size);

        // Create a bidirectional clone (snapshot) of the root VMO.
        let clone = unwrap_ok!(vmo.create_clone(
            Resizability::NonResizable,
            SnapshotType::Full,
            0,
            vmo_size,
            true
        ));

        let c_cow = VmObject::downcast_paged(clone.clone())
            .expect("is paged")
            .debug_get_cow_pages()
            .expect("clone has cow pages");

        // The sibling clone is created with ever_pinned_ = false.
        expect_true!(c_cow.should_delay_reuse_on_free() == PmmOptDelayReuse::Default);

        // Close the root VMO. This merges the hidden parent's pages into the clone.
        drop(vmo);

        // The clone should now have ever_pinned_ = true since the hidden parent collapsed and
        // merged its pages into the clone.
        expect_eq!(PmmOptDelayReuse::Yes, c_cow.should_delay_reuse_on_free());
    }
}
