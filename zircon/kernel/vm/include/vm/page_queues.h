// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_

#include <lib/page/size.h>
#include <sys/types.h>

#include <fbl/macros.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/array.h>
#include <ktl/optional.h>
#include <vm/page.h>
#include <vm/vm_constants.h>

class VmCowPages;
class PageQueues;

// Version of PageQueues::VmoBacklink that uses a raw pointer instead of a RefPtr for FFI.
struct PageQueuesVmoBacklink {
  VmCowPages* cow;
  vm_page_t* page;
  uint64_t offset;
};

extern "C" {
void rust_page_queues_init(void* storage);
void rust_page_queues_destroy(void* storage);

void rust_page_queues_set_wired(const void* storage, vm_page_t* page, VmCowPages* object,
                                uint64_t page_offset);
void rust_page_queues_set_anonymous(const void* storage, vm_page_t* page, VmCowPages* object,
                                    uint64_t page_offset, bool skip_reclaim);
void rust_page_queues_set_reclaim(const void* storage, vm_page_t* page, VmCowPages* object,
                                  uint64_t page_offset);
void rust_page_queues_set_pager_backed_dirty(const void* storage, vm_page_t* page,
                                             VmCowPages* object, uint64_t page_offset);
void rust_page_queues_set_high_priority(const void* storage, vm_page_t* page, VmCowPages* object,
                                        uint64_t page_offset);

void rust_page_queues_move_to_wired(const void* storage, vm_page_t* page);
void rust_page_queues_move_to_anonymous(const void* storage, vm_page_t* page, bool skip_reclaim);
void rust_page_queues_move_to_reclaim(const void* storage, vm_page_t* page);
void rust_page_queues_move_to_reclaim_dont_need(const void* storage, vm_page_t* page);
void rust_page_queues_move_to_pager_backed_dirty(const void* storage, vm_page_t* page);
void rust_page_queues_move_to_high_priority(const void* storage, vm_page_t* page);
void rust_page_queues_move_anonymous_to_anonymous_zero_fork(const void* storage, vm_page_t* page);

void rust_page_queues_compress_failed(const void* storage, vm_page_t* page);

void rust_page_queues_change_object_offset_array(const void* storage, vm_page_t** pages,
                                                 VmCowPages* object, const uint64_t* offsets,
                                                 size_t count);

void rust_page_queues_remove(const void* storage, vm_page_t* page);
void rust_page_queues_remove_array_into_list(const void* storage, vm_page_t** pages, size_t count,
                                             VmPageDoublyLinkedList* out_list);

void rust_page_queues_mark_accessed(const void* storage, vm_page_t* page);

void rust_page_queues_rotate_reclaim_queues(const void* storage);

bool rust_page_queues_peek_isolate(const void* storage, size_t lowest_queue,
                                   PageQueuesVmoBacklink* out_backlink);

void rust_page_queues_queue_counts(const void* storage, void* out_counts);
void rust_page_queues_get_active_inactive_counts(const void* storage, void* out_counts);
bool rust_page_queues_reclaim_is_only_pager_backed(const void* storage);
bool rust_page_queues_is_page_reclaimable(const vm_page_t* page);

bool rust_page_queues_debug_page_is_reclaim(const void* storage, const vm_page_t* page,
                                            size_t* out_queue);
bool rust_page_queues_debug_page_is_reclaim_isolate(const void* storage, const vm_page_t* page);
bool rust_page_queues_debug_page_is_pager_backed_dirty(const void* storage, const vm_page_t* page);
bool rust_page_queues_debug_page_is_anonymous(const void* storage, const vm_page_t* page);
bool rust_page_queues_debug_page_is_any_anonymous(const void* storage, const vm_page_t* page);
bool rust_page_queues_debug_page_is_wired(const void* storage, const vm_page_t* page);
bool rust_page_queues_debug_page_is_high_priority(const void* storage, const vm_page_t* page);

void rust_page_queues_start_threads(const void* storage, zx_duration_mono_t min_mru_rotate_time,
                                    zx_duration_mono_t max_mru_rotate_time);
void rust_page_queues_set_active_ratio_multiplier(const void* storage, uint32_t multiplier);
}  // extern "C"

