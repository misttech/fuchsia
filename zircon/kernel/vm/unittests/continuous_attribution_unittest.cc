// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <ktl/source_location.h>
#include <ktl/utility.h>
#include <vm/continuous_attribution_tracker.h>

#include "test_helper.h"

namespace vm_unittest {

namespace {

bool should_skip_no_feature(ktl::source_location caller = ktl::source_location::current()) {
  if constexpr (EXPERIMENTAL_CONTINUOUS_PER_VMO_ATTRIBUTION_ENABLED) {
    return false;
  }
  printf("Skipping %s; no support for continuous attribution feature detected.\n",
         caller.function_name());
  return true;
}

// Test that the continuous attribution tracker supports a "stubbed out" state.
bool continuous_attribution_tracker_stub() {
  BEGIN_TEST;

  StubContinuousAttributionTracker tracker;

  tracker.Increment(1);
  tracker.Decrement(1);

  tracker.Increment(100);
  tracker.Decrement(100);

  tracker.Increment(100);
  tracker.Increment(100);
  tracker.Increment(100);

  tracker.Decrement(150);
  tracker.Decrement(150);

  // Overflow is okay.
  tracker.Decrement(3000);

  // Do not call stub FetchCurrent and FetchHwmAndReset methods, as these unconditionally panic.

  StubContinuousAttributionTracker assigned = ktl::move(tracker);
  StubContinuousAttributionTracker moved(ktl::move(assigned));
  ktl::ignore = moved;

  END_TEST;
}

// Test that the initial state of the ContinuousAttributionTracker is zero.
bool continuous_attribution_tracker_create() {
  BEGIN_TEST;

  ContinuousAttributionTracker tracker;
  EXPECT_EQ(0u, tracker.FetchCurrent());
  EXPECT_EQ(0u, tracker.FetchHwmAndReset());

  END_TEST;
}

// Test that the move and assignment transfers data to the new ContinuousAttributionTracker
// object.
bool continuous_attribution_tracker_transfer() {
  BEGIN_TEST;

  ContinuousAttributionTracker tracker;

  tracker.Increment(5);

  EXPECT_EQ(5u, tracker.FetchCurrent());

  ContinuousAttributionTracker assigned_stats;
  assigned_stats = ktl::move(tracker);

  // The old one has nothing...
  EXPECT_EQ(0u, tracker.FetchCurrent());

  // but the new one has the data.
  EXPECT_EQ(5u, assigned_stats.FetchCurrent());

  ContinuousAttributionTracker constructed_stats(ktl::move(assigned_stats));

  // The old one has nothing...
  EXPECT_EQ(0u, assigned_stats.FetchCurrent());

  // but the new one has the data.
  EXPECT_EQ(5u, constructed_stats.FetchCurrent());

  // Only inspect the high-water mark down here because if we checked before it would have been
  // reset.
  EXPECT_EQ(5u, constructed_stats.FetchHwmAndReset());

  END_TEST;
}

// Test that the high-water mark accumulates values since last reset.
bool continuous_attribution_tracker_high_water_mark() {
  BEGIN_TEST;

  ContinuousAttributionTracker tracker;

  tracker.Increment(5);
  tracker.Decrement(5);

  // The high-water mark is reset by the below.
  EXPECT_EQ(5u, tracker.FetchHwmAndReset());

  tracker.Increment(2);
  tracker.Decrement(2);
  tracker.Increment(3);
  tracker.Decrement(2);
  tracker.Decrement(1);
  tracker.Increment(2);

  EXPECT_EQ(2u, tracker.FetchCurrent());

  // The high-water mark is 3 even though the current value is 2, since that was the highest since
  // last reset.
  EXPECT_EQ(3u, tracker.FetchHwmAndReset());

  END_TEST;
}

// Test that the continuous attribution tracker supports large counts.
bool continuous_attribution_tracker_extreme() {
  BEGIN_TEST;

  ContinuousAttributionTracker tracker;
  tracker.Increment(ktl::numeric_limits<uint32_t>::max());
  EXPECT_EQ(ktl::numeric_limits<uint32_t>::max(), tracker.FetchCurrent());

  END_TEST;
}

bool continuous_attribution_tracker_populate_vmo() {
  BEGIN_TEST;

  if (should_skip_no_feature()) {
    END_TEST;
  }

  AutoVmScannerDisable disable_scanner;

  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0u, 3 * kPageSize, &vmo);
  ASSERT_OK(status);

