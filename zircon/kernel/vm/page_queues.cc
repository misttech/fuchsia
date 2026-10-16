// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/intrin.h>
#include <lib/counters.h>
#include <lib/fit/defer.h>
#include <lib/zircon-internal/macros.h>

#include <fbl/ref_counted_upgradeable.h>
#include <kernel/auto_preempt_disabler.h>
#include <object/thread_dispatcher.h>
#include <vm/compression.h>
#include <vm/page.h>
#include <vm/page_queues.h>
#include <vm/pmm.h>
#include <vm/scanner.h>
#include <vm/vm_cow_pages.h>

namespace {

KCOUNTER(pq_peek_sync_aging, "pq.peek.sync_aging")
KCOUNTER(pq_peek_process_lru, "pq.peek.process_lru")
KCOUNTER(pq_peek_sync_aging_all_inactive, "pq.peek.sync_aging.all_inactive")
KCOUNTER(pq_peek_process_lru_all_inactive, "pq.peek.process_lru.all_inactive")

}  // namespace

// static
uint64_t PageQueues::GetLruPagesCompressed() { return rust_page_queues_get_lru_pages_compressed(); }

PageQueues::PageQueues()
    : min_mru_rotate_time_(kDefaultMinMruRotateTime),
      max_mru_rotate_time_(kDefaultMaxMruRotateTime),
      active_ratio_multiplier_(kDefaultActiveRatioMultiplier) {}

PageQueues::~PageQueues() {
  rust_page_queues_stop_threads(this);
  for (uint32_t i = 0; i < PageQueueNumQueues; i++) {
    DEBUG_ASSERT(page_queues_[i].is_empty());
  }
  for (size_t i = 0; i < page_queue_counts_.size(); i++) {
    DEBUG_ASSERT_MSG(page_queue_counts_[i] == 0, "i=%zu count=%zu", i,
                     page_queue_counts_[i].load());
  }
}

void PageQueues::StartThreads(zx_duration_mono_t min_mru_rotate_time,
                              zx_duration_mono_t max_mru_rotate_time) {
  rust_page_queues_start_threads(this, min_mru_rotate_time, max_mru_rotate_time);
}

void PageQueues::StartDebugCompressor() {
  // The debug compressor should not be enabled without debug asserts as we guard all usages of the
  // debug compressor with compile time checks so that it cannot impact the performance of release
  // versions.
  ASSERT(DEBUG_ASSERT_IMPLEMENTED);
#if DEBUG_ASSERT_IMPLEMENTED
  fbl::AllocChecker ac;
  ktl::unique_ptr<VmDebugCompressor> dc(new (&ac) VmDebugCompressor);
  if (!ac.check()) {
    panic("Failed to allocate VmDebugCompressor");
  }
  zx_status_t status = dc->Init();
  ASSERT(status == ZX_OK);
  Guard<CriticalMutex> guard{&list_lock_};
  // We should only be initializing the debug compressor once.
  DEBUG_ASSERT(!debug_compressor_);
  debug_compressor_ = ktl::move(dc);
#endif
}

void PageQueues::SetLruAction(LruAction action) {
  rust_page_queues_set_lru_action(this, static_cast<int32_t>(action));
}

void PageQueues::SetActiveRatioMultiplier(uint32_t multiplier) {
  rust_page_queues_set_active_ratio_multiplier(this, multiplier);
}

void PageQueues::CheckActiveRatioAgingLocked() {
  if (active_ratio_triggered_) {
    // Already triggered, nothing more to do.
    return;
  }
  if (IsActiveRatioTriggeringAging()) {
    active_ratio_triggered_ = true;
    mru_event_.Signal();
  }
}

bool PageQueues::IsActiveRatioTriggeringAging() {
  ActiveInactiveCounts counts = GetActiveInactiveCounts();
  return counts.active * active_ratio_multiplier_ > counts.inactive;
}

void PageQueues::DisableAging() { rust_page_queues_disable_aging(this); }

void PageQueues::EnableAging() { rust_page_queues_enable_aging(this); }

const char* PageQueues::string_from_age_reason(PageQueues::AgeReason reason) {
  switch (reason) {
    case AgeReason::ActiveRatio:
      return "Active ratio";
    case AgeReason::Timeout:
      return "Timeout";
    case AgeReason::Manual:
      return "Manual";
    default:
      panic("Unreachable");
  }
}

