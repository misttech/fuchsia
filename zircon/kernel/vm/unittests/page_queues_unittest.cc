// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <vm/page_queues.h>

#include "test_helper.h"

namespace vm_unittest {

static bool pq_add_remove() {
  BEGIN_TEST;

  PageQueues pq;

  // Pretend we have an allocated page
  vm_page_t test_page = {};
  test_page.set_state(vm_page_state::OBJECT);

  // Need a VMO to claim our pages are in
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::Create(0, 0, kPageSize, &vmo);
  ASSERT_EQ(ZX_OK, status);

  // Put the page in each queue and make sure it shows up
  pq.SetWired(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsWired(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.wired = 1}));

  pq.Remove(&test_page);
  EXPECT_FALSE(pq.DebugPageIsWired(&test_page));
  EXPECT_FALSE(pq.DebugPageIsAnonymous(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  pq.SetAnonymous(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsAnonymous(&test_page));
  if (pq.ReclaimIsOnlyPagerBacked()) {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.anonymous = 1}));
  } else {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));
  }

  pq.Remove(&test_page);
  EXPECT_FALSE(pq.DebugPageIsAnonymous(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  // Need a pager VMO to claim our page is in.
  status = make_uncommitted_pager_vmo(1, false, false, &vmo);
  ASSERT_OK(status);

  pq.SetReclaim(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));

  pq.Remove(&test_page);
  EXPECT_FALSE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  pq.SetPagerBackedDirty(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.pager_backed_dirty = 1}));

  pq.Remove(&test_page);
  EXPECT_FALSE(pq.DebugPageIsPagerBackedDirty(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  END_TEST;
}

static bool pq_move_queues() {
  BEGIN_TEST;

  PageQueues pq;

  // Pretend we have an allocated page
  vm_page_t test_page = {};
  test_page.set_state(vm_page_state::OBJECT);

  // Need a VMO to claim our pages are in
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::Create(0, 0, kPageSize, &vmo);
  ASSERT_EQ(ZX_OK, status);

  // Move the page between queues.
  pq.SetWired(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsWired(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.wired = 1}));

  pq.MoveToAnonymous(&test_page);
  EXPECT_FALSE(pq.DebugPageIsWired(&test_page));
  EXPECT_TRUE(pq.DebugPageIsAnonymous(&test_page));
  if (pq.ReclaimIsOnlyPagerBacked()) {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.anonymous = 1}));
  } else {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));
  }
  pq.Remove(&test_page);

  // Now try some pager backed queues.
  status = make_uncommitted_pager_vmo(1, false, false, &vmo);
  ASSERT_OK(status);

  pq.SetReclaim(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));

  pq.MoveToPagerBackedDirty(&test_page);
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.pager_backed_dirty = 1}));

  pq.MoveToReclaim(&test_page);
  EXPECT_TRUE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));

  pq.MoveToReclaimDontNeed(&test_page);
  EXPECT_FALSE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim_isolate = 1}));

  // Verify that the DontNeed page is first in line for eviction.
  auto backlink = pq.PeekIsolate(PageQueues::kNumReclaim - 1);
  EXPECT_TRUE(backlink != ktl::nullopt && backlink->page == &test_page);

  pq.MoveToWired(&test_page);
  EXPECT_FALSE(pq.DebugPageIsReclaimIsolate(&test_page));
  EXPECT_FALSE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.DebugPageIsWired(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.wired = 1}));

  pq.Remove(&test_page);
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  END_TEST;
}

