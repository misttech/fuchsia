// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm_node.h"

#include <align.h>
#include <assert.h>
#include <inttypes.h>
#include <lib/boot-options/boot-options.h>
#include <lib/counters.h>
#include <lib/crypto/global_prng.h>
#include <lib/instrumentation/asan.h>
#include <lib/memalloc/range.h>
#include <lib/zircon-internal/macros.h>
#include <trace.h>

#include <new>

#include <fbl/algorithm.h>
#include <kernel/auto_preempt_disabler.h>
#include <kernel/mp.h>
#include <kernel/scheduler.h>
#include <kernel/thread.h>
#include <phys/handoff.h>
#include <pretty/cpp/sizes.h>
#include <vm/compression.h>
#include <vm/phys/arena.h>
#include <vm/physmap.h>
#include <vm/pmm.h>
#include <vm/pmm_checker.h>

#include "vm_priv.h"

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

// The number of PMM allocation calls that have failed.
KCOUNTER(pmm_alloc_failed, "vm.pmm.alloc.failed")
KCOUNTER(pmm_alloc_delayed, "vm.pmm.alloc.delayed")

namespace {

// Indicates whether a PMM alloc call has ever failed with ZX_ERR_NO_MEMORY.  Used to trigger an
// OOM response.  See |MemoryWatchdog::WorkerThread|.
ktl::atomic<bool> alloc_failed_no_mem;

// Poison a page |p| with value |value|. Accesses to a poisoned page via the physmap are not
// allowed and may cause faults or kASAN checks.
void AsanPoisonPage(vm_page_t* p, uint8_t value) {
#if __has_feature(address_sanitizer)
  asan_poison_shadow(reinterpret_cast<uintptr_t>(paddr_to_physmap(p->paddr())), PAGE_SIZE, value);
#endif  // __has_feature(address_sanitizer)
}

// Unpoison a page |p|. Accesses to a unpoisoned pages will not cause KASAN check failures.
void AsanUnpoisonPage(vm_page_t* p) {
#if __has_feature(address_sanitizer)
  asan_unpoison_shadow(reinterpret_cast<uintptr_t>(paddr_to_physmap(p->paddr())), PAGE_SIZE);
#endif  // __has_feature(address_sanitizer)
}

void ReturnPagesToFreeList(list_node* target_list, list_node* to_free) {
  if constexpr (!__has_feature(address_sanitizer)) {
    // splice list at the head of free_list_; free_loaned_list_.
    list_splice_after(to_free, target_list);
  } else {
    // If address sanitizer is enabled, put the pages at the tail to maximize reuse distance.
    if (!list_is_empty(target_list)) {
      list_splice_after(to_free, list_peek_tail(target_list));
    } else {
      list_splice_after(to_free, target_list);
    }
  }
}

}  // namespace

// We disable thread safety analysis here, since this function is only called
// during early boot before threading exists.
zx_status_t PmmNode::Init(ktl::span<const memalloc::Range> ranges) TA_NO_THREAD_SAFETY_ANALYSIS {
  // Make sure we're in early boot (ints disabled and no active Schedulers)
  DEBUG_ASSERT(Scheduler::PeekActiveMask() == 0);
  DEBUG_ASSERT(arch_ints_disabled());

  zx_status_t status = ZX_OK;
  auto init_arena = [&status, this](const PmmArenaSelection& selected) {
    if (status == ZX_ERR_NO_MEMORY) {
      return;
    }
    zx_status_t init_status = InitArena(selected);
    if (status == ZX_OK) {
      status = init_status;
    }
  };

  bool allocation_excluded = false;
  auto record_error = [&allocation_excluded](const PmmArenaSelectionError& error) {
    bool allocated = memalloc::IsAllocatedType(error.range.type);
    allocation_excluded = allocation_excluded || allocated;

    // If we have to throw out less than two pages of free RAM, don't regard
    // that as a full blown error.
    const char* error_type =
        error.type == PmmArenaSelectionError::Type::kTooSmall && !allocated ? "warning" : "error";
    ktl::string_view reason = PmmArenaSelectionError::ToString(error.type);
    ktl::string_view range_type = memalloc::ToString(error.range.type);
    printf("PMM: %s: unable to include [%#" PRIx64 ", %#" PRIx64 ") (%.*s) in arena: %.*s\n",
           error_type,                                              //
           error.range.addr, error.range.end(),                     //
           static_cast<int>(range_type.size()), range_type.data(),  //
           static_cast<int>(reason.size()), reason.data());
  };

  SelectPmmArenas<PAGE_SIZE>(ranges, init_arena, record_error);
  if (status != ZX_OK) {
    return status;
  }

  // If we fail to include a pre-PMM allocation in an arena that could be
  // disastrous in unpredictable/hard-to-debug ways, so fail hard early.
  ZX_ASSERT(!allocation_excluded);

  // Now mark all pre-PMM allocations and holes within our arenas as reserved.
  ktl::span arenas = active_arenas();
  auto reserve_range = [this, arena{arenas.begin()},
                        end{arenas.end()}](const memalloc::Range& range) mutable {
    // Find the first arena encompassing this range.
    //
    // Note that trying to include `range` in an arena may have resulted in an
    // error during the selection process. If we do encounter a range not in an
    // arena, just skip it.
    while (arena != end && arena->end() <= range.addr) {
      ++arena;
    }
    if (arena == end) {
      // In this case the tail of ranges did not end up in any arenas, so we can
      // just short-circuit.
      return false;
    }
    if (!arena->address_in_arena(range.addr)) {
      return true;
    }

    DEBUG_ASSERT(arena->address_in_arena(range.end() - 1));
    InitReservedRange(range);
    return true;
  };
  ForEachAlignedAllocationOrHole<PAGE_SIZE>(ranges, reserve_range);

  return ZX_OK;
}

void PmmNode::EndHandoff() {
  FreeList(&phys_handoff_temporary_list_);
  ZX_ASSERT(list_is_empty(&phys_handoff_vmo_list_));
}