// Allocated pages that are part of the cow pages in a VmObjectPaged can be placed in a page queue.
// The page queues provide a way to
//  * Classify and group pages across VMO boundaries
//  * Retrieve the VMO that a page is contained in (via a back reference stored in the vm_page_t)
// Once a page has been placed in a page queue its queue_node becomes owned by the page queue and
// must not be used until the page has been Remove'd. It is not sufficient to call list_delete on
// the queue_node yourself as this operation is not atomic and needs to be performed whilst holding
// the PageQueues::lock_.
class PageQueues {
 public:
  // The number of reclamation queues is slightly arbitrary, but to be useful you want at least 3
  // representing
  //  * Very new pages that you probably don't want to evict as doing so probably implies you are in
  //    swap death
  //  * Slightly old pages that could be evicted if needed
  //  * Very old pages that you'd be happy to evict
  // With two active queues 8 page queues are used so that there is some fidelity of information in
  // the inactive queues. Additional queues have reduced value as sufficiently old pages quickly
  // become equivalently unlikely to be used in the future.
  static constexpr size_t kNumReclaim = 8;

  // Two active queues are used to allow for better fidelity of active information. This prevents
  // a race between aging once and needing to collect/harvest age information.
  static constexpr size_t kNumActiveQueues = 2;

  // The amount of pages that will have to move around the queues before the active/inactive
  // ratio is re-checked. This therefore represents how much error the active ratio aging process
  // might have, or how delayed the MRU generation might be.
  // In the worst case once the active ratio is triggered this value is how much page data needs to
  // then change queues before the aging process happens.
  static constexpr size_t kActiveInactiveErrorMargin = (2 * MB) / kPageSize;

  static_assert(kNumReclaim > kNumActiveQueues, "Needs to be at least one non-active queue");

  // In addition to active and inactive, we want to consider some of the queues as 'oldest' to
  // provide an additional way to limit eviction. Presently the processing of the LRU queue to make
  // room for aging is not integrated with the Evictor, and so will not trigger eviction, therefore
  // to have a non-zero number of pages ever appear in an oldest queue for eviction the last two
  // queues are considered the oldest.
  static constexpr size_t kNumOldestQueues = 2;
  static_assert(kNumOldestQueues + kNumActiveQueues <= kNumReclaim);

  // Number of different isolate queues that are available. Different isolate queues allow for
  // separating isolate pages into different buckets such that more nuanced choices on what page to
  // reclaim can be made.
  // We use 2 queues to separate "Don't Need" pages (high reclamation priority, index 0) from
  // standard aged pages (standard reclamation priority, index 1).
  static constexpr size_t kIsolateQueueDontNeed = 0;
  static constexpr size_t kIsolateQueueStandard = 1;
  static constexpr size_t kNumIsolateQueues = 2;

  static_assert(kIsolateQueueDontNeed < kNumIsolateQueues);
  static_assert(kIsolateQueueStandard < kNumIsolateQueues);
  static_assert(kIsolateQueueDontNeed < kIsolateQueueStandard);

  static constexpr zx_duration_mono_t kDefaultMinMruRotateTime = ZX_SEC(5);
  static constexpr zx_duration_mono_t kDefaultMaxMruRotateTime = ZX_SEC(5);

  // This is presently an arbitrary constant, since the min and max mru rotate time are currently
  // fixed at the same value, meaning that the active ratio can not presently trigger, or prevent,
  // aging.
  static constexpr uint64_t kDefaultActiveRatioMultiplier = 0;

  PageQueues();
  ~PageQueues();

  DISALLOW_COPY_ASSIGN_AND_MOVE(PageQueues);

  // All Set operations places a page, which must not currently be in a page queue, into the
  // specified queue. The backlink information of |object| and |page_offset| must be specified and
  // valid. If the page is either removed from the referenced object, or moved to a different
  // offset, the backlink information must be updated either by calling ChangeObjectOffsetLocked, or
  // removing the page completely from the queues.

  // TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
  FFI_ALWAYS_INLINE void SetWired(vm_page_t* page, VmCowPages* object, uint64_t page_offset) {
    rust_page_queues_set_wired(&storage_, page, object, page_offset);
  }
  // |skip_reclaim| controls whether reclaiming the page should be forcibly skipped regardless of
  // whether anonymous pages are considered reclaimable in general.
  FFI_ALWAYS_INLINE void SetAnonymous(vm_page_t* page, VmCowPages* object, uint64_t page_offset,
                                      bool skip_reclaim = false) {
    rust_page_queues_set_anonymous(&storage_, page, object, page_offset, skip_reclaim);
  }
  FFI_ALWAYS_INLINE void SetReclaim(vm_page_t* page, VmCowPages* object, uint64_t page_offset) {
    rust_page_queues_set_reclaim(&storage_, page, object, page_offset);
  }
  FFI_ALWAYS_INLINE void SetPagerBackedDirty(vm_page_t* page, VmCowPages* object,
                                             uint64_t page_offset) {
    rust_page_queues_set_pager_backed_dirty(&storage_, page, object, page_offset);
  }
  FFI_ALWAYS_INLINE void SetHighPriority(vm_page_t* page, VmCowPages* object,
                                         uint64_t page_offset) {
    rust_page_queues_set_high_priority(&storage_, page, object, page_offset);
  }