void PageQueues::Dump() const {
  // Need to grab a copy of all the counts and generations. As the lock is needed to acquire the
  // active/inactive counts, also hold the lock over the copying of the counts to avoid needless
  // races.
  uint64_t mru_gen;
  uint64_t lru_gen;
  size_t counts[kNumReclaim] = {};
  size_t inactive_count;
  size_t failed_reclaim;
  size_t dirty;
  zx_instant_mono_t last_age_time;
  AgeReason last_age_reason;
  ActiveInactiveCounts activeinactive;
  {
    Guard<CriticalMutex> guard{&lock_};
    mru_gen = mru_gen_.load(ktl::memory_order_relaxed);
    lru_gen = lru_gen_.load(ktl::memory_order_relaxed);
    failed_reclaim = page_queue_counts_[PageQueueFailedReclaim].load(ktl::memory_order_relaxed);
    inactive_count = page_queue_counts_[PageQueueReclaimIsolate].load(ktl::memory_order_relaxed);
    dirty = page_queue_counts_[PageQueuePagerBackedDirty].load(ktl::memory_order_relaxed);
    for (uint32_t i = 0; i < kNumReclaim; i++) {
      counts[i] = page_queue_counts_[PageQueueReclaimBase + i].load(ktl::memory_order_relaxed);
    }
    activeinactive = GetActiveInactiveCounts();
    last_age_time = last_age_time_.load(ktl::memory_order_relaxed);
    last_age_reason = last_age_reason_;
  }
  // Small arbitrary number that should be more than large enough to hold the constructed string
  // without causing stack allocation pressure.
  constexpr size_t kBufSize = 50;
  // Start with the buffer null terminated. snprintf will always keep it null terminated.
  char buf[kBufSize] __UNINITIALIZED = "\0";
  size_t buf_len = 0;
  // This formats the counts of all buckets, not just those within the mru->lru range, even though
  // any buckets not in that range should always have a count of zero. The format this generates is
  // [active],[active],inactive,inactive,{last inactive},should-be-zero,should-be-zero
  // Although the inactive and should-be-zero use the same formatting, they are broken up by the
  // {last inactive}.
  for (uint64_t i = 0; i < kNumReclaim; i++) {
    PageQueue queue = gen_to_queue(mru_gen - i);
    ASSERT(buf_len < kBufSize);
    const size_t remain = kBufSize - buf_len;
    int write_len;
    if (i < kNumActiveQueues) {
      write_len = snprintf(buf + buf_len, remain, "[%zu],", counts[queue - PageQueueReclaimBase]);
    } else if (i == mru_gen - lru_gen) {
      write_len = snprintf(buf + buf_len, remain, "{%zu},", counts[queue - PageQueueReclaimBase]);
    } else {
      write_len = snprintf(buf + buf_len, remain, "%zu,", counts[queue - PageQueueReclaimBase]);
    }
    // Negative values are returned on encoding errors, which we never expect to get.
    ASSERT(write_len >= 0);
    if (static_cast<uint>(write_len) >= remain) {
      // Buffer too small, just use whatever we have constructed so far.
      break;
    }
    buf_len += write_len;
  }
  zx_instant_mono_t current = current_mono_time();
  timespec age_time = zx_timespec_from_duration(zx_time_sub_time(current, last_age_time));
  printf("pq: MRU:%" PRIu64 " (%ld.%lds ago, %s) LRU:%" PRIu64 " Act/Inact:%zu/%zu\n", mru_gen,
         age_time.tv_sec, age_time.tv_nsec, string_from_age_reason(last_age_reason), lru_gen,
         activeinactive.active, activeinactive.inactive);
  printf("pq: %s Isolate:%zu Dirty:%zu Fail:%zu\n", buf, inactive_count, dirty, failed_reclaim);
}

void PageQueues::RotateReclaimQueues() { rust_page_queues_rotate_reclaim_queues(this); }

ktl::optional<PageQueues::VmoBacklink> PageQueues::PeekIsolateList() {
  Guard<CriticalMutex> guard{&list_lock_};
  for (auto& list : isolate_queues_) {
    if (!list.is_empty()) {
      vm_page_t* head = &list.front();
      PageQueue page_queue =
          static_cast<PageQueue>(head->object.get_page_queue_ref().load(ktl::memory_order_relaxed));
      DEBUG_ASSERT(page_queue == PageQueueReclaimIsolate);
      VmCowPages* cow = reinterpret_cast<VmCowPages*>(head->object.get_object());
      DEBUG_ASSERT(cow);
      // Upgrading to a refptr can never fail as all pages are removed from a VmCowPages, and
      // hence from the page queues here, prior to the last reference to a cow pages being
      // dropped.
      fbl::RefPtr<VmCowPages> cow_pages = fbl::MakeRefPtrUpgradeFromRaw(cow, list_lock_);
      DEBUG_ASSERT(cow_pages);
      return VmoBacklink{
          .cow = ktl::move(cow_pages), .page = head, .offset = head->object.get_page_offset()};
    }
  }
  return ktl::nullopt;
}

