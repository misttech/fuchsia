// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_EVICTOR_H_

#include <lib/zircon-internal/thread_annotations.h>
#include <sys/types.h>

#include <kernel/event.h>
#include <kernel/ffi.h>
#include <kernel/mutex.h>
#include <kernel/spinlock.h>
#include <ktl/atomic.h>

// Implements page evictor logic to free pages belonging to a PmmNode under memory pressure.
// Eviction in this context is both direct eviction of pager backed memory and discardable VMO
// memory, as well as by performing compression.
// This class is thread-safe.
class Evictor {
 public:
  Evictor();
  ~Evictor();

 private:
  enum class EvictionLevel : uint8_t {
    OnlyOldest = 0,
    IncludeNewest = 1,
  };

  struct EvictionTarget {
    bool pending = false;
    uint64_t free_pages_target = 0;
    uint64_t min_pages_to_free = 0;
    EvictionLevel level = EvictionLevel::OnlyOldest;
    bool print_counts = false;
    bool oom_trigger = false;
  };

  struct EvictedPageCounts {
    uint64_t pager_backed = 0;
    uint64_t pager_backed_loaned = 0;
    uint64_t discardable = 0;
    uint64_t compressed = 0;
  };

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
