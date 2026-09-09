// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/page/size.h>
#include <lib/root_resource_filter_internal.h>
#include <lib/unittest/unittest.h>
#include <zircon/syscalls/resource.h>

#include <ktl/array.h>
#include <ktl/limits.h>

#include <ktl/enforce.h>

static bool root_resource_filter_default() {
  BEGIN_TEST;
  RootResourceFilter filter;

  // Start with asking for access to all of the various resource range kinds.
  // None of these requests should be denied, not even the ones which have no
  // meaningful concept of "range" associated with this.  Unless explicitly
  // disallowed, all requests should default to OK.
  ktl::array kResourceKinds = {ZX_RSRC_KIND_MMIO, ZX_RSRC_KIND_IRQ, ZX_RSRC_KIND_IOPORT,
                               ZX_RSRC_KIND_SMC, ZX_RSRC_KIND_SYSTEM};

  // make sure that if someone adds new resource type, that someone comes back
  // here and adds it to this test.
  static_assert(kResourceKinds.size() == ZX_RSRC_KIND_COUNT - 1,
                "The set of resource kinds has changed and this test needs to be updated.");

  for (const auto kind : kResourceKinds) {
    EXPECT_TRUE(filter.IsRegionAllowed(0, 1, kind));
  }

  END_TEST;
}

static bool root_resource_filter_non_mmio() {
  BEGIN_TEST;
  RootResourceFilter filter;

  // Now manually add some ranges to the set of ranges to be denied.  Test both
  // before and after to make sure that the ranges are allowed before they have
  // been added to the filter, and are properly denied afterwards.
  constexpr size_t kRangeSize = 128;
  struct TestVector {
    uintptr_t base;
    size_t size;
    zx_rsrc_kind_t kind;
  };
  ktl::array kTestVectors = {
      TestVector{0x0040, kRangeSize, ZX_RSRC_KIND_IOPORT},
      TestVector{0x01c0, kRangeSize, ZX_RSRC_KIND_IOPORT},
      TestVector{0x70ef, kRangeSize, ZX_RSRC_KIND_IOPORT},
      TestVector{0x80ef, kRangeSize, ZX_RSRC_KIND_SMC},
      TestVector{0x90ef, kRangeSize, ZX_RSRC_KIND_SMC},
  };

  for (uint32_t pass = 0; pass < 2; ++pass) {
    constexpr size_t kTestSize = 16;
    static_assert(
        (kTestSize * 2) < kRangeSize,
        "test range size must be at least twice as small as the test vector deny-range size.");

    for (const auto& v : kTestVectors) {
      // Entirely before and entirely after ranges should always pass.
      EXPECT_TRUE(filter.IsRegionAllowed(v.base - kTestSize, kTestSize / 2, v.kind));
      EXPECT_TRUE(filter.IsRegionAllowed(v.base + kRangeSize, kTestSize / 2, v.kind));

// Now check ranges which overlap the start, overlap the end, and are
// entirely contained within the deny ranges.  These should succeed on the
// first pass, but fail on the second (after we have added the deny-ranges
// to the filter), or if the kind of range is SMC (currently the
// deny list does not yet apply to the SMC domain).
#ifdef __x86_64__
      bool expected = (pass == 0) || (v.kind == ZX_RSRC_KIND_SMC);
#else
      bool expected = (pass == 0) || (v.kind != ZX_RSRC_KIND_MMIO);
#endif
      EXPECT_EQ(expected, filter.IsRegionAllowed(v.base - (kTestSize / 2), kTestSize, v.kind));
      EXPECT_EQ(expected, filter.IsRegionAllowed(v.base + kTestSize, kTestSize, v.kind));
      EXPECT_EQ(expected,
                filter.IsRegionAllowed(v.base + kRangeSize - (kTestSize / 2), kTestSize, v.kind));
    }

    // If this was the first pass, add in our deny ranges.
    if (pass == 0) {
      for (const auto& v : kTestVectors) {
        filter.AddDenyRegion(v.base, v.size, v.kind);
      }
    }
  }

  END_TEST;
}