  {
    fbl::RefPtr<VmCowPages> cow_pages = vmo->DebugGetCowPages();
    ASSERT_NONNULL(cow_pages);
    auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();
    EXPECT_EQ(0u, tracker.FetchCurrent());  // There is no content.
  }

  // Write a non-zero value to the first two pages.
  {
    fbl::AllocChecker ac;
    fbl::Vector<uint8_t> a;
    a.resize(2 * kPageSize, 42, &ac);
    ASSERT_TRUE(ac.check());
    EXPECT_EQ(ZX_OK, vmo->Write(a.data(), 0, a.size()));
  }

  {
    fbl::RefPtr<VmCowPages> cow_pages = vmo->DebugGetCowPages();
    ASSERT_NONNULL(cow_pages);
    auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();
    EXPECT_EQ(2u, tracker.FetchCurrent());  // There are two populated pages.
  }

  END_TEST;
}

// Test that the correct tracker is provided to the unidirectional clone and parent.
bool continuous_attribution_tracker_unidirectional_child() {
  BEGIN_TEST;

  if (should_skip_no_feature()) {
    END_TEST;
  }
  AutoVmScannerDisable disable_scanner;

  fbl::RefPtr<VmObjectPaged> vmo;
  ASSERT_OK(make_partially_committed_pager_vmo(3, /*committed_pages=*/2, /*trap_dirty=*/false,
                                               /*resizable=*/false, false, nullptr, &vmo));

  // Create a unidirectional clone.
  fbl::RefPtr<VmObject> child_vmo_no_paged;
  ASSERT_OK(vmo->CreateClone(Resizability::NonResizable, SnapshotType::OnWrite, /*offset=*/0,
                             /*size=*/3 * kPageSize, /*copy_name=*/false, &child_vmo_no_paged));

  // Assert there is no hidden parent (true unidirectional)
  {
    fbl::RefPtr<VmCowPages> hidden_parent = vmo->DebugGetCowPages()->DebugGetParent();
    ASSERT_NULL(hidden_parent);
  }

  {
    fbl::RefPtr<VmCowPages> cow_pages = vmo->DebugGetCowPages();
    ASSERT_NONNULL(cow_pages);

    auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();

    // There are two pages committed in the parent.
    EXPECT_EQ(2u, tracker.FetchCurrent());
  }

  {
    VmObjectPaged *child = DownCastVmObject<VmObjectPaged>(child_vmo_no_paged.get());
    ASSERT_NONNULL(child);
    fbl::RefPtr<VmCowPages> cow_pages = child->DebugGetCowPages();
    ASSERT_NONNULL(cow_pages);

    auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();

    // There are no parent content markers in this hierarchy to track, as intended.
    EXPECT_EQ(0u, tracker.FetchCurrent());
  }

  END_TEST;
}

// Test that the correct tracker is provided to the bidirectional clone and parent.
bool continuous_attribution_tracker_bidirectional_child() {
  BEGIN_TEST;

  if (should_skip_no_feature()) {
    END_TEST;
  }
  AutoVmScannerDisable disable_scanner;

  fbl::RefPtr<VmObjectPaged> vmo;
  ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, 3 * kPageSize, &vmo));

  // Write a non-zero value to the first two pages.
  {
    fbl::AllocChecker ac;
    fbl::Vector<uint8_t> a;
    a.resize(2 * kPageSize, 42, &ac);
    ASSERT_TRUE(ac.check());
    EXPECT_EQ(ZX_OK, vmo->Write(a.data(), 0, a.size()));
  }

  // Create a bidirectional clone.
  fbl::RefPtr<VmObject> child_vmo_no_paged;
  ASSERT_OK(vmo->CreateClone(Resizability::NonResizable, SnapshotType::Full, /*offset=*/0,
                             /*size=*/3 * kPageSize, /*copy_name=*/false, &child_vmo_no_paged));

  // Assert there is a hidden parent (true bidirectional)
  fbl::RefPtr<VmCowPages> hidden_parent = vmo->DebugGetCowPages()->DebugGetParent();
  ASSERT_NONNULL(hidden_parent);

  {
    fbl::RefPtr<VmCowPages> cow_pages = vmo->DebugGetCowPages();
    ASSERT_NONNULL(cow_pages);

    auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();

    EXPECT_EQ(2u, tracker.FetchCurrent());
  }

  {
    VmObjectPaged *child = DownCastVmObject<VmObjectPaged>(child_vmo_no_paged.get());
    ASSERT_NONNULL(child);
    fbl::RefPtr<VmCowPages> cow_pages = child->DebugGetCowPages();
    ASSERT_NONNULL(cow_pages);

    auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();

    EXPECT_EQ(2u, tracker.FetchCurrent());
  }

  auto &tracker = hidden_parent->DebugGetContinuousAttributionTracker();

  // There are two pages committed in the parent.
  EXPECT_EQ(2u, tracker.FetchCurrent());

  END_TEST;
}