void PageQueues::MaybeCheckActiveRatioAging(size_t pages) {
  if (unlikely(!RecordActiveRatioSkips(pages))) {
    Guard<CriticalMutex> guard{&lock_};
    CheckActiveRatioAgingLocked();
  }
}

void PageQueues::MoveToQueueLockedList(vm_page_t* page, PageQueue queue) {
  DEBUG_ASSERT(queue != PageQueueReclaimIsolate);
  DEBUG_ASSERT(page->state() == vm_page_state::OBJECT);
  DEBUG_ASSERT(!page->is_free());
  DEBUG_ASSERT(page->queue_node.InContainer());
  DEBUG_ASSERT(page->object.get_object());
  uint32_t old_queue = page->object.get_page_queue_ref().exchange(queue, ktl::memory_order_relaxed);
  DEBUG_ASSERT(old_queue != PageQueueNone);

  page->queue_node.template RemoveFromContainer<vm_page_node_traits>();
  page_queues_[queue].push_front(page);
  page_queue_counts_[old_queue].fetch_sub(1, ktl::memory_order_relaxed);
  page_queue_counts_[queue].fetch_add(1, ktl::memory_order_relaxed);
}

PageQueues::ReclaimCounts PageQueues::GetReclaimQueueCounts() const {
  ReclaimCounts counts;
  rust_page_queues_get_reclaim_queue_counts(this, &counts);
  return counts;
}

PageQueues::Counts PageQueues::QueueCounts() const {
  Counts counts;
  rust_page_queues_queue_counts(this, &counts);
  return counts;
}

bool PageQueues::DebugPageIsReclaim(const vm_page_t* page, size_t* queue) const {
  return rust_page_queues_debug_page_is_reclaim(this, page, queue);
}

bool PageQueues::DebugPageIsReclaimIsolate(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_reclaim_isolate(this, page);
}

bool PageQueues::DebugPageIsPagerBackedDirty(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_pager_backed_dirty(this, page);
}

bool PageQueues::DebugPageIsAnonymous(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_anonymous(this, page);
}

bool PageQueues::DebugPageIsAnonymousZeroFork(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_anonymous_zero_fork(this, page);
}

bool PageQueues::DebugPageIsAnyAnonymous(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_any_anonymous(this, page);
}

bool PageQueues::DebugPageIsWired(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_wired(this, page);
}

bool PageQueues::DebugPageIsHighPriority(const vm_page_t* page) const {
  return rust_page_queues_debug_page_is_high_priority(this, page);
}

ktl::optional<PageQueues::VmoBacklink> PageQueues::PopAnonymousZeroFork() {
  ktl::optional<PageQueues::VmoBacklink> ret;
  {
    Guard<CriticalMutex> guard{&list_lock_};

    auto& q = page_queues_[PageQueueAnonymousZeroFork];
    vm_page_t* page = q.is_empty() ? nullptr : &q.back();
    if (!page) {
      return ktl::nullopt;
    }

    VmCowPages* cow = reinterpret_cast<VmCowPages*>(page->object.get_object());
    uint64_t page_offset = page->object.get_page_offset();
    DEBUG_ASSERT(cow);
    MoveToQueueLockedList(page, PageQueueAnonymous);
    ret = VmoBacklink{fbl::MakeRefPtrUpgradeFromRaw(cow, guard), page, page_offset};
  }
  MaybeCheckActiveRatioAging(1);
  return ret;
}

