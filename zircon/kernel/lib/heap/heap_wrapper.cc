// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2008-2015 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <assert.h>
#include <debug.h>
#include <lib/cmpctmalloc.h>
#include <lib/heap.h>
#include <lib/heap_ffi.h>
#include <lib/heap_internal.h>
#include <lib/heap_profile.h>
#include <lib/instrumentation/asan.h>
#include <lib/lazy_init/lazy_init.h>
#include <lib/page/size.h>
#include <lib/virtual_alloc.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/ops.h>
#include <fbl/intrusive_double_list.h>
#include <kernel/ffi.h>
#include <kernel/spinlock.h>
#include <kernel/thread.h>
#include <ktl/atomic.h>
#include <phys/handoff/heap.h>
#include <vm/physmap.h>
#include <vm/pmm.h>
#include <vm/vm.h>

#define LOCAL_TRACE 0

#ifndef HEAP_PANIC_ON_ALLOC_FAIL
#if LK_DEBUGLEVEL > 2
#define HEAP_PANIC_ON_ALLOC_FAIL 1
#else
#define HEAP_PANIC_ON_ALLOC_FAIL 0
#endif
#endif

static_assert(
    !(KERNEL_MEMORY_PROFILER && HEAP_COLLECT_STATS),
    "Kernel memory profiler and heap collect statistics cannot be enabled at the same time.");

// Heap allocation tracing flag. Accessed atomically from both C++ (`ktl::atomic<bool>`) and the
// Rust `k heap trace` console command (`AtomicBool`).
extern "C" constinit ktl::atomic<bool> heap_trace = false;
static_assert(ktl::atomic<bool>::is_always_lock_free);
static_assert(sizeof(ktl::atomic<bool>) == 1);
static_assert(alignof(ktl::atomic<bool>) == 1);

// keep a list of unique caller:size sites in a list
namespace {
#if KERNEL_MEMORY_PROFILER
heap_profile::HeapProfileMap</*ValuesSize=*/1024ul * kPageSize, /*Capacity=*/4096ul * 4,
                             /*BucketCount=*/4096>
    alloc_map;
#endif

struct alloc_stat : fbl::DoublyLinkedListable<alloc_stat*> {
  // caller and size are used to uniquely identify this stat
  void* caller;
  size_t size;

  // Count of allocations and frees of this unique caller+size. The difference is the number of
  // allocations that are active.
  uint64_t alloc_count;
  uint64_t free_count;
  // Index of this entry in the |alloc_stat| array, we use this for when we find this from the list.
  uint32_t index;
};

constexpr uint32_t kNumStats = HEAP_COLLECT_STATS ? 1024 : 1;
uint32_t next_unused_stat = 0;
alloc_stat stats[kNumStats];

fbl::DoublyLinkedList<alloc_stat*> stat_list;
DECLARE_SINGLETON_SPINLOCK(stat_lock);
lazy_init::LazyInit<VirtualAlloc> virtual_alloc;

// Increments the alloc_count for the given caller+size reference and associates it with the given
// heap_ptr, which should be the result from a cmpct_alloc call.
void add_alloc_stat(void* caller, size_t size, void* heap_ptr) {
  if constexpr (!HEAP_COLLECT_STATS && !KERNEL_MEMORY_PROFILER) {
    return;
  }
  if (!heap_ptr) {
    return;
  }

#if KERNEL_MEMORY_PROFILER
  {
    // Get rounded up size.
    const uint32_t act_size = cmpct_get_size(heap_ptr);
    cmpct_set_cookie(heap_ptr, profile_track_alloc(act_size));
    return;  // Not compatible with HEAP_COLLECT_STATS because both use the cookie.
  }
#endif

  Guard<SpinLock, IrqSave> guard(stat_lock::Get());

  // look for an existing stat, bump the count and move to head if found
  for (alloc_stat& s : stat_list) {
    if (s.caller == caller && s.size == size) {
      s.alloc_count++;
      cmpct_set_cookie(heap_ptr, s.index);
      stat_list.erase(s);
      stat_list.push_front(&s);
      return;
    }
  }

  // allocate a new one and add it to the list
  if (unlikely(next_unused_stat >= kNumStats)) {
    // Set an out of range cookie so add_free_stat will skip later.
    cmpct_set_cookie(heap_ptr, kNumStats);
    return;
  }
  cmpct_set_cookie(heap_ptr, next_unused_stat);

  alloc_stat* s = &stats[next_unused_stat];
  s->caller = caller;
  s->size = size;
  s->alloc_count = 1;
  s->free_count = 0;
  s->index = next_unused_stat;
  stat_list.push_front(s);
  next_unused_stat++;
}

// Increments the free_count stat associated with the given heap pointer. This is the pointer
// returned by cmpct_alloc and should have previously been given to add_alloc_stat. add_free_stat
// must be called *before* passing the ptr to cmpct_free
void add_free_stat(void* heap_ptr) {
  if constexpr (!HEAP_COLLECT_STATS && !KERNEL_MEMORY_PROFILER) {
    return;
  }
  if (!heap_ptr) {
    return;
  }

#if KERNEL_MEMORY_PROFILER
  {
    const uint32_t handle = cmpct_get_cookie(heap_ptr);
    const uint32_t size = cmpct_get_size(heap_ptr);
    profile_track_free(handle, size);
    return;  // Not compatible with HEAP_COLLECT_STATS.
  }
#endif

  uint32_t index = cmpct_get_cookie(heap_ptr);
  if (index >= kNumStats) {
    return;
  }
  Guard<SpinLock, IrqSave> guard(stat_lock::Get());
  stats[index].free_count++;
}

void dump_stats() {
  if (!HEAP_COLLECT_STATS) {
    return;
  }

  Guard<SpinLock, IrqSave> guard(stat_lock::Get());

  // remove all of them from the list
  stat_list.clear();

  // reinsert all of the entries, sorted by size
  for (size_t i = 0; i < next_unused_stat; i++) {
    bool added = false;
    for (alloc_stat& s : stat_list) {
      if (stats[i].size >= s.size) {
        stat_list.insert(s, &stats[i]);
        added = true;
        break;
      }
    }
    // fell off the end
    if (!added) {
      stat_list.push_back(&stats[i]);
    }
  }

  // dump the list of stats
  for (alloc_stat& s : stat_list) {
    printf("size %8zu alloc_count %8" PRIu64 " free_count %8" PRIu64 " caller %p\n", s.size,
           s.alloc_count, s.free_count, s.caller);
  }

  if (next_unused_stat >= kNumStats) {
    printf("WARNING: max number of unique records hit, some statistics were likely lost\n");
  }
}

inline bool is_size_in_range(size_t size) { return size > 0 && size <= kHeapMaxAllocSize; }

}  // namespace

