// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

/// Unit tests for the continuous attribution tracker.
#[cfg(ktest)]
#[unittest::suite(name = "continuous_attribution_rust")]
mod tests {
    use crate::vm::continuous_attribution_tracker::{
        ContinuousAttributionTracker, StubContinuousAttributionTracker,
    };
    use crate::vm::page::VmPageDoublyLinkedList;
    use crate::vm::pmm::{self, ALLOC_FLAG_ANY};
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_cow_pages::{CanOverwriteSlot, DeferredOps, VmCowPages};
    use crate::vm::vm_object_paged::VmObjectPaged;
    use core::pin::Pin;
    use fbl::RefPtr;
    use kprint::kprintln;
    use ksync::lock;
    use page::SIZE as PAGE_SIZE_USIZE;
    use pin_init::stack_pin_init;
    use unittest::{assert_ok, expect_eq, unwrap_ok};
    use zx_status::Status;

    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    macro_rules! should_skip_no_feature {
        () => {
            if cfg!(feature = "experimental_continuous_per_vmo_attribution_enabled") {
                false
            } else {
                kprintln!(
                    "Skipping {:s}:{:u}; no support for continuous attribution feature detected.",
                    file!(),
                    line!()
                );
                true
            }
        };
    }

    /// Test that the continuous attribution tracker supports a "stubbed out" state.
    #[test]
    fn stub() {
        let mut tracker = StubContinuousAttributionTracker::new();

        tracker.increment(1);
        tracker.decrement(1);

        tracker.increment(100);
        tracker.decrement(100);

        tracker.increment(100);
        tracker.increment(100);
        tracker.increment(100);

        tracker.decrement(150);
        tracker.decrement(150);

        // Overflow is okay.
        tracker.decrement(3000);

        // Do not call stub FetchCurrent and FetchHwmAndReset methods, as these unconditionally
        // panic.

        let assigned = tracker;
        let moved = assigned;
        let _ = moved;
    }

    /// Test that the initial state of the ContinuousAttributionTracker is zero.
    #[test]
    fn create() {
        let mut tracker = ContinuousAttributionTracker::new();
        expect_eq!(0, tracker.fetch_current());
        expect_eq!(0, tracker.fetch_hwm_and_reset());
    }

    /// Test that the move and assignment transfers data to the new tracker object.
    #[test]
    fn transfer() {
        let mut tracker = ContinuousAttributionTracker::new();

        tracker.increment(5);

        expect_eq!(5, tracker.fetch_current());

        let mut assigned_stats = ContinuousAttributionTracker::new();
        assigned_stats.take_from(&mut tracker);

        // The old one has nothing...
        expect_eq!(0, tracker.fetch_current());

        // but the new one has the data.
        expect_eq!(5, assigned_stats.fetch_current());

        let mut constructed_stats = ContinuousAttributionTracker::new();
        constructed_stats.take_from(&mut assigned_stats);

        // The old one has nothing...
        expect_eq!(0, assigned_stats.fetch_current());

        // but the new one has the data.
        expect_eq!(5, constructed_stats.fetch_current());

        // Test that core::mem::take behaves identically to take_from.
        let mut taken_stats = core::mem::take(&mut constructed_stats);
        expect_eq!(0, constructed_stats.fetch_current());
        expect_eq!(5, taken_stats.fetch_current());

        // Only inspect the high-water mark down here because if we checked before it would have
        // been reset.
        expect_eq!(5, taken_stats.fetch_hwm_and_reset());
    }

    /// Test that the high-water mark accumulates values since last reset.
    #[test]
    fn high_water_mark() {
        let mut tracker = ContinuousAttributionTracker::new();

        tracker.increment(5);
        tracker.decrement(5);

        // The high-water mark is reset by the below.
        expect_eq!(5, tracker.fetch_hwm_and_reset());

        tracker.increment(2);
        tracker.decrement(2);
        tracker.increment(3);
        tracker.decrement(2);
        tracker.decrement(1);
        tracker.increment(2);

        expect_eq!(2, tracker.fetch_current());

        // The high-water mark is 3 even though the current value is 2, since that was the highest
        // since last reset.
        expect_eq!(3, tracker.fetch_hwm_and_reset());
    }

    /// Test that the continuous attribution tracker supports large counts.
    #[test]
    fn extreme() {
        let mut tracker = ContinuousAttributionTracker::new();
        tracker.increment(u32::MAX);
        expect_eq!(u32::MAX, tracker.fetch_current());
    }

    /// Test that failing to add pages updates populated slots count on cleanup.
    #[test]
    fn continuous_attribution_tracker_add_pages() {
        // Test that failing to add a sequence of pages correctly updates the populated slots count
        // on cleanup.
        if should_skip_no_feature!() {
            return true;
        }

        let _disable_scanner = AutoVmScannerDisable::new();

        let vmo: RefPtr<VmObjectPaged> =
            unwrap_ok!(VmObjectPaged::create(ALLOC_FLAG_ANY, 0, 4 * PAGE_SIZE));
        let vmo_cow: RefPtr<VmCowPages> = vmo.debug_get_cow_pages().unwrap();

        expect_eq!(0u32, vmo_cow.debug_get_populated_slots_count());

        assert_ok!(vmo.commit_range(PAGE_SIZE, PAGE_SIZE));

        expect_eq!(1u32, vmo_cow.debug_get_populated_slots_count());

        {
            struct CleanupList<'a>(Pin<&'a mut VmPageDoublyLinkedList>);

            impl Drop for CleanupList<'_> {
                fn drop(&mut self) {
                    // SAFETY: Pages on `self.0` are valid allocated PMM pages that have not
                    // already been freed.
                    unsafe {
                        pmm::free_list(self.0.as_mut());
                    }
                }
            }

            stack_pin_init!(let deferred = DeferredOps::new(&vmo_cow));
            lock!(let guard = vmo_cow.lock());

            stack_pin_init!(let list = VmPageDoublyLinkedList::new());
            let count: usize = 3;
            assert_ok!(pmm::alloc_pages(count, 0, list.as_mut()));
            let mut cleanup = CleanupList(list);

            expect_eq!(
                Status::ALREADY_EXISTS.into_raw(),
                Status::result_into_raw(vmo_cow.add_new_pages_locked(
                    guard.token(),
                    0,
                    cleanup.0.as_mut(),
                    CanOverwriteSlot::Empty,
                    /*zero=*/ true,
                    deferred.as_mut(),
                ))
            );
        }

        expect_eq!(1u32, vmo_cow.debug_get_populated_slots_count());
    }
}
