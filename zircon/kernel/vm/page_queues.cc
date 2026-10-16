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

const char* PageQueues::string_from_age_reason(PageQueues::AgeReason reason) {
  return rust_page_queues_string_from_age_reason(static_cast<int32_t>(reason));
}

void PageQueues::RotateReclaimQueues() { rust_page_queues_rotate_reclaim_queues(&storage_); }

ktl::optional<PageQueues::VmoBacklink> PageQueues::PopAnonymousZeroFork() {
  PageQueuesVmoBacklink backlink{};
  if (!rust_page_queues_pop_anonymous_zero_fork(&storage_, &backlink)) {
    return ktl::nullopt;
  }
  return VmoBacklink{
      .cow = fbl::ImportFromRawPtr(backlink.cow),
      .page = backlink.page,
      .offset = backlink.offset,
  };
}

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

ktl::optional<PageQueues::VmoBacklink> PageQueues::GetCowForLoanedPage(vm_page_t* page) {
  PageQueuesVmoBacklink backlink{};
  if (!rust_page_queues_get_cow_for_loaned_page(&storage_, page, &backlink)) {
    return ktl::nullopt;
  }
  return VmoBacklink{
      .cow = fbl::ImportFromRawPtr(backlink.cow),
      .page = backlink.page,
      .offset = backlink.offset,
  };
}

PageQueues::ReclaimCounts PageQueues::GetReclaimQueueCounts() const {
  ReclaimCounts counts;
  rust_page_queues_get_reclaim_queue_counts(&storage_, &counts);
  return counts;
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

void PageQueues::Dump() const { rust_page_queues_dump(&storage_); }

uint64_t PageQueues::GetLruPagesCompressed() { return rust_page_queues_get_lru_pages_compressed(); }

void PageQueues::EnableAnonymousReclaim(bool zero_forks) {
  rust_page_queues_enable_anonymous_reclaim(&storage_, zero_forks);
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

bool PageQueues::DebugPageIsAnonymousZeroFork(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_anonymous_zero_fork(&storage_, page);
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

void PageQueues::StartDebugCompressor() { rust_page_queues_start_debug_compressor(&storage_); }

void PageQueues::SetActiveRatioMultiplier(uint32_t multiplier) {
  rust_page_queues_set_active_ratio_multiplier(&storage_, multiplier);
}

void PageQueues::SetLruAction(LruAction action) {
  rust_page_queues_set_lru_action(&storage_, static_cast<int32_t>(action));
}

void PageQueues::DisableAging() { rust_page_queues_disable_aging(&storage_); }

void PageQueues::EnableAging() { rust_page_queues_enable_aging(&storage_); }

void PageQueues::SetAgingEvent(Event* event) { rust_page_queues_set_aging_event(&storage_, event); }