zx_status_t PmmNode::GetArenaInfo(size_t count, uint64_t i, pmm_arena_info_t* buffer,
                                  size_t buffer_size) {
  Guard<Mutex> guard{&lock_};

  if ((count == 0) || (count + i > active_arenas().size()) || (i >= active_arenas().size())) {
    return ZX_ERR_OUT_OF_RANGE;
  }
  const size_t size_required = count * sizeof(pmm_arena_info_t);
  if (buffer_size < size_required) {
    return ZX_ERR_BUFFER_TOO_SMALL;
  }

  // Skip the first |i| elements.
  auto iter = active_arenas().begin();
  for (uint64_t j = 0; j < i; j++) {
    iter++;
  }

  // Copy the next |count| elements.
  for (uint64_t j = 0; j < count; j++) {
    buffer[j] = iter->info();
    iter++;
  }

  return ZX_OK;
}

// called at boot time as arenas are brought online, no locks are acquired
void PmmNode::AddFreePages(list_node* list) TA_NO_THREAD_SAFETY_ANALYSIS {
  LTRACEF("list %p\n", list);

  uint64_t free_count = 0;
  vm_page *temp, *page;
  list_for_every_entry_safe (list, page, temp, vm_page, queue_node) {
    list_delete(&page->queue_node);
    DEBUG_ASSERT(!page->is_loaned());
    DEBUG_ASSERT(!page->is_loan_cancelled());
    DEBUG_ASSERT(page->is_free());
    list_add_tail(&free_list_, &page->queue_node);
    ++free_count;
  }
  free_count_.fetch_add(free_count);
  ASSERT(free_count_);
  free_pages_evt_.Signal();

  LTRACEF("free count now %" PRIu64 "\n", free_count_.load(ktl::memory_order_relaxed));
}

void PmmNode::FillFreePagesAndArm() {
  // Require both locks so we can process both of the free lists and modify all_free_pages_filled_.
  Guard<Mutex> loaned_guard{&loaned_list_lock_};
  Guard<Mutex> free_guard{&lock_};

  if (!free_fill_enabled_) {
    return;
  }

  vm_page* page;
  list_for_every_entry (&free_list_, page, vm_page, queue_node) {
    checker_.FillPattern(page);
  }
  list_for_every_entry (&free_loaned_list_, page, vm_page, queue_node) {
    checker_.FillPattern(page);
  }

  // Now that every page has been filled, we can arm the checker.
  checker_.Arm();
  all_free_pages_filled_ = true;

  checker_.PrintStatus(stdout);
}

void PmmNode::CheckAllFreePages() {
  // Require both locks so we can process both of the free lists. This is an infrequent manual
  // operation and does not need to be optimized to avoid holding both locks at once.
  Guard<Mutex> loaned_guard{&loaned_list_lock_};
  Guard<Mutex> free_guard{&lock_};

  if (!checker_.IsArmed()) {
    return;
  }

  uint64_t free_page_count = 0;
  uint64_t free_loaned_page_count = 0;
  vm_page* page;
  list_for_every_entry (&free_list_, page, vm_page, queue_node) {
    checker_.AssertPattern(page);
    ++free_page_count;
  }
  list_for_every_entry (&free_loaned_list_, page, vm_page, queue_node) {
    checker_.AssertPattern(page);
    ++free_loaned_page_count;
  }

  ASSERT(free_page_count == free_count_.load(ktl::memory_order_relaxed));
  ASSERT(free_loaned_page_count == free_loaned_count_.load(ktl::memory_order_relaxed));
}

#if __has_feature(address_sanitizer)
void PmmNode::PoisonAllFreePages() {
  // Require both locks so we can process both of the free lists. This is an infrequent manual
  // operation and does not need to be optimized to avoid holding both locks at once.
  Guard<Mutex> loaned_guard{&loaned_list_lock_};
  Guard<Mutex> free_guard{&lock_};

  vm_page* page;
  list_for_every_entry (&free_list_, page, vm_page, queue_node) {
    AsanPoisonPage(page, kAsanPmmFreeMagic);
  };
  list_for_every_entry (&free_loaned_list_, page, vm_page, queue_node) {
    AsanPoisonPage(page, kAsanPmmFreeMagic);
  };
}
#endif  // __has_feature(address_sanitizer)

bool PmmNode::EnableFreePageFilling(size_t fill_size, CheckFailAction action) {
  // Require both locks so we can manipulate free_fill_enabled_.
  Guard<Mutex> loaned_guard{&loaned_list_lock_};
  Guard<Mutex> free_guard{&lock_};
  if (free_fill_enabled_) {
    // Checker is already enabled.
    return false;
  }
  checker_.SetFillSize(fill_size);
  checker_.SetAction(action);
  // As free_fill_enabled_ may be examined outside of the lock, ensure the manipulations to checker_
  // complete first by performing a release. See IsFreeFillEnabledRacy for where the acquire is
  // performed.
  free_fill_enabled_.store(true, ktl::memory_order_release);
  return true;
}

void PmmNode::AllocPageHelperLocked(vm_page_t* page) {
  LTRACEF("allocating page %p, pa %#" PRIxPTR ", prev state %s\n", page, page->paddr(),
          page_state_to_string(page->state()));

  AsanUnpoisonPage(page);

  DEBUG_ASSERT(page->is_free() && !page->is_loaned());

  // Here we transition the page from FREE->ALLOC, completing the transfer of ownership from the
  // PmmNode to the stack. This must be done under lock_, and more specifically the same lock_
  // acquisition that removes the page from the free list, as both being the free list, or being
  // in the ALLOC state, indicate ownership by the PmmNode.
  page->set_state(vm_page_state::ALLOC);
  // Used by the FLPH for loaned pages, but cleared here for consistency to ensure no stale pointers
  // that could be accidentally referenced.
  page->alloc.owner = nullptr;
}