// Test that zeroing an anonymous VMO decreases the populated slots count.
bool continuous_attribution_tracker_zero_anonymous() {
  BEGIN_TEST;

  if (should_skip_no_feature()) {
    END_TEST;
  }

  AutoVmScannerDisable disable_scanner;

  fbl::RefPtr<VmObjectPaged> vmo;
  ASSERT_OK(VmObjectPaged::Create(PMM_ALLOC_FLAG_ANY, 0, 3 * kPageSize, &vmo));

  fbl::RefPtr<VmCowPages> cow_pages = vmo->DebugGetCowPages();
  ASSERT_NONNULL(cow_pages);
  auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();

  // Write a non-zero value to the first two pages.
  {
    fbl::AllocChecker ac;
    fbl::Vector<uint8_t> a;
    a.resize(2 * kPageSize, 42, &ac);
    ASSERT_TRUE(ac.check());
    EXPECT_EQ(ZX_OK, vmo->Write(a.data(), 0, a.size()));
  }

  EXPECT_EQ(2u, tracker.FetchCurrent());

  // Clear out one page, so that afterwards the VMO will only have one populated page.
  {
    __UNINITIALIZED MultiPageRequest page_request;
    VmCowPages::DeferredOps deferred(cow_pages.get());
    Guard<CriticalMutex> guard{cow_pages->lock()};
    VmCowRange range(0, kPageSize);
    // Directly call the lower-level interface, as opposed to the method on VmObject. The
    // attribution for the higher-level method is incomplete.
    auto [status, zeroed_bytes] =
        cow_pages->ZeroPagesLocked(range, /*dirty_track=*/false, deferred, &page_request);
    EXPECT_EQ(kPageSize, zeroed_bytes);
    EXPECT_OK(status);
  }

  EXPECT_EQ(1u, tracker.FetchCurrent());

  END_TEST;
}

// Test that zeroing a pager-backed VMO decreases the populated slots count.
bool continuous_attribution_tracker_zero_pager_backed() {
  BEGIN_TEST;

  if (should_skip_no_feature()) {
    END_TEST;
  }

  AutoVmScannerDisable disable_scanner;

  fbl::RefPtr<VmObjectPaged> vmo;
  ASSERT_OK(make_partially_committed_pager_vmo(3, /*committed_pages=*/2, /*trap_dirty=*/false,
                                               /*resizable=*/false, false, nullptr, &vmo));

  fbl::RefPtr<VmCowPages> cow_pages = vmo->DebugGetCowPages();
  ASSERT_NONNULL(cow_pages);
  auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();

  EXPECT_EQ(2u, tracker.FetchCurrent());

  // Clear out one page, so that afterwards the VMO will only have one populated page.
  {
    __UNINITIALIZED MultiPageRequest page_request;
    VmCowPages::DeferredOps deferred(cow_pages.get());
    Guard<CriticalMutex> guard{cow_pages->lock()};
    VmCowRange range(0, kPageSize);
    // Directly call the lower-level interface, as opposed to the method on VmObject. The
    // attribution for the higher-level method is incomplete.
    auto [status, zeroed_bytes] =
        cow_pages->ZeroPagesLocked(range, /*dirty_track=*/false, deferred, &page_request);
    EXPECT_EQ(kPageSize, zeroed_bytes);
    EXPECT_OK(status);
  }

  EXPECT_EQ(1u, tracker.FetchCurrent());

  END_TEST;
}

