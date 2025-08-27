// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <lib/fit/defer.h>
#include <lib/unittest/unittest.h>
#include <zircon/errors.h>
#include <zircon/listnode.h>
#include <zircon/types.h>

#include <cassert>
#include <cstddef>
#include <cstdint>

#include <arch/defines.h>
#include <fbl/paged_view.h>
#include <ktl/array.h>
#include <vm/page.h>
#include <vm/pmm.h>
#include <vm/vm_aspace.h>

namespace {

struct Record {
  uint8_t a;
  uint64_t b;
  uint8_t c;

  void fill(int v) {
    a = static_cast<uint8_t>(v % 19);
    b = 200000ul + v;
    c = static_cast<uint8_t>(v % 99);
  }

  void zero() {
    a = 0;
    b = 0;
    c = 0;
  }

  bool is_zero() const { return (a == 0) && (b == 0) && (c == 0); }

  bool check(int v) const { return (a == (v % 19)) && (b == 200000ul + v) && (c == (v % 99)); }
};

constexpr int record_count = PAGE_SIZE / sizeof(Record);  // 170.

zx_status_t alloc_pages(size_t count, list_node_t* pages) {
  list_initialize(pages);
  zx_status_t status = pmm_alloc_pages(count, 0, pages);
  if (status != ZX_OK) {
    return ZX_ERR_NO_MEMORY;
  }
  return ZX_OK;
}

vm_page_t* next_page(list_node_t* list, vm_page_t* current) {
  return list_next_type(list, &current->queue_node, vm_page_t, queue_node);
}

template <size_t n>
ktl::array<vm_page_t*, n> get_pages(list_node_t* list) {
  vm_page_t* curr = list_peek_head_type(list, vm_page_t, queue_node);
  ktl::array<vm_page_t*, n> out;
  int ix = 0;
  while (true) {
    out[ix] = curr;
    curr = next_page(list, curr);
    if (curr == nullptr)
      break;
    ix += 1;
  }
  return out;
}

bool simple_test() {
  BEGIN_TEST;

  constexpr int page_count = 3;
  list_node_t pages;
  auto result = alloc_pages(page_count, &pages);
  ASSERT_OK(result);
  auto cleanup = fit::defer([&pages]() { pmm_free(&pages); });

  fbl::PagedView<Record> paged_view(&pages);

  // 4096 = 170*sizeof(Record) + 16. Note: sizeof(Record) = 24.
  EXPECT_EQ(record_count, fbl::PagedView<Record>::kMaxCount);

  // Fill the pages using the record() + next_record() interface.
  int counter = 0;
  do {
    paged_view.record().fill(counter);
    counter++;
  } while (paged_view.next_record());

  EXPECT_EQ(record_count * page_count, counter);

  paged_view.reset();

  // Check the data using the ktl::span + next_page() interface.
  counter = 0;
  int pc = 0;
  do {
    pc++;
    for (const auto& r : paged_view.span()) {
      ASSERT_TRUE(r.check(counter));
      counter++;
    }
  } while (paged_view.next_page());

  EXPECT_EQ(page_count, pc);
  EXPECT_EQ(record_count * page_count, counter);

  auto pages_arr = get_pages<3>(&pages);
  paged_view.reset();

  // Move to the 3rd page.
  auto page = paged_view.move((record_count * 2) + 10);
  EXPECT_EQ(pages_arr[2], page);
  EXPECT_EQ(uint32_t(10), paged_view.current_index());

  // Move to the first page.
  page = paged_view.move(-(record_count * 2) - 5);
  EXPECT_EQ(pages_arr[0], page);
  EXPECT_EQ(uint32_t(5), paged_view.current_index());

  // Move too far. It should fail.
  page = paged_view.move(10000);
  EXPECT_EQ(nullptr, page);
  EXPECT_EQ(uint32_t(5), paged_view.current_index());

  END_TEST;
}

bool marker_test() {
  BEGIN_TEST;

  constexpr int page_count = 2;
  list_node_t pages;
  auto result = alloc_pages(page_count, &pages);
  ASSERT_OK(result);
  auto cleanup = fit::defer([&pages]() { pmm_free(&pages); });

  fbl::PagedView<Record> paged_view(&pages);

  // Zero all the data using the for_each_page interface.
  size_t count = 0;
  paged_view.for_each_page([&count](ktl::span<Record> span) {
    for (auto& r : span) {
      r.zero();
      count += 1;
    }
  });

  EXPECT_EQ(page_count * fbl::PagedView<Record>::kMaxCount, size_t(count));

  constexpr size_t records_skipped = 5;
  constexpr size_t records_filled = 21;

  paged_view.reset();

  // Skip some zeroed records.
  for (size_t c = 0; c < records_skipped; ++c) {
    ASSERT_TRUE(paged_view.next_record());
  }

  // Get a marker, from top of page to this point.
  auto zero_marker = paged_view.get_marker_from_start();
  EXPECT_FALSE(zero_marker.is_null())

  // Fill a portion of the first page.
  for (size_t c = 0; c < records_filled; ++c) {
    paged_view.record().fill(17);
    ASSERT_TRUE(paged_view.next_record());
  }

  // Get a span to the rest of the page, check they are still zeros.
  count = 0;
  for (auto& r : paged_view.span_last()) {
    EXPECT_TRUE(r.is_zero());
    count += 1;
  }
  EXPECT_EQ(record_count - (records_skipped + records_filled), count);

  // Get a marker to the last position and reset.
  auto fill_marker = paged_view.reset();
  EXPECT_FALSE(fill_marker.is_null())

  // At this point we have 5 zeroed records, followed by 21 filled records, followed by zeros.
  // The makers are used to get a reduced span relative to the top of the page.
  auto span_z = paged_view.span(zero_marker);
  EXPECT_EQ(records_skipped, span_z.size());
  auto span_f = paged_view.span(fill_marker);
  EXPECT_EQ(records_skipped + records_filled, span_f.size());

  EXPECT_TRUE(paged_view.marker_in_page(zero_marker));
  EXPECT_TRUE(paged_view.marker_in_page(fill_marker));

  // The makers don't say anything about other pages and therefore other pages spans
  // are returned full.
  ASSERT_TRUE(paged_view.next_page() != nullptr);
  EXPECT_FALSE(paged_view.marker_in_page(zero_marker));
  EXPECT_FALSE(paged_view.marker_in_page(fill_marker));
  EXPECT_EQ(fbl::PagedView<Record>::kMaxCount, paged_view.span(zero_marker).size());
  EXPECT_EQ(fbl::PagedView<Record>::kMaxCount, paged_view.span(fill_marker).size());

  // You can use the marker to reset to the appropriate page and index.
  paged_view.reset(zero_marker);
  EXPECT_TRUE(paged_view.marker_in_page(zero_marker));

  // Check the records from the top. They should be zero.
  count = 0;
  for (auto& r : paged_view.span_first()) {
    EXPECT_TRUE(r.is_zero());
    count += 1;
  }
  EXPECT_EQ(records_skipped, count);

  // Now check the records from the zero marker to the end of the page. From
  // them the first 21 should be filled.
  count = 0;
  for (auto& r : paged_view.span_last().first(records_filled)) {
    EXPECT_TRUE(r.check(17));
    count += 1;
  }
  EXPECT_EQ(records_filled, count);

  // Now move to the last filled record, and get a marker from there to the end.
  paged_view.reset(fill_marker);
  auto zero_to_end = paged_view.get_marker_to_end();

  // Check the rest are zero.
  count = 0;
  paged_view.for_each_page(zero_to_end, [&count](ktl::span<Record> span) {
    for (const auto& r : span) {
      r.is_zero();
      count += 1;
    }
  });

  EXPECT_EQ((record_count * page_count) - (records_filled + records_skipped), count);

  END_TEST;
}

}  // namespace

UNITTEST_START_TESTCASE(paged_view)
UNITTEST("simple", simple_test)
UNITTEST("marker", marker_test)
UNITTEST_END_TESTCASE(paged_view, "paged_view", "Paged View test")
