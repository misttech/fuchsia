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

KCOUNTER(pq_aging_reason_timeout, "pq.aging.reason.timeout")
KCOUNTER(pq_aging_reason_active_ratio, "pq.aging.reason.active_ratio")
KCOUNTER(pq_aging_reason_manual, "pq.aging.reason.manual")
KCOUNTER(pq_lru_spurious_wakeup, "pq.lru.spurious_wakeup")
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
  StopThreads();
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
  // Clamp the max rotate to the minimum.
  max_mru_rotate_time = ktl::max(min_mru_rotate_time, max_mru_rotate_time);
  // Prevent a rotation rate that is too small.
  max_mru_rotate_time = ktl::max(max_mru_rotate_time, ZX_SEC(1));

  min_mru_rotate_time_ = min_mru_rotate_time;
  max_mru_rotate_time_ = max_mru_rotate_time;

  // Cannot perform all of thread creation under the lock as thread creation requires
  // allocations so we create in temporaries first and then stash.
  Thread* mru_thread = Thread::Create(
      "page-queue-mru-thread",
      [](void* arg) -> int {
        static_cast<PageQueues*>(arg)->MruThread();
        return 0;
      },
      this, LOW_PRIORITY);
  DEBUG_ASSERT(mru_thread);

  mru_thread->Resume();

  Thread* lru_thread = Thread::Create(
      "page-queue-lru-thread",
      [](void* arg) -> int {
        static_cast<PageQueues*>(arg)->LruThread();
        return 0;
      },
      this, LOW_PRIORITY);
  DEBUG_ASSERT(lru_thread);
  lru_thread->Resume();

  {
    Guard<CriticalMutex> guard{&lock_};
    ASSERT(!mru_thread_);
    ASSERT(!lru_thread_);
    mru_thread_ = mru_thread;
    lru_thread_ = lru_thread;
    // Kick start any LRU processing that might be pending to ensure it doesn't spuriously timeout.
    MaybeTriggerLruProcessingLocked();
  }
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

void PageQueues::StopThreads() {
  // Cannot wait for threads to complete with the lock held, so update state and then perform any
  // joins outside the lock.
  Thread* mru_thread = nullptr;
  Thread* lru_thread = nullptr;

  {
    Guard<CriticalMutex> guard{&lock_};
    shutdown_threads_ = true;
    mru_thread = mru_thread_;
    lru_thread = lru_thread_;
    mru_event_.Signal();
    lru_event_.Signal();
  }

  int retcode;
  if (mru_thread) {
    zx_status_t status = mru_thread->Join(&retcode, ZX_TIME_INFINITE);
    ASSERT(status == ZX_OK);
  }
  if (lru_thread) {
    zx_status_t status = lru_thread->Join(&retcode, ZX_TIME_INFINITE);
    ASSERT(status == ZX_OK);
  }
}

void PageQueues::SetLruAction(LruAction action) {
  rust_page_queues_set_lru_action(this, static_cast<int32_t>(action));
}