void PmmNode::AllocLoanedPageHelperLocked(vm_page_t* page) {
  LTRACEF("allocating loaned page %p, pa %#" PRIxPTR ", prev state %s\n", page, page->paddr(),
          page_state_to_string(page->state()));

  AsanUnpoisonPage(page);

  DEBUG_ASSERT(page->is_free_loaned() && page->is_loaned());

  // Here we transition the page from FREE_LOANED->ALLOC, completing the transfer of ownership from
  // the PmmNode to the stack. This must be done under loaned_pages_lock_, and more specifically
  // the same loaned_pages_lock_ acquisition that removes the page from the free list, as both being
  // the free list, or being in the ALLOC state, indicate ownership by the PmmNode.
  page->set_state(vm_page_state::ALLOC);
  page->alloc.owner = nullptr;
}

zx::result<vm_page_t*> PmmNode::AllocLoanedPage(
    fit::inline_function<void(vm_page_t*), 32> allocated) {
  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());
  AutoPreemptDisabler preempt_disable;

  bool free_list_had_fill_pattern = false;
  vm_page* page = nullptr;
  {
    Guard<Mutex> guard{&loaned_list_lock_};
    free_list_had_fill_pattern = FreePagesFilledLoanedLocked();

    page = list_remove_head_type(&free_loaned_list_, vm_page, queue_node);
    if (!page) {
      // Does not count as out of memory, so do not report an allocation failure, just tell the
      // caller we are out of resources.
      return zx::error(ZX_ERR_NO_RESOURCES);
    }

    AllocLoanedPageHelperLocked(page);

    DecrementFreeLoanedCountLocked(1);

    // Run the callback while still holding the lock.
    allocated(page);
    // Before we drop the loaned list lock the page is expected to be in the object state with a
    // back pointer.
    DEBUG_ASSERT(page->state() == vm_page_state::OBJECT && page->object.get_object());
  }

  if (free_list_had_fill_pattern) {
    checker_.AssertPattern(page);
  }

  return zx::ok(page);
}

zx::result<vm_page_t*> PmmNode::AllocPage(uint alloc_flags) {
  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());

  vm_page* page = nullptr;
  bool free_list_had_fill_pattern = false;

  {
    AutoPreemptDisabler preempt_disable;
    Guard<Mutex> guard{&lock_};
    free_list_had_fill_pattern = FreePagesFilledLocked();

    if ((alloc_flags & PMM_ALLOC_FLAG_CAN_WAIT) && ShouldDelayAllocationLocked()) {
      pmm_alloc_delayed.Add(1);
      return zx::error(ZX_ERR_SHOULD_WAIT);
    }

    page = list_remove_head_type(&free_list_, vm_page, queue_node);
    if (!page) {
      // Allocation failures from the regular free list are likely to become user-visible.
      ReportAllocFailureLocked(AllocFailure{.type = AllocFailure::Type::Pmm, .size = 1});
      return zx::error(ZX_ERR_NO_MEMORY);
    }

    AllocPageHelperLocked(page);

    DecrementFreeCountLocked(1);
  }

  if (free_list_had_fill_pattern) {
    checker_.AssertPattern(page);
  }

  return zx::ok(page);
}

zx_status_t PmmNode::AllocPages(size_t count, uint alloc_flags, list_node* list) {
  LTRACEF("count %zu\n", count);

  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());
  // list must be initialized prior to calling this
  DEBUG_ASSERT(list);

  if (unlikely(count == 0)) {
    return ZX_OK;
  } else if (count == 1) {
    zx::result<vm_page_t*> result = AllocPage(alloc_flags);
    if (result.is_ok()) {
      vm_page_t* page = result.value();
      list_add_tail(list, &page->queue_node);
    }
    return result.status_value();
  }

  bool free_list_had_fill_pattern = false;
  // Holds the pages that we pull out of the PMMs free list. These pages may still need to have
  // their pattern checked (based on the bool above) before being appended to |list| and returned to
  // the caller.
  list_node_t alloc_list = LIST_INITIAL_VALUE(alloc_list);
  {
    AutoPreemptDisabler preempt_disable;
    Guard<Mutex> guard{&lock_};
    free_list_had_fill_pattern = FreePagesFilledLocked();

    uint64_t free_count = free_count_.load(ktl::memory_order_relaxed);
    // based on whether allocated loaned pages or not, setup which_list to point directly to the
    // appropriate free list to simplify later allocation code that operates on either list.

    if (unlikely(count > free_count)) {
      if ((alloc_flags & PMM_ALLOC_FLAG_CAN_WAIT) && should_wait_ != ShouldWaitState::Never) {
        pmm_alloc_delayed.Add(1);
        return ZX_ERR_SHOULD_WAIT;
      }
      // Allocation failures from the regular free list are likely to become user-visible.
      ReportAllocFailureLocked(AllocFailure{.type = AllocFailure::Type::Pmm, .size = count});
      return ZX_ERR_NO_MEMORY;
    }

    DecrementFreeCountLocked(count);

    if ((alloc_flags & PMM_ALLOC_FLAG_CAN_WAIT) && ShouldDelayAllocationLocked()) {
      IncrementFreeCountLocked(count);
      pmm_alloc_delayed.Add(1);
      return ZX_ERR_SHOULD_WAIT;
    }
    list_node_t* node = &free_list_;
    while (count > 0) {
      node = list_next(&free_list_, node);
      AllocPageHelperLocked(containerof(node, vm_page, queue_node));
      --count;
    }

    // Want to take the pages ranging from the start of the list (identified by which_list) up to
    // node, and place them in alloc_list. Due to how the listnode operations work, it's easier to
    // move the entire list into alloc_list, then split the pages that we are not allocating back
    // into which_list.
    list_move(&free_list_, &alloc_list);
    list_split_after(&alloc_list, node, &free_list_);
  }

  // Check the pages we are allocating before appending them into the user's allocation list. Do
  // this check before since we must not existing pages in the user's allocation list, as they are
  // completely arbitrary pages and there's no reason to expect a fill pattern in them.
  if (free_list_had_fill_pattern) {
    vm_page* page;
    list_for_every_entry (&alloc_list, page, vm_page, queue_node) {
      checker_.AssertPattern(page);
    }
  }

  // Append the checked list onto the user provided list.
  if (list_is_empty(list)) {
    list_move(&alloc_list, list);
  } else {
    list_splice_after(&alloc_list, list_peek_tail(list));
  }

  return ZX_OK;
}