  // All Move operations change the queue that a page is considered to be in, but do not change the
  // object or offset backlink information. The page must currently be in a valid page queue.

  FFI_ALWAYS_INLINE void MoveToWired(vm_page_t* page) {
    rust_page_queues_move_to_wired(&storage_, page);
  }
  // |skip_reclaim| controls whether reclaiming the page should be forcibly skipped regardless of
  // whether anonymous pages are considered reclaimable in general.
  FFI_ALWAYS_INLINE void MoveToAnonymous(vm_page_t* page, bool skip_reclaim = false) {
    rust_page_queues_move_to_anonymous(&storage_, page, skip_reclaim);
  }
  FFI_ALWAYS_INLINE void MoveToReclaim(vm_page_t* page) {
    rust_page_queues_move_to_reclaim(&storage_, page);
  }
  FFI_ALWAYS_INLINE void MoveToReclaimDontNeed(vm_page_t* page) {
    rust_page_queues_move_to_reclaim_dont_need(&storage_, page);
  }
  FFI_ALWAYS_INLINE void MoveToPagerBackedDirty(vm_page_t* page) {
    rust_page_queues_move_to_pager_backed_dirty(&storage_, page);
  }
  FFI_ALWAYS_INLINE void MoveToHighPriority(vm_page_t* page) {
    rust_page_queues_move_to_high_priority(&storage_, page);
  }

  // If a page is presently in the anonymous (or reclaim queue, depending if anonymous pages are
  // reclaimable) moves it to the appropriate anonymous zero fork queue (exact queue depends on
  // whether zero forks are reclaimable or not). If the page is not in the anonymous queue then it
  // is not modified.
  FFI_ALWAYS_INLINE void MoveAnonymousToAnonymousZeroFork(vm_page_t* page) {
    rust_page_queues_move_anonymous_to_anonymous_zero_fork(&storage_, page);
  }

  // Indicates that page has failed a compression attempted, and moves it to a separate queue to
  // prevent it from being considered part of the reclaim set, which makes it neither active nor
  // inactive. The specified page must be in the page queues, but if not presently in a reclaim
  // queue this method will do nothing.
  // TODO(https://fxbug.dev/42138396): Determine whether/how pages are moved back into the reclaim
  // pool and either further generalize this to support pager backed, or specialize FailedReclaim to
  // be explicitly only anonymous.
  FFI_ALWAYS_INLINE void CompressFailed(vm_page_t* page) {
    rust_page_queues_compress_failed(&storage_, page);
  }

  // Changes the backlink information for a page and should only be called by the page owner under
  // its lock (that is the VMO lock). The page must currently be in a valid page queue.
  FFI_ALWAYS_INLINE void ChangeObjectOffsetArray(vm_page_t** pages, VmCowPages* object,
                                                 uint64_t* offsets, size_t count) {
    rust_page_queues_change_object_offset_array(&storage_, pages, object, offsets, count);
  }

  // Removes the page from any page list and returns ownership of the queue_node.
  FFI_ALWAYS_INLINE void Remove(vm_page_t* page) { rust_page_queues_remove(&storage_, page); }
  // Batched version of Remove that also places all the pages in the specified list
  FFI_ALWAYS_INLINE void RemoveArrayIntoList(vm_page_t** page, size_t count,
                                             VmPageDoublyLinkedList* out_list) {
    rust_page_queues_remove_array_into_list(&storage_, page, count, out_list);
  }

  // Tells the page queue this page has been accessed, and it should have its position in the queues
  // updated.
  FFI_ALWAYS_INLINE void MarkAccessed(vm_page_t* page) {
    rust_page_queues_mark_accessed(&storage_, page);
  }

  // Performs a manually requested aging event. This ignores usual aging triggers / restrictions and
  // waits, if necessary, for aging to be possible and then performs it.
  // Only for tests and debugging.
  void RotateReclaimQueues();

  // Used to represent and return page backlink information acquired whilst holding the page queue
  // lock. As a VMO may not destruct while it has pages in it, the cow RefPtr will always be valid,
  // although the page and offset contained here are not synchronized and must be separately
  // validated before use. This can be done by acquiring the returned vmo's lock and then validating
  // that the page is still contained at the offset.
  struct VmoBacklink {
    fbl::RefPtr<VmCowPages> cow;
    vm_page_t* page = nullptr;
    uint64_t offset = 0;
  };