static bool pq_move_self_queue() {
  BEGIN_TEST;

  PageQueues pq;

  // Pretend we have an allocated page
  vm_page_t test_page = {};
  test_page.set_state(vm_page_state::OBJECT);

  // Need a VMO to claim our pages are in
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = VmObjectPaged::Create(0, 0, kPageSize, &vmo);
  ASSERT_EQ(ZX_OK, status);

  // Move the page into the queue it is already in.
  pq.SetWired(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsWired(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.wired = 1}));

  pq.MoveToWired(&test_page);
  EXPECT_TRUE(pq.DebugPageIsWired(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.wired = 1}));

  pq.Remove(&test_page);
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  pq.SetAnonymous(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsAnonymous(&test_page));
  if (pq.ReclaimIsOnlyPagerBacked()) {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.anonymous = 1}));
  } else {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));
  }

  pq.MoveToAnonymous(&test_page);
  EXPECT_TRUE(pq.DebugPageIsAnonymous(&test_page));
  if (pq.ReclaimIsOnlyPagerBacked()) {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.anonymous = 1}));
  } else {
    EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));
  }

  pq.Remove(&test_page);
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  // Now try some pager backed queues.
  status = make_uncommitted_pager_vmo(1, false, false, &vmo);
  ASSERT_OK(status);

  pq.SetReclaim(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));

  pq.MoveToReclaim(&test_page);
  EXPECT_TRUE(pq.DebugPageIsReclaim(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));

  pq.Remove(&test_page);
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  pq.SetPagerBackedDirty(&test_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.pager_backed_dirty = 1}));

  pq.MoveToPagerBackedDirty(&test_page);
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&test_page));
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.pager_backed_dirty = 1}));

  pq.Remove(&test_page);
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){}));

  END_TEST;
}

static bool pq_rotate_queue() {
  BEGIN_TEST;

  PageQueues pq;

  pq.SetActiveRatioMultiplier(0);
  pq.StartThreads(0, ZX_TIME_INFINITE);

  // Pretend we have a few allocated pages.
  vm_page_t wired_page = {};
  vm_page_t clean_pager_page = {};
  vm_page_t dirty_pager_page = {};
  wired_page.set_state(vm_page_state::OBJECT);
  clean_pager_page.set_state(vm_page_state::OBJECT);
  dirty_pager_page.set_state(vm_page_state::OBJECT);

  // Need a VMO to claim our pages are in.
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = make_uncommitted_pager_vmo(1, false, false, &vmo);
  ASSERT_OK(status);

  // Put the pages in and validate initial state.
  pq.SetWired(&wired_page, vmo->DebugGetCowPages().get(), 0);
  pq.SetReclaim(&clean_pager_page, vmo->DebugGetCowPages().get(), 0);
  pq.SetPagerBackedDirty(&dirty_pager_page, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsWired(&wired_page));
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&dirty_pager_page));
  size_t queue;
  EXPECT_TRUE(pq.DebugPageIsReclaim(&clean_pager_page, &queue));
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {1, 0, 0, 0, 0, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){1, 0}));
  EXPECT_EQ(queue, 0u);

  // Gradually rotate the queue.
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.DebugPageIsWired(&wired_page));
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&dirty_pager_page));
  EXPECT_TRUE(pq.DebugPageIsReclaim(&clean_pager_page, &queue));
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 1, 0, 0, 0, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){1, 0}));
  EXPECT_EQ(queue, 1u);

  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 0, 1, 0, 0, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){0, 1}));
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 0, 0, 1, 0, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){0, 1}));
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 0, 0, 0, 1, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 0, 0, 0, 0, 1, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 0, 0, 0, 0, 0, 1, 0}, .pager_backed_dirty = 1, .wired = 1}));
  pq.RotateReclaimQueues();
  // Further rotations might cause the page to be visible in the same queue, or the isolate,
  // depending on whether the lru processing already ran in preparation of the next aging event.
  const PageQueues::Counts counts_last = (PageQueues::Counts){
      .reclaim = {0, 0, 0, 0, 0, 0, 0, 1}, .pager_backed_dirty = 1, .wired = 1};
  const PageQueues::Counts counts_isolate =
      (PageQueues::Counts){.reclaim = {0, 0, 0, 0, 0, 0, 0, 0},
                           .reclaim_isolate = 1,
                           .pager_backed_dirty = 1,
                           .wired = 1};
  PageQueues::Counts counts = pq.QueueCounts();
  EXPECT_TRUE(counts == counts_last || counts == counts_isolate);

  // Further rotations should not move the page.
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.DebugPageIsWired(&wired_page));
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&dirty_pager_page));
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&clean_pager_page));
  counts = pq.QueueCounts();
  EXPECT_TRUE(counts == counts_isolate);
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){0, 1}));

  // Moving the page should bring it back to the first queue.
  pq.MoveToReclaim(&clean_pager_page);
  EXPECT_TRUE(pq.DebugPageIsWired(&wired_page));
  EXPECT_TRUE(pq.DebugPageIsPagerBackedDirty(&dirty_pager_page));
  EXPECT_TRUE(pq.DebugPageIsReclaim(&clean_pager_page));
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {1, 0, 0, 0, 0, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){1, 0}));

  // Just double check two rotations.
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 1, 0, 0, 0, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){1, 0}));
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){
                  .reclaim = {0, 0, 1, 0, 0, 0, 0, 0}, .pager_backed_dirty = 1, .wired = 1}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){0, 1}));

  pq.Remove(&wired_page);
  pq.Remove(&clean_pager_page);
  pq.Remove(&dirty_pager_page);

  END_TEST;
}