void PageQueues::SetActiveRatioMultiplier(uint32_t multiplier) {
  Guard<CriticalMutex> guard{&lock_};
  active_ratio_multiplier_ = multiplier;
  // The change in multiplier might have caused us to need to age.
  CheckActiveRatioAgingLocked();
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

void PageQueues::SynchronizeWithAging() {
  for (int iterations = 1;; iterations++) {
    // Typically this loop should exit on its second iteration at most, since it might take two
    // calls to `TryAgingLocked` before the MRU generation is incremented, at which point the LRU
    // gen can be incremented. The exception to this is if racing with another concurrent
    // modifications to the isolate list, such as by another reclamation thread. Still, it is
    // incredibly unlikely to race such that other threads can completely depopulate both the LRU
    // queues and the isolate list so if we see many instances of such an incredibly unlikely race
    // then there's probably a bug and so generate a warning message.
    if (iterations % 20 == 0) {
      printf("[pq]: Warning %s iterated %d times\n", __FUNCTION__, iterations);
    }
    Guard<CriticalMutex> guard{&lock_};
    // If the LRU queue can be processed, then aging already happened and there's no need to wait
    // for it.
    if (CanIncrementLruGenLocked()) {
      return;
    }
    // Check for races with LRU processing that might have already populated the isolate list.
    if (page_queue_counts_[PageQueueReclaimIsolate].load(ktl::memory_order_relaxed) > 0) {
      return;
    }
    auto reason = GetAgeReasonLocked();
    // If there's no pending age reason then we are not going to wait, so return.
    if (ktl::get_if<zx_instant_mono_t>(&reason)) {
      return;
    }
    // Attempt to perform any aging, and then go around the loop to check if we synchronized.
    TryAgingLocked(ktl::get<AgeReason>(reason), guard.take());
  }
}

ktl::variant<PageQueues::AgeReason, zx_instant_mono_t> PageQueues::GetAgeReasonLocked() const {
  const zx_instant_mono_t current = current_mono_time();
  if (active_ratio_triggered_) {
    // Need to have passed the min time though.
    const zx_instant_mono_t min_timeout =
        zx_time_add_duration(last_age_time_.load(ktl::memory_order_relaxed), min_mru_rotate_time_);
    if (current < min_timeout) {
      return min_timeout;
    }
    // At least min time has elapsed, can age via active ratio.
    return AgeReason::ActiveRatio;
  }

  // Exceeding the maximum time forces aging.
  const zx_instant_mono_t max_timeout =
      zx_time_add_duration(last_age_time_.load(ktl::memory_order_relaxed), max_mru_rotate_time_);
  if (max_timeout <= current) {
    return AgeReason::Timeout;
  }
  // With no other reason, we will age once we hit the maximum timeout.
  return max_timeout;
}

void PageQueues::MaybeTriggerLruProcessingLocked() {
  bool needs_lru_processing = NeedsLruProcessingLocked();
  if (needs_lru_processing) {
    lru_event_.Signal();
  }
}

bool PageQueues::NeedsLruProcessingLocked() const {
  // Currently only reason to trigger lru processing is if the MRU needs space. This requires the
  // lock since the typical use case wants an ordering of any changes to the generation counts with
  // respect to this query. This works since the generation counts are also modified with the lock
  // held.
  if (mru_gen_.load(ktl::memory_order_relaxed) - lru_gen_.load(ktl::memory_order_relaxed) ==
      kNumReclaim - 1) {
    return true;
  }
  return false;
}

void PageQueues::DisableAging() {
  {
    // Take the lock_ over disabling aging to ensure the MruThread is not presently aging and will
    // therefore observe the aging_disabled_ flag next time it runs.
    Guard<CriticalMutex> guard{&lock_};
    // Validate a double DisableAging is not happening.
    if (aging_disabled_) {
      panic("Mismatched disable/enable pair");
    }
    aging_disabled_ = true;
    // Drop the lock_ before pausing the debug_compressor_ below which might trigger VMO destruction
    // and acquire the heap lock.
  }

#if DEBUG_ASSERT_IMPLEMENTED
  // Pause might drop the last reference to a VMO and trigger VMO destruction, which would then call
  // back into the page queues, so we must not hold the list_lock_ over the operation. We can
  // utilize the fact that once the debug_compressor_ is set it is never destroyed, so can take a
  // raw pointer to it.
  VmDebugCompressor* dc = nullptr;
  {
    Guard<CriticalMutex> list_guard{&list_lock_};
    if (debug_compressor_) {
      dc = &*debug_compressor_;
    }
  }
  if (dc) {
    dc->Pause();
  }
#endif
}

void PageQueues::EnableAging() {
  {
    Guard<CriticalMutex> guard{&lock_};
    // Validate a double EnableAging is not happening.
    if (!aging_disabled_) {
      panic("Mismatched disable/enable pair");
    }
    aging_disabled_ = false;

    // Notify the threads, allowing them to proceed with any pending actions if they had been
    // waiting.
    mru_event_.Signal();
    lru_event_.Signal();
  }

#if DEBUG_ASSERT_IMPLEMENTED
  Guard<CriticalMutex> guard{&list_lock_};
  if (debug_compressor_) {
    debug_compressor_->Resume();
  }
#endif
}

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

// This runs the aging thread. Aging, unlike lru processing, scanning or eviction, requires very
// little work and is more about coordination. As such this thread is heavy on checks and signalling
// but generally only needs to hold any locks for the briefest of times.
// There is, currently, one exception to that, which is the calls to scanner_wait_for_accessed_scan.
// The scanner will, eventually, be a separate thread that is synchronized with, but presently
// a full scan may happen inline in that method call, and get attributed directly to this thread.
void PageQueues::MruThread() {
  // Pretend that aging happens during startup to simplify the rest of the loop logic.
  last_age_time_ = current_mono_time();

  while (!shutdown_threads_.load(ktl::memory_order_relaxed)) {
    // Attempt to perform aging steps in a helper lambda that acquires the lock. Once there is no
    // more aging to be done returns a deadline to wait for.
    zx_instant_mono_t wait_deadline = [&]() {
      while (!shutdown_threads_.load(ktl::memory_order_relaxed)) {
        Guard<CriticalMutex> guard{&lock_};
        // If aging is disabled or we are unable to age if we wanted to then exit and wait to be
        // signaled.
        if (aging_disabled_ || !CanIncrementMruGenLocked()) {
          return ZX_TIME_INFINITE;
        }
        auto reason_or_deadline = GetAgeReasonLocked();
        if (const zx_instant_mono_t* age_deadline =
                ktl::get_if<zx_instant_mono_t>(&reason_or_deadline)) {
          // Cannot age yet, so wait till the provided deadline or we are otherwise signaled.
          return *age_deadline;
        }
        // Attempt to proceed with aging.
        TryAgingLocked(ktl::get<AgeReason>(reason_or_deadline), guard.take());
      }
      return ZX_TIME_INFINITE_PAST;
    }();
    // The deadline returned is a suggestion on how long to wait before attempting to age again.
    // However, we additionally wait on the mru_event_ instead of just sleeping as the event will
    // get signaled should anything material change that may cause us to be able to age sooner than
    // the original deadline.
    mru_event_.WaitDeadline(wait_deadline, Interruptible::No);
  }
}

// This thread should, at some point, have some of its logic and signaling merged with the Evictor.
// Currently it might process the lru queue whilst the evictor is already trying to evict, which is
// not harmful but it's a bit wasteful as it doubles the work that happens.
// LRU processing, via ProcessIsolateAndLruQueues, is expensive and happens under the lock_. It is
// expected that ProcessIsolateAndLruQueues perform small units of work to avoid this thread
// causing excessive lock contention.
void PageQueues::LruThread() {
  constexpr uint kLruNeedsProcessingPollSeconds = 90;
  uint64_t pending_target_gen = UINT64_MAX;
  while (!shutdown_threads_.load(ktl::memory_order_relaxed)) {
    zx_status_t wait_status =
        lru_event_.Wait(Deadline::after_mono(ZX_SEC(kLruNeedsProcessingPollSeconds)));

    uint64_t target_gen;
    bool needs_processing = false;
    // Take the lock so we can calculate (race free) a target mru-gen.
    {
      Guard<CriticalMutex> guard{&lock_};
      needs_processing = NeedsLruProcessingLocked();
      // If needs processing is false this will calculate an incorrect target_gen, but that's fine
      // as we'll just discard it and it's simpler to just do it here unconditionally while the lock
      // is already held.
      target_gen = lru_gen_.load(ktl::memory_order_relaxed) + 1;
    }
    if (!needs_processing) {
      pq_lru_spurious_wakeup.Add(1);
      continue;
    }
    if (wait_status == ZX_ERR_TIMED_OUT) {
      // The queue needs processing, but we woke up due to a timeout on the event and not a signal.
      // This could happen due to a race where we woke up before the MruThread could actually set
      // the signal, in which case we want to record the target_gen that we saw and then go back and
      // wait for the signal.
      // In the case where we have timed out *and* the target_gen we want is the same as the last
      // target_gen we were looking for then this means that we have gone a full poll interval with:
      //  * Processing needing to happen.
      //  * No event being signaled.
      //  * No other thread processing the queue for us.
      if (pending_target_gen != target_gen) {
        pending_target_gen = target_gen;
        continue;
      }
      printf("ERROR LruThread signal was not seen after %u seconds and queue needs processing\n",
             kLruNeedsProcessingPollSeconds);
      Dump();
    }

    // Keep processing until we have caught up to what is required. This ensures we are
    // re-synchronized with the mru-thread and will not miss any signals on the lru_event_.
    while (needs_processing) {
      // With the lock dropped process the target. This is not racy as generations are monotonic, so
      // worst case someone else already processed this generation and this call will be a no-op.
      rust_page_queues_process_lru_queue(this, target_gen, SIZE_MAX);

      // Take the lock so we can calculate (race free) a target mru-gen.
      Guard<CriticalMutex> guard{&lock_};
      needs_processing = NeedsLruProcessingLocked();
      // If needs processing is false this will calculate an incorrect target_gen, but that's fine
      // as we'll just discard it and it's simpler to just do it here unconditionally while the lock
      // is already held.
      target_gen = lru_gen_.load(ktl::memory_order_relaxed) + 1;
    }
  }
}

void PageQueues::TryAgingLocked(AgeReason age_reason, Guard<CriticalMutex>::Adoptable&& adopt) {
  VM_KTRACE_DURATION(2, "LruThread Aging");
  Guard<CriticalMutex> guard{AdoptLock, &lock_, ktl::move(adopt)};
  if (scanner_needs_accessed_scan(last_age_time_)) {
    // Perform the accessed without our lock held to avoid unnecessary contention.
    guard.Release();
    scanner_wait_for_accessed_scan(last_age_time_);
  } else {
    IncrementMruGenLocked(age_reason);
  }
}

void PageQueues::IncrementMruGenLocked(AgeReason age_reason) {
  ASSERT(CanIncrementMruGenLocked());

  // Increment the mru generation and record the current age reason etc.
  mru_gen_.fetch_add(1, ktl::memory_order_relaxed);
  last_age_time_ = current_mono_time();
  last_age_reason_ = age_reason;

  // As aging has happened we should consume any old active ratio trigger and re-calculate it.
  active_ratio_triggered_ = false;
  CheckActiveRatioAgingLocked();

  // Signal any externally supplied aging event if one exists.
  if (aging_event_) {
    aging_event_->Signal();
  }

  // Required to notify the LRU thread of mru generation changes.
  MaybeTriggerLruProcessingLocked();

  // Keep a count of the different reasons we have rotated.
  switch (age_reason) {
    case AgeReason::Timeout:
      pq_aging_reason_timeout.Add(1);
      break;
    case AgeReason::ActiveRatio:
      pq_aging_reason_active_ratio.Add(1);
      break;
    case AgeReason::Manual:
      pq_aging_reason_manual.Add(1);
      break;
    default:
      panic("Unknown age reason");
  }
}

void PageQueues::RotateReclaimQueues() {
  Guard<CriticalMutex> guard{&lock_};
  // 'Force' aging to happen by processing the lru queue until we are able to increment the mru gen.
  // This directly manipulates the mru gen instead of using TryAgingLocked as it intentionally
  // sidesteps certain updates.
  while (!CanIncrementMruGenLocked()) {
    uint64_t target_gen = lru_gen_.load(ktl::memory_order_relaxed) + 1;
    guard.CallUnlocked([&]() { rust_page_queues_process_lru_queue(this, target_gen, SIZE_MAX); });
  }
  IncrementMruGenLocked(AgeReason::Manual);
}

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

void PageQueues::MaybeCheckActiveRatioAgingLocked(size_t pages) {
  if (unlikely(!RecordActiveRatioSkips(pages))) {
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
      SynchronizeWithAging();
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

void PageQueues::SetAgingEvent(Event* event) {
  Guard<CriticalMutex> guard{&lock_};
  ASSERT(!event || !aging_event_);
  aging_event_ = event;
}

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