zx_status_t PmmNode::AllocRange(paddr_t address, size_t count, list_node* list) {
  LTRACEF("address %#" PRIxPTR ", count %zu\n", address, count);

  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());
  // list must be initialized prior to calling this
  DEBUG_ASSERT(list);
  // On error scenarios we will free the list, so make sure the caller didn't leave anything in
  // there.
  DEBUG_ASSERT(list_is_empty(list));

  size_t allocated = 0;
  if (count == 0) {
    return ZX_OK;
  }

  address = ROUNDDOWN_PAGE_SIZE(address);

  bool free_list_had_fill_pattern = false;

  {
    AutoPreemptDisabler preempt_disable;
    Guard<Mutex> guard{&lock_};
    free_list_had_fill_pattern = FreePagesFilledLocked();

    // walk through the arenas, looking to see if the physical page belongs to it
    for (auto& a : active_arenas()) {
      for (; allocated < count && a.address_in_arena(address); address += PAGE_SIZE) {
        vm_page_t* page = a.FindSpecific(address);
        if (!page) {
          break;
        }

        // As we hold lock_, we can assume that any page in the FREE state is owned by us, and
        // protected by lock_, and so should is_free() be true we will be allowed to assume it is in
        // the free list, remove it from said list, and allocate it.
        if (!page->is_free()) {
          break;
        }

        // We never allocate loaned pages for caller of AllocRange()
        if (page->is_loaned()) {
          break;
        }

        list_delete(&page->queue_node);

        AllocPageHelperLocked(page);

        list_add_tail(list, &page->queue_node);

        allocated++;
        DecrementFreeCountLocked(1);
      }

      if (allocated == count) {
        break;
      }
    }

    if (allocated != count) {
      // We were not able to allocate the entire run, free these pages. As we allocated these pages
      // under this lock acquisition, the fill status is whatever it was before, i.e. the status of
      // whether free pages have all been filled..
      FreeListLocked(list, FreePagesFilledLocked());
      return ZX_ERR_NOT_FOUND;
    }
  }

  if (free_list_had_fill_pattern) {
    vm_page* page;
    list_for_every_entry (list, page, vm_page, queue_node) {
      checker_.AssertPattern(page);
    }
  }

  return ZX_OK;
}

zx_status_t PmmNode::AllocContiguous(const size_t count, uint alloc_flags, uint8_t alignment_log2,
                                     paddr_t* pa, list_node* list) {
  DEBUG_ASSERT(Thread::Current::memory_allocation_state().IsEnabled());
  LTRACEF("count %zu, align %u\n", count, alignment_log2);

  if (count == 0) {
    return ZX_OK;
  }
  if (alignment_log2 < PAGE_SIZE_SHIFT) {
    alignment_log2 = PAGE_SIZE_SHIFT;
  }

  DEBUG_ASSERT(!(alloc_flags & PMM_ALLOC_FLAG_CAN_WAIT));
  // pa and list must be valid pointers
  DEBUG_ASSERT(pa);
  DEBUG_ASSERT(list);

  AutoPreemptDisabler preempt_disable;
  Guard<Mutex> guard{&lock_};

  for (auto& a : active_arenas()) {
    // FindFreeContiguous will search the arena for FREE pages. As we hold lock_, any pages in the
    // FREE state are assumed to be owned by us, and would only be modified if lock_ were held.
    vm_page_t* p = a.FindFreeContiguous(count, alignment_log2);
    if (!p) {
      continue;
    }

    *pa = p->paddr();

    // remove the pages from the run out of the free list
    for (size_t i = 0; i < count; i++, p++) {
      DEBUG_ASSERT_MSG(p->is_free(), "p %p state %u\n", p, static_cast<uint32_t>(p->state()));
      // Loaned pages are never returned by FindFreeContiguous() above.
      DEBUG_ASSERT(!p->is_loaned());
      DEBUG_ASSERT(list_in_list(&p->queue_node));

      // Atomically (that is, in a single lock acquisition) remove this page from both the free list
      // and FREE state, ensuring it is owned by us.
      list_delete(&p->queue_node);
      p->set_state(vm_page_state::ALLOC);

      DecrementFreeCountLocked(1);
      AsanUnpoisonPage(p);
      checker_.AssertPattern(p);

      list_add_tail(list, &p->queue_node);
    }

    return ZX_OK;
  }

  // We could potentially move contents of non-pinned pages out of the way for critical contiguous
  // allocations, but for now...
  LTRACEF("couldn't find run\n");
  return ZX_ERR_NOT_FOUND;
}

// We disable thread safety analysis here, since this function is only called
// during early boot before threading exists.
zx_status_t PmmNode::InitArena(const PmmArenaSelection& selected) TA_NO_THREAD_SAFETY_ANALYSIS {
  if (used_arena_count_ >= kArenaCount) {
    return ZX_ERR_NOT_SUPPORTED;
  }
  if (selected.arena.size > (kMaxPagesPerArena * PAGE_SIZE)) {
    // We have this limit since we need to compress a page_t pointer to a 24 bit integer.
    return ZX_ERR_NOT_SUPPORTED;
  }

  arenas_[used_arena_count_++].Init(selected, this);
  arena_cumulative_size_ += selected.arena.size;
  return ZX_OK;
}