  // Looks at the isolate queues and returns backlink information of the first page found. If the
  // isolate queue is empty then LRU queues up to |lowest_queue| epochs from the most recent will be
  // processed to attempt to fill the isolate list. If no page was found a nullopt is returned,
  // otherwise if it has_value the vmo field may be null to indicate that the vmo is running its
  // destructor (see VmoBacklink for more details). If a page is returned its location in the
  // reclaim queue is not modified.
  ktl::optional<VmoBacklink> PeekIsolate(size_t lowest_queue);

  // Helper struct to group queue length counts returned by QueueCounts.
  struct Counts {
    ktl::array<size_t, kNumReclaim> reclaim = {0};
    size_t reclaim_isolate = 0;
    size_t pager_backed_dirty = 0;
    size_t anonymous = 0;
    size_t wired = 0;
    size_t anonymous_zero_fork = 0;
    size_t failed_reclaim = 0;
    size_t high_priority = 0;

    bool operator==(const Counts& other) const {
      return reclaim == other.reclaim && reclaim_isolate == other.reclaim_isolate &&
             pager_backed_dirty == other.pager_backed_dirty && anonymous == other.anonymous &&
             wired == other.wired && anonymous_zero_fork == other.anonymous_zero_fork &&
             failed_reclaim == other.failed_reclaim && high_priority == other.high_priority;
    }
    bool operator!=(const Counts& other) const { return !(*this == other); }
  };

  Counts QueueCounts() const;

  struct ActiveInactiveCounts {
    // Pages that would normally be available for eviction, but are presently considered active and
    // so will not be evicted.
    size_t active = 0;
    // Pages that are available for eviction due to not presently being considered active.
    size_t inactive = 0;

    bool operator==(const ActiveInactiveCounts& other) const {
      return active == other.active && inactive == other.inactive;
    }
    bool operator!=(const ActiveInactiveCounts& other) const { return !(*this == other); }
  };
  ActiveInactiveCounts GetActiveInactiveCounts() const;

  // Returns whether or not the reclaim queues only include pager backed pages or not.
  FFI_ALWAYS_INLINE bool ReclaimIsOnlyPagerBacked() const {
    return rust_page_queues_reclaim_is_only_pager_backed(&storage_);
  }

  // Returns true if the page is in an isolate queue.
  FFI_ALWAYS_INLINE static bool IsPageReclaimable(const vm_page_t* page) {
    return rust_page_queues_is_page_reclaimable(page);
  }

  // These query functions are marked Debug as it is generally a racy way to determine a pages state
  // and these are exposed for the purpose of writing tests or asserts against the pagequeue.

  // This takes an optional output parameter that, if the function returns true, will contain the
  // index of the queue that the page was in.
  bool DebugPageIsReclaim(const vm_page_t* page, size_t* queue = nullptr) const;
  bool DebugPageIsReclaimIsolate(const vm_page_t* page) const;
  bool DebugPageIsPagerBackedDirty(const vm_page_t* page) const;
  bool DebugPageIsAnonymous(const vm_page_t* page) const;
  bool DebugPageIsAnyAnonymous(const vm_page_t* page) const;
  bool DebugPageIsWired(const vm_page_t* page) const;
  bool DebugPageIsHighPriority(const vm_page_t* page) const;

  // These methods are public so that the scanner can call. Once the scanner is an object that can
  // be friended, and not a collection of anonymous functions, these can be made private.

  // Creates any threads for queue management. This needs to be done separately to construction as
  // there is a recursive dependency where creating threads will need to manipulate pages, which
  // will call back into the page queues.
  // Delaying thread creation is fine as these threads are purely for aging and eviction management,
  // which is not needed during early kernel boot.
  // Failure to start the threads may cause operations such as RotatePagerBackedQueues to block
  // indefinitely as they might attempt to offload work to a nonexistent thread. This issue is only
  // relevant for unittests that may wish to avoid starting the threads for some tests.
  // It is the responsibility of the caller to only call this once, otherwise it will panic.
  void StartThreads(zx_duration_mono_t min_mru_rotate_time, zx_duration_mono_t max_mru_rotate_time);

  // Sets the active ratio multiplier.
  void SetActiveRatioMultiplier(uint32_t multiplier);

 private:
  OpaqueStorage<kPageQueuesStorageSize, kPageQueuesStorageAlign> storage_;
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_H_