void heap_init() {
  if (vm_using_virtual_heap()) {
    virtual_alloc.Initialize(vm_page_state::HEAP);
    zx_status_t status = virtual_alloc->Init(vm_get_kernel_heap_base(), vm_get_kernel_heap_size(),
                                             1, kArchHeapAlignmentBits);
    if (status != ZX_OK) {
      panic("Failed to initialized heap backing allocator: %d", status);
    }

    printf("Kernel heap [%" PRIxPTR ", %" PRIxPTR
           ") using %zu pages (%zu KiB) for tracking bitmap\n",
           vm_get_kernel_heap_base(), vm_get_kernel_heap_base() + vm_get_kernel_heap_size(),
           virtual_alloc->DebugBitmapPages(), virtual_alloc->DebugBitmapPages() * kPageSize / 1024);
  }

  cmpct_init();
}

__NO_INLINE void* malloc(size_t size) {
  DEBUG_ASSERT(!arch_blocking_disallowed());
  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());

  LTRACEF("size %zu\n", size);

  void* ptr = cmpct_alloc(size);
  add_alloc_stat(__GET_CALLER(), size, ptr);

  if (unlikely(heap_trace.load(ktl::memory_order_relaxed))) {
    printf("caller %p malloc %zu -> %p\n", __GET_CALLER(), size, ptr);
  }

  if (HEAP_PANIC_ON_ALLOC_FAIL && unlikely((!ptr) && is_size_in_range(size))) {
    panic("malloc of size %zu failed\n", size);
  }

  return ptr;
}

__NO_INLINE void* memalign(size_t alignment, size_t size) {
  DEBUG_ASSERT(!arch_blocking_disallowed());
  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());

  LTRACEF("alignment %zu, size %zu\n", alignment, size);

  void* ptr = cmpct_memalign(alignment, size);
  add_alloc_stat(__GET_CALLER(), size, ptr);
  if (unlikely(heap_trace.load(ktl::memory_order_relaxed))) {
    printf("caller %p memalign %zu, %zu -> %p\n", __GET_CALLER(), alignment, size, ptr);
  }

  if (HEAP_PANIC_ON_ALLOC_FAIL && unlikely((!ptr) && is_size_in_range(size))) {
    panic("memalign of size %zu align %zu failed\n", size, alignment);
  }

  return ptr;
}

__NO_INLINE void* calloc(size_t count, size_t size) {
  DEBUG_ASSERT(!arch_blocking_disallowed());
  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());

  LTRACEF("count %zu, size %zu\n", count, size);

  size_t realsize = count * size;

  void* ptr = cmpct_alloc(realsize);
  add_alloc_stat(__GET_CALLER(), size, ptr);
  if (likely(ptr)) {
    memset(ptr, 0, realsize);
  }
  if (unlikely(heap_trace.load(ktl::memory_order_relaxed))) {
    printf("caller %p calloc %zu, %zu -> %p\n", __GET_CALLER(), count, size, ptr);
  }
  return ptr;
}

