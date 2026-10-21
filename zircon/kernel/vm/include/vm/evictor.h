// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_H_

#include <lib/fit/result.h>
#include <lib/zircon-internal/thread_annotations.h>
#include <sys/types.h>
#include <zircon/time.h>

#include <kernel/event.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <ktl/atomic.h>
#include <ktl/optional.h>
#include <vm/page.h>

class PmmNode;
class PageQueues;
class VmCompression;

// Forward declarations. Defined in vm/vm_cow_pages.h.
struct VmCowReclaimSuccess;
enum class VmCowReclaimFailure : uint8_t;
using VmCowReclaimResult = fit::result<VmCowReclaimFailure, VmCowReclaimSuccess>;

// Implements page evictor logic to free pages belonging to a PmmNode under memory pressure.
// Eviction in this context is both direct eviction of pager backed memory and discardable VMO
// memory, as well as by performing compression.
// This class is thread-safe.
class Evictor {
 public:
  enum class EvictionLevel : uint8_t {
    OnlyOldest = 0,
    IncludeNewest = 1,
  };

  enum class Output : bool {
    Print = true,
    NoPrint = false,
  };

  enum class TriggerReason : bool {
    OOM = true,
    Other = false,
  };

  // Eviction target state is grouped together behind a lock to allow different threads to safely
  // trigger and perform the eviction.
  struct EvictionTarget {
    bool pending = false;
    // The desired value to get |pmm_node_|'s free page count to
    uint64_t free_pages_target = 0;
    // A minimum amount of pages we want to evict, regardless of how much free memory is available.
    uint64_t min_pages_to_free = 0;
    EvictionLevel level = EvictionLevel::OnlyOldest;
    bool print_counts = false;
    bool oom_trigger = false;
  };

  // We count non-loaned and loaned evicted pages separately since the eviction goal is set in terms
  // of non-loaned pages (for now) so in order to verify expected behavior in tests we keep separate
  // counts.
  struct EvictedPageCounts {
    // The pager_backed and pager_backed_loaned counts are exclusive; any given evicted page is
    // counted as pager_backed or pager_backed_loaned depending on whether it's non-loaned or loaned
    // respectively.
    //
    // evicted from pager-backed VMO non-loaned page count
    uint64_t pager_backed = 0;
    // evicted from pager-backed VMO loaned page count
    uint64_t pager_backed_loaned = 0;
    // evicted from/via discardable VMO page count
    uint64_t discardable = 0;
    // evicted from an anonymous VMO via compression
    uint64_t compressed = 0;

    EvictedPageCounts& operator+=(const EvictedPageCounts& counts) {
      pager_backed += counts.pager_backed;
      pager_backed_loaned += counts.pager_backed_loaned;
      discardable += counts.discardable;
      compressed += counts.compressed;
      return *this;
    }
    EvictedPageCounts operator-(const EvictedPageCounts& counts) const {
      return EvictedPageCounts{
          .pager_backed = pager_backed - counts.pager_backed,
          .pager_backed_loaned = pager_backed_loaned - counts.pager_backed_loaned,
          .discardable = discardable - counts.discardable,
          .compressed = compressed - counts.compressed};
    }
    uint64_t non_loaned_total() const { return pager_backed + discardable + compressed; }
  };

  // Result returned from eviction methods indicating what they performed and why they returned.
  // Provides by the count of how many different items were evicted, which could be more or less
  // than requested, as well as whether any free memory target was achieved.
  struct EvictionResult {
    EvictedPageCounts counts = {};
    bool free_target_reached = false;
  };

  Evictor();
  ~Evictor();

  // Called from the scanner to enable eviction if required. Creates an eviction thread to process
  // asynchronous eviction requests.
  // By default this only enables user pager based eviction and |use_compression| can be used to
  // also perform compression.
  void EnableEviction(bool use_compression);

  // Evict from a user specified external |target| which is only used for this eviction attempt and
  // does not interfere with the |eviction_target_|.
  EvictionResult EvictFromExternalTarget(EvictionTarget target);

  // Performs a synchronous request to evict until free memory equals |free_mem_start| (in bytes)
  // and at least |min_mem_to_free| (in bytes) has been reclaimed. The return value is an
  // EvictionResult detailing exactly what was evicted and whether the free_mem_target was achieved.
  // The |eviction_level| is a rough control that maps to how old a page needs to be for being
  // considered for eviction. This may acquire arbitrary vmo and aspace locks.
  EvictionResult EvictSynchronous(uint64_t min_mem_to_free, uint64_t free_mem_target,
                                  EvictionLevel eviction_level = EvictionLevel::OnlyOldest,
                                  Output output = Output::NoPrint,
                                  TriggerReason reason = TriggerReason::Other);

  using TestReclaimFunction = bool (*)(void* ctx, VmCompression* compression,
                                       EvictionLevel eviction_level, bool* out_is_ok,
                                       VmCowReclaimSuccess* out_success,
                                       VmCowReclaimFailure* out_failure);
  using TestFreePagesFunction = uint64_t (*)(void* ctx);

  // Constructor for test code to specify custom methods to fake the reclamation.
  Evictor(void* ctx, TestReclaimFunction reclaim_function,
          TestFreePagesFunction free_pages_function);

  // Helpers for testing.
  EvictionTarget DebugGetEvictionTarget() const;

  // Combine the specified |target| with the pre-existing |eviction_target_|.
  void CombineEvictionTarget(EvictionTarget target);

  // Perform eviction based on the current values of |eviction_target_|. The expectation is that the
  // user will have set the target before calling this function with either SetEvictionTarget() or
  // CombineEvictionTarget(). This may acquire arbitrary vmo and aspace locks.
  EvictedPageCounts EvictFromPreloadedTarget();

 private:
  // Layout description matching Rust `Evictor` state for compile-time size/align/offset assertions.
  struct StateLayout {
    EvictionTarget eviction_target;
    DECLARE_MUTEX(StateLayout) eviction_lock;
    EvictedPageCounts total_evicted;
    DECLARE_SPINLOCK_WITH_TYPE(StateLayout, MonitoredSpinLock) lock;
    Thread* eviction_thread;
    ktl::atomic<bool> eviction_thread_exiting;
    AutounsignalEvent eviction_signal;
    OpaqueStorage<48, 8> test_reclaim_function;
    OpaqueStorage<48, 8> test_free_pages_function;
    bool eviction_enabled;
    bool use_compression;
  };

  [[maybe_unused]] OpaqueStorage<sizeof(StateLayout), alignof(StateLayout)> storage_{};
};

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_H_