ktl::optional<PageQueues::VmoBacklink> PageQueues::PeekIsolate(size_t lowest_queue) {
  // Ignore any requests to evict from the active queues as this is never allowed.
  lowest_queue = ktl::max(lowest_queue, kNumActiveQueues);
  // Whether we're allowed to peek from all but the active queues. Typically true only under OOM.
  const bool peek_all_inactive = lowest_queue == kNumActiveQueues;

  // TODO(adanis): Restructure this loop such that there is no question about its termination, but
  // for now be paranoid.
  constexpr uint kMaxIterations = kNumReclaim * 2;
  uint loop_iterations = 0;

  while (true) {
    // Peek the Isolate queue in case anything is ready for us.
    ktl::optional<VmoBacklink> result = PeekIsolateList();
    if (result) {
      return result;
    }
    if (loop_iterations++ > kMaxIterations) {
      KERNEL_OOPS("[pq]: %s iterated more than %u times (%u). lru:%zu mru:%zu\n", __FUNCTION__,
                  kMaxIterations, loop_iterations, lru_gen_.load(ktl::memory_order_relaxed),
                  mru_gen_.load(ktl::memory_order_relaxed));
    }

    // Synchronize with any aging that was outstanding at the time of this call, and then use that
    // updated mru_gen_ to compute the lru_target. Only do this for the first iteration of the loop,
    // because we want to meet the termination condition to break out of the loop, which might not
    // happen if we update mru_gen_ every time.
    if (loop_iterations == 1) {
      rust_page_queues_synchronize_with_aging(this);
      if (peek_all_inactive) {
        pq_peek_sync_aging_all_inactive.Add(1);
      } else {
        pq_peek_sync_aging.Add(1);
      }
    }
    // The limit gen is 1 larger than the lowest queue because evicting from queue X is done by
    // attempting to make the lru queue be X+1.
    const uint64_t lru_limit = mru_gen_.load(ktl::memory_order_relaxed) - (lowest_queue - 1);
    // Attempt to process one generation at a time to limit the work done before we find a
    // reclaimable page.
    const uint64_t lru_target = lru_gen_.load(ktl::memory_order_relaxed) + 1;
    if (lru_target > lru_limit) {
      return PeekIsolateList();
    }
    // Although we only need a single page in the Isolate queue to return for the peek result, under
    // the assumption that the caller is probably going to peek multiple pages move a few pages for
    // efficiency.
    // We do not want to process the entire LRU queue since it could contain tends to hundreds of
    // thousands of items and so the full processing is left to the LruThread.
    rust_page_queues_process_lru_queue(this, lru_target, 16);
    if (peek_all_inactive) {
      pq_peek_process_lru_all_inactive.Add(1);
    } else {
      pq_peek_process_lru.Add(1);
    }
  }
}

PageQueues::ActiveInactiveCounts PageQueues::GetActiveInactiveCounts() const {
  ActiveInactiveCounts counts;
  rust_page_queues_get_active_inactive_counts(this, &counts);
  return counts;
}

void PageQueues::SetAgingEvent(Event* event) { rust_page_queues_set_aging_event(this, event); }

void PageQueues::EnableAnonymousReclaim(bool zero_forks) {
  rust_page_queues_enable_anonymous_reclaim(this, zero_forks);
}

ktl::optional<PageQueues::VmoBacklink> PageQueues::GetCowForLoanedPage(vm_page_t* page) {
  DEBUG_ASSERT(page->is_loaned());
  vm_page_state state = page->state();
  switch (state) {
    case vm_page_state::FREE_LOANED:
      // Page is not owned by the page queues, so no cow pages to lookup.
      return ktl::nullopt;
    case vm_page_state::OBJECT: {
      // Delaying the lock acquisition and then reading the object field here is safe since the
      // caller has guaranteed that the page state is not changing, and then the  object field is
      // only modified under lock_, which we will be holding.
      Guard<CriticalMutex> guard{&list_lock_};
      VmCowPages* cow = reinterpret_cast<VmCowPages*>(page->object.get_object());
      if (!cow) {
        // Our examination of the state was racy and this page may or may not be owned by a VMO, but
        // it's not in the page queues. It is the responsibility of the caller to deal with scenario
        // and the fact that once we drop our lock the page could get inserted into the page queues.
        return ktl::nullopt;
      }
      // There is a using/borrowing cow and we know it is still alive as we hold the
      // PageQueues lock, and the cow may not destruct while it still has pages.
      uint64_t page_offset = page->object.get_page_offset();
      VmoBacklink backlink{fbl::MakeRefPtrUpgradeFromRaw(cow, guard), page, page_offset};
      DEBUG_ASSERT(backlink.cow);
      return backlink;
    }
    case vm_page_state::ALLOC:
      // Page is moving between the PMM and a VMO in some direction, but is not in the page
      // queues.
      return ktl::nullopt;
    default:
      // A loaned page in any other state is invalid and represents a programming error or bug.
      panic("Unexpected page state %s for loaned page", page_state_to_string(state));
      return ktl::nullopt;
  }
}