void PmmNode::InitReservedRange(const memalloc::Range& range) {
  DEBUG_ASSERT(IS_PAGE_ROUNDED(range.addr));
  DEBUG_ASSERT(IS_PAGE_ROUNDED(range.size));

  ktl::string_view what =
      range.type == memalloc::Type::kReserved ? "hole in RAM"sv : memalloc::ToString(range.type);
  list_node reserved = LIST_INITIAL_VALUE(reserved);
  zx_status_t status = pmm_alloc_range(range.addr, range.size / PAGE_SIZE, &reserved);
  if (status != ZX_OK) {
    dprintf(INFO, "PMM: unable to reserve [%#" PRIx64 ", %#" PRIx64 "): %.*s: %d\n", range.addr,
            range.end(), static_cast<int>(what.size()), what.data(), status);
    return;  // this is probably fatal but go ahead and continue
  }
  dprintf(INFO, "PMM: reserved [%#" PRIx64 ", %#" PRIx64 "): %.*s\n", range.addr, range.end(),
          static_cast<int>(what.size()), what.data());

  // Kernel page tables belong to the arch-specific VM backend, just as they'd
  // be if they were created post-Physboot.
  if (range.type == memalloc::Type::kKernelPageTables) {
    ArchVmAspace::HandoffPageTablesFromPhysboot(&reserved);
    return;
  }

  // Otherwise, mark it as wired and merge it into the appropriate reserved
  // list.
  vm_page_t* p;
  list_for_every_entry (&reserved, p, vm_page_t, queue_node) {
    p->set_state(vm_page_state::WIRED);
  }

  list_node_t* list;
  if (range.type == memalloc::Type::kTemporaryPhysHandoff) {
    list = &phys_handoff_temporary_list_;
  } else if (PhysHandoff::IsPhysVmoType(range.type)) {
    list = &phys_handoff_vmo_list_;
  } else {
    list = &permanently_reserved_list_;
  }
  if (list_is_empty(list)) {
    list_move(&reserved, list);
  } else {
    list_splice_after(&reserved, list_peek_tail(list));
  }
}

void PmmNode::FreePageHelperLocked(vm_page* page, bool already_filled) {
  LTRACEF("page %p state %zu paddr %#" PRIxPTR "\n", page, VmPageStateIndex(page->state()),
          page->paddr());

  DEBUG_ASSERT(!page->is_free());
  DEBUG_ASSERT(!page->is_free_loaned());
  DEBUG_ASSERT(page->state() != vm_page_state::OBJECT ||
               (page->object.pin_count == 0 && page->object.get_object() == nullptr));

  // mark it free. This makes the page owned the PmmNode, even though it may not be in any page
  // list, since the page is findable via the arena, and so we must ensure to:
  // 1. Be performing set_state here under the lock_
  // 2. Place the page in the free list and cease referring to the page before ever dropping lock_
  page->set_state(vm_page_state::FREE);

  // This page cannot be loaned.
  DEBUG_ASSERT(!page->is_loaned());

  // The caller may have called RacyFreeFillEnabled and potentially already filled a pattern,
  // however if it raced with enabling of free filling we may still need to fill the pattern. This
  // should be unlikely, and since free filling can never be turned back off there is no race in the
  // other direction.
  if (FreeFillEnabledLocked() && !already_filled) {
    checker_.FillPattern(page);
  }

  AsanPoisonPage(page, kAsanPmmFreeMagic);
}

void PmmNode::FreeLoanedPageHelperLocked(vm_page* page, bool already_filled) {
  LTRACEF("page %p state %zu paddr %#" PRIxPTR "\n", page, VmPageStateIndex(page->state()),
          page->paddr());

  DEBUG_ASSERT(!page->is_free());
  DEBUG_ASSERT(page->state() != vm_page_state::OBJECT || page->object.pin_count == 0);
  DEBUG_ASSERT(page->state() != vm_page_state::ALLOC || page->alloc.owner == nullptr);

  // mark it free. This makes the page owned the PmmNode and even though it may not be in any page
  // list, since the page is findable via the arena we must ensure the following happens:
  // 1. We hold loaned_list_lock_ preventing pages from transition to/from loaned
  // 2. This page is loaned and hence will not be considered by an arena traversal that holds lock_
  // 3. Perform set_state here under the loaned_list_lock_
  // 4. Place the page in the loaned_free_list_ and cease referring to the page before ever dropping
  // the loaned_list_lock_.
  page->set_state(vm_page_state::FREE_LOANED);

  // The caller may have called IsFreeFillEnabledRacy and potentially already filled a pattern,
  // however if it raced with enabling of free filling we may still need to fill the pattern. This
  // should be unlikely, and since free filling can never be turned back off there is no race in the
  // other direction. As we hold lock we can safely perform a relaxed read.
  if (!already_filled && FreeFillEnabledLoanedLocked()) {
    checker_.FillPattern(page);
  }

  AsanPoisonPage(page, kAsanPmmFreeMagic);
}

void PmmNode::BeginFreeLoanedPage(vm_page_t* page,
                                  fit::inline_function<void(vm_page_t*)> release_page,
                                  FreeLoanedPagesHolder& flph) {
  AutoPreemptDisabler preempt_disable;
  DEBUG_ASSERT(page->is_loaned());
  // On entry we require that the page has a valid backlink.
  DEBUG_ASSERT(page->state() == vm_page_state::OBJECT && page->object.get_object());

  Guard<Mutex> guard{&loaned_list_lock_};
  release_page(page);

  // pages freed individually shouldn't be in a queue
  DEBUG_ASSERT(!list_in_list(&page->queue_node));

  DEBUG_ASSERT(!flph.used_);
  page->set_state(vm_page_state::ALLOC);
  page->alloc.owner = &flph;
  list_add_head(&flph.pages_, &page->queue_node);
}

