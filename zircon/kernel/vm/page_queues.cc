// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <vm/page_queues.h>
#include <vm/vm_constants.h>
#include <vm/vm_cow_pages.h>

static_assert(sizeof(PageQueues) == kPageQueuesStorageSize);
static_assert(alignof(PageQueues) == kPageQueuesStorageAlign);

PageQueues::PageQueues() { rust_page_queues_init(&storage_); }

PageQueues::~PageQueues() { rust_page_queues_destroy(&storage_); }

void PageQueues::RotateReclaimQueues() { rust_page_queues_rotate_reclaim_queues(&storage_); }

ktl::optional<PageQueues::VmoBacklink> PageQueues::PeekIsolate(size_t lowest_queue) {
  PageQueuesVmoBacklink backlink{};
  if (!rust_page_queues_peek_isolate(&storage_, lowest_queue, &backlink)) {
    return ktl::nullopt;
  }
  return VmoBacklink{
      .cow = fbl::ImportFromRawPtr(backlink.cow),
      .page = backlink.page,
      .offset = backlink.offset,
  };
}

PageQueues::Counts PageQueues::QueueCounts() const {
  Counts counts;
  rust_page_queues_queue_counts(&storage_, &counts);
  return counts;
}

PageQueues::ActiveInactiveCounts PageQueues::GetActiveInactiveCounts() const {
  ActiveInactiveCounts counts;
  rust_page_queues_get_active_inactive_counts(&storage_, &counts);
  return counts;
}

bool PageQueues::DebugPageIsReclaim(const vm_page_t* page, size_t* queue) const {
  return rust_page_queues_debug_page_is_reclaim(&storage_, page, queue);
}

bool PageQueues::DebugPageIsReclaimIsolate(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_reclaim_isolate(&storage_, page);
}

bool PageQueues::DebugPageIsPagerBackedDirty(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_pager_backed_dirty(&storage_, page);
}

bool PageQueues::DebugPageIsAnonymous(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_anonymous(&storage_, page);
}

bool PageQueues::DebugPageIsAnyAnonymous(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_any_anonymous(&storage_, page);
}

bool PageQueues::DebugPageIsWired(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_wired(&storage_, page);
}

bool PageQueues::DebugPageIsHighPriority(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_high_priority(&storage_, page);
}

void PageQueues::StartThreads(zx_duration_mono_t min_mru_rotate_time,
                              zx_duration_mono_t max_mru_rotate_time) {
  rust_page_queues_start_threads(&storage_, min_mru_rotate_time, max_mru_rotate_time);
}

void PageQueues::SetActiveRatioMultiplier(uint32_t multiplier) {
  rust_page_queues_set_active_ratio_multiplier(&storage_, multiplier);
}