static bool root_resource_filter_mmio() {
  BEGIN_TEST;
  RootResourceFilter filter;

  constexpr size_t kHalfPageSize = kPageSize / 2;
  constexpr size_t kPageSizeX2 = kPageSize * 2;
  constexpr size_t kPageSizeX3 = kPageSize * 3;
  constexpr size_t kPageSizeX4 = kPageSize * 4;

  // By default, [0, 0x4000) should be allowed.
  EXPECT_TRUE(filter.IsRegionAllowed(0, kPageSizeX4, ZX_RSRC_KIND_MMIO));

  // Check that we can indeed deny [0, PAGE_SIZE)
  filter.AddDenyRegion(0, kPageSize, ZX_RSRC_KIND_MMIO);
  EXPECT_FALSE(filter.IsRegionAllowed(0, kPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(0, kHalfPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kHalfPageSize, kHalfPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_TRUE(filter.IsRegionAllowed(kPageSize, kPageSizeX3, ZX_RSRC_KIND_MMIO));

  // With page rounding, denying [PAGE_SIZE + 0x100, PAGE_SIZE * 2) should be the same as denying
  // [PAGE_SIZE, PAGE_SIZE * 2), after which [PAGE_SIZE * 2, PAGE_SIZE * 4) should still be allowed.
  filter.AddDenyRegion(kPageSize + 0x100, kPageSize - 0x100, ZX_RSRC_KIND_MMIO);
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSize, kPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSize, 0x100, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSize, kHalfPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSize + kHalfPageSize, kPageSize - kHalfPageSize,
                                      ZX_RSRC_KIND_MMIO));
  EXPECT_TRUE(filter.IsRegionAllowed(kPageSizeX2, kPageSizeX2, ZX_RSRC_KIND_MMIO));

  // With page rounding, denying [PAGE_SIZE * 2, PAGE_SIZE * 2 - 0x100) should be the same as
  // denying [PAGE_SIZE * 2, PAGE_SIZE * 3), after which [PAGE_SIZE * 3, PAGE_SIZE * 4) should still
  // be allowed.
  filter.AddDenyRegion(kPageSizeX2, kPageSize - 0x100, ZX_RSRC_KIND_MMIO);
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSizeX2, kPageSize - 0x100, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSizeX2 - 0x100, 0x100, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSizeX2, kHalfPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(
      filter.IsRegionAllowed(kPageSizeX2 + kHalfPageSize, kHalfPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_TRUE(filter.IsRegionAllowed(kPageSizeX3, kPageSize, ZX_RSRC_KIND_MMIO));

  // With page rounding, denying [PAGE_SIZE * 3 + 0x100, PAGE_SIZE * 4 - 0x100) should be the same
  // as denying [PAGE_SIZE * 3, PAGE_SIZE * 4), after which all of [0x0, PAGE_SIZE * 4) should still
  // be denied.
  filter.AddDenyRegion(kPageSizeX3 + 0x100, kPageSize - 0x200, ZX_RSRC_KIND_MMIO);
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSizeX3, kPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSizeX4 - 0x100, 0x100, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(kPageSizeX3, kHalfPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(
      filter.IsRegionAllowed(kPageSizeX3 + kHalfPageSize, kHalfPageSize, ZX_RSRC_KIND_MMIO));
  EXPECT_FALSE(filter.IsRegionAllowed(0x0, kPageSizeX4, ZX_RSRC_KIND_MMIO));

  END_TEST;
}

UNITTEST_START_TESTCASE(resources)
UNITTEST("test root_resource_filter default behaviour", root_resource_filter_default)
UNITTEST("test root_resource_filter (non-MMIO)", root_resource_filter_non_mmio)
UNITTEST("test root_resource_filter (MMIO)", root_resource_filter_mmio)
UNITTEST_END_TESTCASE(resources, "resource", "Tests for Resource bookkeeping")