void PmmNode::FinishFreeLoanedPages(FreeLoanedPagesHolder& flph) {
  if (list_is_empty(&flph.pages_)) {
    return;
  }
  const bool fill = IsFreeFillEnabledRacy();
  if (fill) {
    vm_page_t* p;
    list_for_every_entry (&flph.pages_, p, vm_page_t, queue_node) {
      checker_.FillPattern(p);
    }
  }
  bool waiters;
  {
    AutoPreemptDisabler preempt_disable;
    Guard<Mutex> guard{&loaned_list_lock_};
    DEBUG_ASSERT(!flph.used_);
    flph.used_ = true;
    FreeLoanedListLocked(&flph.pages_, fill, [&](vm_page_t* page) {
      DEBUG_ASSERT(page->state() == vm_page_state::ALLOC);
      DEBUG_ASSERT(page->alloc.owner == &flph);
      page->alloc.owner = nullptr;
    });
    // We hold the lock and have removed all the pages from the list (clearing their owner in the
    // process) and so whatever waiters that presently exist are all the ones that can exist.
    waiters = flph.num_waiters_ > 0;
    // If we have waiters then we need to manipulate the event objects while we still hold the lock,
    // but this can be skipped if there are no waiters.
    if (waiters) {
      // Unblock all waiters. As freed_pages_event_ is a regular event, and not AutoUnsignal, this
      // means that even waiters that have not progress through to the actual |Wait| operation will
      // not block.
      flph.freed_pages_event_.Signal();
    }
  }
  // If there were any waiters we must wait for them to complete. This is necessary since
  // |WithLoanedPage| holds a pointer to |flph|, but has no way to keep the object alive. As such we
  // must not return until we know that |WithLoanedPage| has ceased holding any references to
  // |flph|.
  if (waiters) {
    // First wait for any waiters to complete. This event gets signalled by the last waiter in
    // |WithLoanedPage| with the locks held.
    flph.no_waiters_event_.Wait();
    // Our signaler in |WithLoanedPage| may still be referencing the no_waiters_event and so we
    // still cannot return as that is a reference to the |flph| object. Therefore we perform a lock
    // acquisition which, once it succeeds, tells us that |WithLoanedPage| has concluded its
    // references to |flph|.
    Guard<Mutex> guard{&loaned_list_lock_};
  }
}

void PmmNode::WithLoanedPage(vm_page_t* page, fit::inline_function<void(vm_page_t*)> with_page) {
  // Technically users could race with |WithLoanedPage| and re-allocate the page after it gets
  // migrated to the PmmNode, and then place it back in a new FLPH before a stable state can be
  // observed. Such behavior almost certainly represents a kernel bug, so if we detect multiple
  // iterations to track the page down we generate a warning.
  for (int iterations = 0;; iterations++) {
    FreeLoanedPagesHolder* flph = nullptr;
    {
      AutoPreemptDisabler preempt_disable;
      Guard<Mutex> guard{&loaned_list_lock_};
      DEBUG_ASSERT(page->is_loaned());
      if (page->state() != vm_page_state::ALLOC || !page->alloc.owner) {
        with_page(page);
        return;
      }
      flph = page->alloc.owner;
      flph->num_waiters_++;
    }
    if (iterations > 0) {
      printf("WARNING: Required multiple attempts (%d) to track down loaned page %p\n", iterations,
             page);
    }
    // We incremented num_waiters_ under the lock while there were pages in the list, so it is
    // guaranteed that |FinishFreeLoanedPages| will see this and signal the event.
    flph->freed_pages_event_.Wait();
    {
      AutoPreemptDisabler preempt_disable;
      Guard<Mutex> guard{&loaned_list_lock_};
      // With the lock re-acquired indicate we have completed waiting.
      flph->num_waiters_--;
      if (flph->num_waiters_ == 0) {
        // If we were the last thread to complete the wait process signal |FinishFreeLoanedPages| so
        // that it knows we have (almost) finished any references to |flph|. We still hold one
        // final reference, the flph->no_waiters_event_, but that will be resolved by
        // |FinishFreeLoanedPages| waiting for the lock (see comments in that method).
        flph->no_waiters_event_.Signal();
      }
    }
  }
}

void PmmNode::FreePage(vm_page* page) {
  AutoPreemptDisabler preempt_disable;
  DEBUG_ASSERT(!page->is_loaned());
  const bool fill = IsFreeFillEnabledRacy();
  if (fill) {
    checker_.FillPattern(page);
  }
  Guard<Mutex> guard{&lock_};

  // pages freed individually shouldn't be in a queue
  DEBUG_ASSERT(!list_in_list(&page->queue_node));

  FreePageHelperLocked(page, fill);

  IncrementFreeCountLocked(1);
  if constexpr (!__has_feature(address_sanitizer)) {
    list_add_head(&free_list_, &page->queue_node);
  } else {
    // If address sanitizer is enabled, put the page at the tail to maximize reuse distance.
    list_add_tail(&free_list_, &page->queue_node);
  }
}

template <typename F>
void PmmNode::FreeLoanedListLocked(list_node* list, bool already_filled, F validator) {
  DEBUG_ASSERT(list);

  uint64_t count = 0;
  {  // scope page
    vm_page* page;
    vm_page* temp;
    list_for_every_entry_safe (list, page, temp, vm_page_t, queue_node) {
      validator(page);
      DEBUG_ASSERT(page->is_loaned());
      FreeLoanedPageHelperLocked(page, already_filled);
      if (page->is_loan_cancelled()) {
        // Loaned cancelled pages do not go back on the free list.
        list_delete(&page->queue_node);
      } else {
        count++;
      }
    }
  }  // end scope page

  ReturnPagesToFreeList(&free_loaned_list_, list);

  IncrementFreeLoanedCountLocked(count);
}

void PmmNode::FreeListLocked(list_node* list, bool already_filled) {
  DEBUG_ASSERT(list);

  uint64_t count = 0;
  {  // scope page
    vm_page* page;
    vm_page* temp;
    list_for_every_entry_safe (list, page, temp, vm_page_t, queue_node) {
      DEBUG_ASSERT(!page->is_loaned());
      FreePageHelperLocked(page, already_filled);
      count++;
    }
  }  // end scope page

  ReturnPagesToFreeList(&free_list_, list);

  IncrementFreeCountLocked(count);
}

void PmmNode::BeginFreeLoanedArray(
    vm_page_t** pages, size_t count,
    fit::inline_function<void(vm_page_t**, size_t, list_node_t*)> release_list,
    FreeLoanedPagesHolder& flph) {
  AutoPreemptDisabler preempt_disable;
  // On entry we expect all pages to have a backlink.
  DEBUG_ASSERT(ktl::all_of(&pages[0], &pages[count], [](vm_page_t* p) {
    return p->state() == vm_page_state::OBJECT && p->object.get_object();
  }));
  Guard<Mutex> guard{&loaned_list_lock_};
  DEBUG_ASSERT(!flph.used_);
  list_node_t free_list = LIST_INITIAL_VALUE(free_list);
  release_list(pages, count, &free_list);
  // Validate that the callback populated the free list correctly.
  vm_page_t* p;
  size_t expected = 0;
  list_for_every_entry (&free_list, p, vm_page_t, queue_node) {
    p->set_state(vm_page_state::ALLOC);
    p->alloc.owner = &flph;
    DEBUG_ASSERT(pages[expected] == p);
    expected++;
  }
  DEBUG_ASSERT(expected == count);
  list_splice_after(&free_list, &flph.pages_);
}