__NO_INLINE void free(void* ptr) {
  DEBUG_ASSERT(!arch_blocking_disallowed());

  LTRACEF("ptr %p\n", ptr);
  if (unlikely(heap_trace.load(ktl::memory_order_relaxed))) {
    printf("caller %p free %p\n", __GET_CALLER(), ptr);
  }
  add_free_stat(ptr);

  cmpct_free(ptr);
}

__NO_INLINE void sized_free(void* ptr, size_t s) {
  DEBUG_ASSERT(!arch_blocking_disallowed());

  LTRACEF("ptr %p size %lu\n", ptr, s);
  if (unlikely(heap_trace.load(ktl::memory_order_relaxed))) {
    printf("caller %p free %p size %lu\n", __GET_CALLER(), ptr, s);
  }
  add_free_stat(ptr);
  cmpct_sized_free(ptr, s);
}

void get_heap_profile(const void** ptr, size_t* size) {
  DEBUG_ASSERT(ptr);
  DEBUG_ASSERT(size);
#if KERNEL_MEMORY_PROFILER
  std::span<const uint8_t> buf = alloc_map.data();
  *ptr = buf.data();
  *size = buf.size();
#else
  *ptr = nullptr;
  *size = 0;
#endif
}

uint32_t profile_track_alloc(size_t size) {
#if KERNEL_MEMORY_PROFILER
  Backtrace bt;
  Thread::Current::GetBacktrace(bt);
  Guard<SpinLock, IrqSave> guard(stat_lock::Get());
  auto [counters, handle] = alloc_map.try_get({bt.data(), bt.size()});
  if (counters) {
    counters->allocate(size);
    return static_cast<uint32_t>(handle);
  }
  alloc_map.event_dropped();
#endif
  return 0;
}

void profile_track_free(uint32_t handle, size_t size) {
#if KERNEL_MEMORY_PROFILER
  Guard<SpinLock, IrqSave> guard(stat_lock::Get());
  heap_profile::Counters* counters = alloc_map.try_by_handle(handle);
  if (counters) {
    counters->deallocate(size);
  } else {
    alloc_map.event_dropped();
  }
#endif
}

void* heap_page_alloc(size_t pages) {
  DEBUG_ASSERT(pages > 0);

  if (vm_using_virtual_heap()) {
    zx::result<vaddr_t> result = virtual_alloc->AllocPages(pages);
    if (result.is_error()) {
      printf("Failed to allocate %zu pages for heap: %d\n", pages, result.error_value());
      return nullptr;
    }

#if __has_feature(address_sanitizer)
    asan_poison_shadow(*result, pages * kPageSize, kAsanInternalHeapMagic);
#endif  // __has_feature(address_sanitizer)

    void* ret = reinterpret_cast<void*>(*result);
    LTRACEF("pages %zu: va %p\n", pages, ret);
    return ret;
  } else {
    VmPageDoublyLinkedList list;

    paddr_t pa;
    zx_status_t status = pmm_alloc_contiguous(pages, 0, kPageShift, &pa, &list);
    if (status != ZX_OK) {
      return nullptr;
    }

    // mark all of the allocated page as HEAP
    while (vm_page_t* p = list.pop_front()) {
      p->set_state(vm_page_state::HEAP);
#if __has_feature(address_sanitizer)
      void* const vaddr = paddr_to_physmap(p->paddr());
      asan_poison_shadow(reinterpret_cast<uintptr_t>(vaddr), kPageSize, kAsanInternalHeapMagic);
#endif  // __has_feature(address_sanitizer)
    }

    LTRACEF("pages %zu: pa %#lx, va %p\n", pages, pa, paddr_to_physmap(pa));

    return paddr_to_physmap(pa);
  }
}

void heap_page_free(void* _ptr, size_t pages) {
  DEBUG_ASSERT(IsPageRounded((uintptr_t)_ptr));
  DEBUG_ASSERT(pages > 0);

  LTRACEF("ptr %p, pages %zu\n", _ptr, pages);

  if (vm_using_virtual_heap()) {
    vaddr_t ptr = reinterpret_cast<vaddr_t>(_ptr);
    virtual_alloc->FreePages(ptr, pages);
  } else {
    uint8_t* ptr = (uint8_t*)_ptr;

    VmPageDoublyLinkedList list;

    while (pages > 0) {
      vm_page_t* p = paddr_to_vm_page(vaddr_to_paddr(ptr));
      if (p) {
        DEBUG_ASSERT(p->state() == vm_page_state::HEAP);
        DEBUG_ASSERT(!p->queue_node.InContainer());

        list.push_back(p);
      }

      ptr += kPageSize;
      pages--;
    }

    pmm_free(&list);
  }
}

void heap_report_alloc_failure(size_t size) {
  Pmm::Node().ReportAllocFailure(
      PmmNode::AllocFailure{.type = PmmNode::AllocFailure::Type::Heap, .size = size});
}

extern "C" {

void cpp_heap_dump_stats() { dump_stats(); }

}  // extern "C"
