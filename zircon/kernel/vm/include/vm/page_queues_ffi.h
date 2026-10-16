// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <kernel/thread.h>

#include "vm/debug_compressor.h"
#include "vm/page_queues.h"

// Version of PageQueues::VmoBacklink that uses a raw pointer instead of a refptr for FFI.
struct PageQueuesVmoBacklink {
  VmCowPages* cow;
  vm_page_t* page;
  uint64_t offset;
};

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
__BEGIN_CDECLS

FFI_ALWAYS_INLINE void cpp_page_queues_init(ffi::Uninitialized<PageQueues>* queues);

FFI_ALWAYS_INLINE void* cpp_page_queues_get_lock(PageQueues* queues);

FFI_ALWAYS_INLINE const char* cpp_page_queues_string_from_age_reason(int32_t reason);

FFI_ALWAYS_INLINE bool cpp_page_queues_pop_anonymous_zero_fork(PageQueues* queues,
                                                               PageQueuesVmoBacklink* out_backlink);
FFI_ALWAYS_INLINE bool cpp_page_queues_peek_isolate(PageQueues* queues, size_t lowest_queue,
                                                    PageQueuesVmoBacklink* out_backlink);
FFI_ALWAYS_INLINE bool cpp_page_queues_get_cow_for_loaned_page(PageQueues* queues, vm_page_t* page,
                                                               PageQueuesVmoBacklink* out_backlink);

FFI_ALWAYS_INLINE void cpp_page_queues_dump(const PageQueues* queues);

FFI_ALWAYS_INLINE void cpp_page_queues_start_debug_compressor(PageQueues* queues);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_QUEUES_FFI_H_