void PmmNode::FreeList(list_node* list) {
  AutoPreemptDisabler preempt_disable;
  const bool fill = IsFreeFillEnabledRacy();
  if (fill) {
    vm_page* page;
    list_for_every_entry (list, page, vm_page, queue_node) {
      checker_.FillPattern(page);
    }
  }
  Guard<Mutex> guard{&lock_};

  FreeListLocked(list, fill);
}

void PmmNode::UnwirePage(vm_page* page) {
  Guard<Mutex> guard{&lock_};
  ASSERT(page->state() == vm_page_state::WIRED);
  list_delete(&page->queue_node);
  page->set_state(vm_page_state::ALLOC);
}

bool PmmNode::ShouldDelayAllocationLocked() {
  if (should_wait_ == ShouldWaitState::UntilReset) {
    return true;
  }
  if (should_wait_ == ShouldWaitState::Never) {
    return false;
  }
  // See pmm_check_alloc_random_should_wait in pmm.cc for an assertion that random should wait is
  // only enabled if DEBUG_ASSERT_IMPLEMENTED.
  if constexpr (DEBUG_ASSERT_IMPLEMENTED) {
    // Randomly try to make 10% of allocations delayed allocations.
    if (gBootOptions->pmm_alloc_random_should_wait &&
        rand_r(&random_should_wait_seed_) < (RAND_MAX / 10)) {
      return true;
    }
  }
  return false;
}

uint64_t PmmNode::CountFreePages() const TA_NO_THREAD_SAFETY_ANALYSIS {
  return free_count_.load(ktl::memory_order_relaxed);
}

uint64_t PmmNode::CountLoanedFreePages() const TA_NO_THREAD_SAFETY_ANALYSIS {
  return free_loaned_count_.load(ktl::memory_order_relaxed);
}

uint64_t PmmNode::CountLoanedNotFreePages() const TA_NO_THREAD_SAFETY_ANALYSIS {
  AutoPreemptDisabler preempt_disable;
  // Require both locks to examine both counts.
  Guard<Mutex> loaned_guard{&loaned_list_lock_};
  Guard<Mutex> free_guard{&lock_};
  return loaned_count_.load(ktl::memory_order_relaxed) -
         free_loaned_count_.load(ktl::memory_order_relaxed);
}

uint64_t PmmNode::CountLoanedPages() const TA_NO_THREAD_SAFETY_ANALYSIS {
  return loaned_count_.load(ktl::memory_order_relaxed);
}

uint64_t PmmNode::CountLoanCancelledPages() const TA_NO_THREAD_SAFETY_ANALYSIS {
  return loan_cancelled_count_.load(ktl::memory_order_relaxed);
}

uint64_t PmmNode::CountTotalBytes() const TA_NO_THREAD_SAFETY_ANALYSIS {
  return arena_cumulative_size_;
}

void PmmNode::DumpFree() const TA_NO_THREAD_SAFETY_ANALYSIS {
  auto megabytes_free = CountFreePages() * PAGE_SIZE / MB;
  printf(" %zu free MBs\n", megabytes_free);
}

void PmmNode::Dump(bool is_panic) const {
  // No lock analysis here, as we want to just go for it in the panic case without the lock.
  auto dump = [this]() TA_NO_THREAD_SAFETY_ANALYSIS {
    uint64_t free_count = free_count_.load(ktl::memory_order_relaxed);
    uint64_t free_loaned_count = free_loaned_count_.load(ktl::memory_order_relaxed);
    printf(
        "pmm node %p: free_count %zu (%zu bytes), free_loaned_count: %zu (%zu bytes), total size "
        "%zu\n",
        this, free_count, free_count * PAGE_SIZE, free_loaned_count, free_loaned_count * PAGE_SIZE,
        arena_cumulative_size_);
    PmmStateCount count_sum = {};
    for (const auto& a : active_arenas()) {
      a.Dump(false, false, &count_sum);
    }
    printf("Totals\n");
    PrintPageStateCounts(count_sum);
  };

  if (is_panic) {
    dump();
  } else {
    Guard<Mutex> guard{&lock_};
    dump();
  }
}

void PmmNode::TripFreePagesLevelLocked() {
  if (should_wait_ == ShouldWaitState::OnceLevelTripped) {
    should_wait_ = ShouldWaitState::UntilReset;
    free_pages_evt_.Unsignal();
  }
}

bool PmmNode::SetFreeMemorySignal(uint64_t free_lower_bound, uint64_t free_upper_bound,
                                  uint64_t delay_allocations_pages, Event* event) {
  Guard<Mutex> guard{&lock_};
  // Ensure delay allocations is valid.
  DEBUG_ASSERT(delay_allocations_pages <= free_lower_bound ||
               delay_allocations_pages == UINT64_MAX);
  const uint64_t free_count = CountFreePages();
  if (free_count < free_lower_bound || free_count > free_upper_bound) {
    return false;
  }
  if (delay_allocations_pages == UINT64_MAX) {
    TripFreePagesLevelLocked();
  } else if (should_wait_ == ShouldWaitState::UntilReset) {
    free_pages_evt_.Signal();
    should_wait_ = ShouldWaitState::OnceLevelTripped;
  }
  should_wait_free_pages_level_ = delay_allocations_pages;
  mem_signal_lower_bound_ = free_lower_bound;
  mem_signal_upper_bound_ = free_upper_bound;
  mem_signal_ = event;
  return true;
}

void PmmNode::SignalFreeMemoryChangeLocked() {
  DEBUG_ASSERT(mem_signal_);
  mem_signal_->Signal();
  mem_signal_ = nullptr;
}

void PmmNode::StopReturningShouldWait() {
  Guard<Mutex> guard{&lock_};
  should_wait_ = ShouldWaitState::Never;
  free_pages_evt_.Signal();
}

