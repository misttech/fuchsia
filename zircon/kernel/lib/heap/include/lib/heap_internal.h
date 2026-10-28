// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_INTERNAL_H_
#define ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_INTERNAL_H_

#include <stddef.h>
#include <zircon/compiler.h>

__BEGIN_CDECLS

// Internal APIs used by the heap implementation to get/return pages to the VM.
// `heap_page_alloc` and `heap_page_free` must be called with `TheHeapLock` held.
void* heap_page_alloc(size_t pages);
void heap_page_free(void* ptr, size_t pages);

// Reports an allocation failure to the PMM; must be called without `TheHeapLock` held.
void heap_report_alloc_failure(size_t size);

__END_CDECLS

#endif  // ZIRCON_KERNEL_LIB_HEAP_INCLUDE_LIB_HEAP_INTERNAL_H_