// Test that zeroing a child of a pager-backed VMO correctly updates the populated bytes count.
bool continuous_attribution_tracker_zero_pager_clone() {
  BEGIN_TEST;

  if (should_skip_no_feature()) {
    END_TEST;
  }

  AutoVmScannerDisable disable_scanner;

  fbl::RefPtr<VmObjectPaged> vmo;
  ASSERT_OK(make_partially_committed_pager_vmo(3, /*committed_pages=*/2, /*trap_dirty=*/false,
                                               /*resizable=*/false, false, nullptr, &vmo));

  fbl::RefPtr<VmObject> child_vmo_no_paged;
  ASSERT_OK(vmo->CreateClone(Resizability::NonResizable, SnapshotType::Modified, /*offset=*/0,
                             /*size=*/3 * kPageSize, /*copy_name=*/false, &child_vmo_no_paged));

  fbl::RefPtr<VmObjectPaged> child = DownCastVmObject<VmObjectPaged>(child_vmo_no_paged);
  ASSERT_NONNULL(child);

  fbl::RefPtr<VmCowPages> child_cow_pages = child->DebugGetCowPages();
  auto &child_tracker = child_cow_pages->DebugGetContinuousAttributionTracker();

  EXPECT_EQ(0u, child_tracker.FetchCurrent());

  ASSERT_OK(child->CommitRange(0, 2 * kPageSize));

  EXPECT_EQ(2u, child_tracker.FetchCurrent());

  // Clear out one page, so that afterwards the VMO will only have one populated page.
  {
    __UNINITIALIZED MultiPageRequest page_request;
    VmCowPages::DeferredOps deferred(child_cow_pages.get());
    Guard<CriticalMutex> guard{child_cow_pages->lock()};
    VmCowRange range(0, kPageSize);
    // Directly call the lower-level interface, as opposed to the method on VmObject. The
    // attribution for the higher-level method is incomplete.
    auto [status, zeroed_bytes] =
        child_cow_pages->ZeroPagesLocked(range, /*dirty_track=*/false, deferred, &page_request);
    EXPECT_EQ(kPageSize, zeroed_bytes);
    EXPECT_OK(status);
  }

  EXPECT_EQ(1u, child_tracker.FetchCurrent());

  END_TEST;
}

UNITTEST_START_TESTCASE(continuous_attribution_tests)
VM_UNITTEST(continuous_attribution_tracker_stub)
VM_UNITTEST(continuous_attribution_tracker_create)
VM_UNITTEST(continuous_attribution_tracker_transfer)
VM_UNITTEST(continuous_attribution_tracker_high_water_mark)
VM_UNITTEST(continuous_attribution_tracker_extreme)
VM_UNITTEST(continuous_attribution_tracker_populate_vmo)
VM_UNITTEST(continuous_attribution_tracker_unidirectional_child)
VM_UNITTEST(continuous_attribution_tracker_bidirectional_child)
VM_UNITTEST(continuous_attribution_tracker_zero_anonymous)
VM_UNITTEST(continuous_attribution_tracker_zero_pager_backed)
VM_UNITTEST(continuous_attribution_tracker_zero_pager_clone)
UNITTEST_END_TESTCASE(continuous_attribution_tests, "continuous_attribution",
                      "Tests for populated bytes high-water mark")

}  // namespace
}  // namespace vm_unittest