int64_t PmmNode::get_alloc_failed_count() { return pmm_alloc_failed.SumAcrossAllCpus(); }

bool PmmNode::has_alloc_failed_no_mem() {
  return alloc_failed_no_mem.load(ktl::memory_order_relaxed);
}

void PmmNode::BeginLoan(list_node* page_list) {
  DEBUG_ASSERT(page_list);
  AutoPreemptDisabler preempt_disable;
  const bool fill = IsFreeFillEnabledRacy();
  if (fill) {
    vm_page* page;
    list_for_every_entry (page_list, page, vm_page, queue_node) {
      checker_.FillPattern(page);
    }
  }
  Guard<Mutex> guard{&loaned_list_lock_};

  uint64_t loaned_count = 0;
  vm_page* page;
  list_for_every_entry (page_list, page, vm_page, queue_node) {
    DEBUG_ASSERT(!page->is_loaned());
    DEBUG_ASSERT(!page->is_free());
    page->set_is_loaned();
    ++loaned_count;
    DEBUG_ASSERT(!page->is_loan_cancelled());
  }
  IncrementLoanedCountLocked(loaned_count);

  // Callers of BeginLoan() generally won't want the pages loaned to them; the intent is to loan to
  // the rest of the system, so go ahead and free also.  Some callers will basically choose between
  // pmm_begin_loan() and pmm_free().
  FreeLoanedListLocked(page_list, fill, [](vm_page_t* p) {});
}

void PmmNode::CancelLoan(vm_page_t* page) {
  AutoPreemptDisabler preempt_disable;
  // Require both locks in order to iterate the arenas and manipulate the loaned list.
  Guard<Mutex> loaned_guard{&loaned_list_lock_};
  Guard<Mutex> arena_guard{&lock_};
  DEBUG_ASSERT(page->is_loaned());
  DEBUG_ASSERT(!page->is_free());
  bool was_cancelled = page->is_loan_cancelled();
  // We can assert this because of PageSource's overlapping request
  // handling.
  DEBUG_ASSERT(!was_cancelled);
  page->set_is_loan_cancelled();
  IncrementLoanCancelledCountLocked(1);
  if (page->is_free_loaned()) {
    // Currently in free_loaned_list_.
    DEBUG_ASSERT(list_in_list(&page->queue_node));
    // Remove from free_loaned_list_ to prevent any new use until
    // after EndLoan.
    list_delete(&page->queue_node);
    DecrementFreeLoanedCountLocked(1);
  }
}

void PmmNode::EndLoan(vm_page_t* page) {
  bool free_list_had_fill_pattern = false;

  {
    AutoPreemptDisabler preempt_disable;
    // Require both locks in order to manipulate loaned pages and the regular free list.
    Guard<Mutex> loaned_guard{&loaned_list_lock_};
    Guard<Mutex> free_guard{&lock_};
    free_list_had_fill_pattern = FreePagesFilledLoanedLocked();

    // PageSource serializing such that there's only one request to
    // PageProvider in flight at a time for any given page is the main
    // reason we can assert these instead of needing to check these.
    DEBUG_ASSERT(page->is_loaned());
    DEBUG_ASSERT(page->is_loan_cancelled());
    DEBUG_ASSERT(page->is_free_loaned());

    // Already not in free_loaned_list_ (because loan_cancelled
    // already).
    DEBUG_ASSERT(!list_in_list(&page->queue_node));

    page->clear_is_loaned();
    page->clear_is_loan_cancelled();

    // Change the state to regular FREE. When this page was made
    // FREE_LOANED all of the pmm checker filling and asan work was
    // done, so we are safe to just change the state without using a
    // helper.
    page->set_state(vm_page_state::FREE);

    AllocPageHelperLocked(page);

    DecrementLoanCancelledCountLocked(1);
    DecrementLoanedCountLocked(1);
  }

  if (free_list_had_fill_pattern) {
    checker_.AssertPattern(page);
  }
}

void PmmNode::ReportAllocFailureLocked(AllocFailure failure) {
  kcounter_add(pmm_alloc_failed, 1);

  // Update before signaling the MemoryWatchdog to ensure it observes the update.
  //
  // |alloc_failed_no_mem| latches so only need to invoke the callback once.  We could call it on
  // every failure, but that's wasteful and we don't want to spam any underlying Event (or the
  // thread lock or the MemoryWatchdog).
  const bool first_time = !alloc_failed_no_mem.exchange(true, ktl::memory_order_relaxed);
  if (first_time) {
    first_alloc_failure_ = failure;
    first_alloc_failure_.free_count = free_count_;
  }
  if (first_time && mem_signal_) {
    SignalFreeMemoryChangeLocked();
  }
}

void PmmNode::ReportAllocFailure(AllocFailure failure) {
  Guard<Mutex> guard{&lock_};
  ReportAllocFailureLocked(failure);
}

PmmNode::AllocFailure PmmNode::GetFirstAllocFailure() {
  Guard<Mutex> guard{&lock_};
  return first_alloc_failure_;
}

void PmmNode::SeedRandomShouldWait() {
  if constexpr (DEBUG_ASSERT_IMPLEMENTED) {
    Guard<Mutex> guard{&lock_};
    crypto::global_prng::GetInstance()->Draw(&random_should_wait_seed_,
                                             sizeof(random_should_wait_seed_));
  }
}

zx_status_t PmmNode::SetPageCompression(fbl::RefPtr<VmCompression> compression) {
  Guard<Mutex> guard{&compression_lock_};
  if (page_compression_) {
    return ZX_ERR_ALREADY_EXISTS;
  }
  page_compression_ = ktl::move(compression);
  return ZX_OK;
}

const char* PmmNode::AllocFailure::TypeToString(Type type) {
  switch (type) {
    case Type::None:
      return "None";
    case Type::Pmm:
      return "PMM";
    case Type::Heap:
      return "Heap";
    case Type::Handle:
      return "Handle";
    case Type::Other:
      return "Other";
  }
  return "UNKNOWN";
}