static bool pq_toggle_dont_need_queue() {
  BEGIN_TEST;

  PageQueues pq;

  pq.SetActiveRatioMultiplier(0);
  pq.StartThreads(0, ZX_TIME_INFINITE);

  // Pretend we have a couple of allocated pager-backed pages.
  vm_page_t page1 = {};
  vm_page_t page2 = {};
  page1.set_state(vm_page_state::OBJECT);
  page2.set_state(vm_page_state::OBJECT);

  // Need a VMO to claim our pager backed pages are in.
  fbl::RefPtr<VmObjectPaged> vmo;
  zx_status_t status = make_uncommitted_pager_vmo(2, false, false, &vmo);
  ASSERT_OK(status);

  // Put the pages in and validate initial state.
  pq.SetReclaim(&page1, vmo->DebugGetCowPages().get(), 0);
  size_t queue;
  EXPECT_TRUE(pq.DebugPageIsReclaim(&page1, &queue));
  EXPECT_EQ(queue, 0u);
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {1, 0, 0, 0, 0, 0, 0, 0}}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){1, 0}));
  pq.SetReclaim(&page2, vmo->DebugGetCowPages().get(), 0);
  EXPECT_TRUE(pq.DebugPageIsReclaim(&page2, &queue));
  EXPECT_EQ(queue, 0u);
  EXPECT_TRUE(pq.QueueCounts() == ((PageQueues::Counts){.reclaim = {2, 0, 0, 0, 0, 0, 0, 0}}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){2, 0}));

  // Move the pages to the DontNeed queue.
  pq.MoveToReclaimDontNeed(&page1);
  pq.MoveToReclaimDontNeed(&page2);
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&page1));
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&page2));
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){.reclaim = {0, 0, 0, 0, 0, 0, 0, 0}, .reclaim_isolate = 2}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){0, 2}));

  // Rotate the queues. This should also process the DontNeed queue.
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&page1));
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&page2));
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){.reclaim = {0, 0, 0, 0, 0, 0, 0, 0}, .reclaim_isolate = 2}));
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){0, 2}));

  // Simulate access for one of the pages. Then rotate the queues again. This should move the
  // accessed page1 out of the DontNeed queue to MRU+1 (as we've rotated the queues after access).
  pq.MarkAccessed(&page1);
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.DebugPageIsReclaim(&page1, &queue));
  EXPECT_EQ(queue, 1u);
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&page2));
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){.reclaim = {0, 1, 0, 0, 0, 0, 0, 0}, .reclaim_isolate = 1}));
  // Two active queues by default, so page1 is still considered active.
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){1, 1}));

  // Rotate the queues again. The page accessed above should move to the next pager-backed queue.
  pq.RotateReclaimQueues();
  EXPECT_TRUE(pq.DebugPageIsReclaim(&page1, &queue));
  EXPECT_EQ(queue, 2u);
  EXPECT_TRUE(pq.DebugPageIsReclaimIsolate(&page2));
  EXPECT_TRUE(pq.QueueCounts() ==
              ((PageQueues::Counts){.reclaim = {0, 0, 1, 0, 0, 0, 0, 0}, .reclaim_isolate = 1}));
  // page1 has now moved on past the two active queues, so it now counts as inactive.
  EXPECT_TRUE(pq.GetActiveInactiveCounts() == ((PageQueues::ActiveInactiveCounts){0, 2}));

  pq.Remove(&page1);
  pq.Remove(&page2);

  END_TEST;
}

UNITTEST_START_TESTCASE(page_queues_tests)
VM_UNITTEST(pq_add_remove)
VM_UNITTEST(pq_move_queues)
VM_UNITTEST(pq_move_self_queue)
VM_UNITTEST(pq_rotate_queue)
VM_UNITTEST(pq_toggle_dont_need_queue)
UNITTEST_END_TESTCASE(page_queues_tests, "pq", "PageQueues tests")

}  // namespace vm_unittest
