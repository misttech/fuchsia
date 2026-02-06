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
    ASSERT(cow_pages);
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
    ASSERT(cow_pages);
    auto &tracker = cow_pages->DebugGetContinuousAttributionTracker();
    EXPECT_EQ(2u, tracker.FetchCurrent());  // There are two populated pages.
  }

  END_TEST;
}

UNITTEST_START_TESTCASE(continuous_attribution_tests)
VM_UNITTEST(continuous_attribution_tracker_stub)
VM_UNITTEST(continuous_attribution_tracker_create)
VM_UNITTEST(continuous_attribution_tracker_transfer)
VM_UNITTEST(continuous_attribution_tracker_high_water_mark)
VM_UNITTEST(continuous_attribution_tracker_extreme)
VM_UNITTEST(continuous_attribution_tracker_populate_vmo)
UNITTEST_END_TESTCASE(continuous_attribution_tests, "continuous_attribution",
                      "Tests for populated bytes high-water mark")

}  // namespace
}  // namespace vm_unittest
