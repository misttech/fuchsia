// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/page_queues_ffi.h"

#include <zircon/types.h>

#include <kernel/event.h>
#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <vm/vm_cow_pages.h>

#include "vm/page_queues.h"
#include "vm/pmm.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_page_queues_stop_threads(PageQueues* queues) { queues->StopThreads(); }

FFI_ALWAYS_INLINE void cpp_page_queues_init(ffi::Uninitialized<PageQueues>* queues) {
  queues->Initialize();
}

FFI_ALWAYS_INLINE void* cpp_page_queues_get_lock(PageQueues* queues) { return queues->get_lock(); }

FFI_ALWAYS_INLINE const char* cpp_page_queues_string_from_age_reason(int32_t reason) {
  return PageQueues::string_from_age_reason(static_cast<PageQueues::AgeReason>(reason));
}

FFI_ALWAYS_INLINE void cpp_page_queues_rotate_reclaim_queues(PageQueues* queues) {
  queues->RotateReclaimQueues();
}

FFI_ALWAYS_INLINE bool cpp_page_queues_pop_anonymous_zero_fork(
    PageQueues* queues, PageQueuesVmoBacklink* out_backlink) {
  auto backlink = queues->PopAnonymousZeroFork();
  if (!backlink.has_value()) {
    return false;
  }
  out_backlink->cow = fbl::ExportToRawPtr(&backlink->cow);
  out_backlink->page = backlink->page;
  out_backlink->offset = backlink->offset;
  return true;
}

FFI_ALWAYS_INLINE bool cpp_page_queues_peek_isolate(PageQueues* queues, size_t lowest_queue,
                                                    PageQueuesVmoBacklink* out_backlink) {
  auto backlink = queues->PeekIsolate(lowest_queue);
  if (!backlink.has_value()) {
    return false;
  }
  out_backlink->cow = fbl::ExportToRawPtr(&backlink->cow);
  out_backlink->page = backlink->page;
  out_backlink->offset = backlink->offset;
  return true;
}

FFI_ALWAYS_INLINE bool cpp_page_queues_get_cow_for_loaned_page(
    PageQueues* queues, vm_page_t* page, PageQueuesVmoBacklink* out_backlink) {
  auto backlink = queues->GetCowForLoanedPage(page);
  if (!backlink.has_value()) {
    return false;
  }
  out_backlink->cow = fbl::ExportToRawPtr(&backlink->cow);
  out_backlink->page = backlink->page;
  out_backlink->offset = backlink->offset;
  return true;
}

FFI_ALWAYS_INLINE void cpp_page_queues_dump(const PageQueues* queues) { queues->Dump(); }

FFI_ALWAYS_INLINE void cpp_page_queues_start_threads(PageQueues* queues,
                                                     zx_duration_mono_t min_mru_rotate_time,
                                                     zx_duration_mono_t max_mru_rotate_time) {
  queues->StartThreads(min_mru_rotate_time, max_mru_rotate_time);
}

FFI_ALWAYS_INLINE void cpp_page_queues_start_debug_compressor(PageQueues* queues) {
  queues->StartDebugCompressor();
}

FFI_ALWAYS_INLINE void cpp_page_queues_set_active_ratio_multiplier(PageQueues* queues,
                                                                   uint32_t multiplier) {
  queues->SetActiveRatioMultiplier(multiplier);
}

FFI_ALWAYS_INLINE void cpp_page_queues_disable_aging(PageQueues* queues) { queues->DisableAging(); }

FFI_ALWAYS_INLINE void cpp_page_queues_enable_aging(PageQueues* queues) { queues->EnableAging(); }

FFI_ALWAYS_INLINE void cpp_page_queues_set_aging_event(PageQueues* queues, Event* event) {
  queues->SetAgingEvent(event);
}

FFI_ALWAYS_INLINE Thread* cpp_page_queues_debug_get_lru_thread(PageQueues* queues) {
  return queues->DebugGetLruThread();
}

FFI_ALWAYS_INLINE Thread* cpp_page_queues_debug_get_mru_thread(PageQueues* queues) {
  return queues->DebugGetMruThread();
}

}  // extern "C"
