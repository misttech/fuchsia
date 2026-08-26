// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/page_queues.h"

__BEGIN_CDECLS

bool cpp_page_queues_debug_page_is_wired(const PageQueues* queues, const vm_page_t* page);
bool cpp_page_queues_debug_page_is_any_anonymous(const PageQueues* queues, const vm_page_t* page);
bool cpp_page_queues_debug_page_is_reclaim(const PageQueues* queues, const vm_page_t* page,
                                           size_t* out_queue);
bool cpp_page_queues_debug_page_is_pager_backed_dirty(const PageQueues* queues,
                                                      const vm_page_t* page);
bool cpp_page_queues_debug_page_is_reclaim_isolate(const PageQueues* queues, const vm_page_t* page);
void cpp_page_queues_rotate_reclaim_queues(PageQueues* queues);
void cpp_page_queues_mark_accessed(PageQueues* queues, vm_page_t* page);
bool cpp_page_queues_is_page_reclaimable(const vm_page_t* page);
void cpp_page_queues_move_to_reclaim_dont_need(PageQueues* queues, vm_page_t* page);
void cpp_page_queues_queue_counts(const PageQueues* queues, PageQueues::Counts* out_counts);
bool cpp_page_queues_reclaim_is_only_pager_backed(const PageQueues* queues);
bool cpp_page_queues_debug_page_is_anonymous(const PageQueues* queues, const vm_page_t* page);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_FFI_H_
