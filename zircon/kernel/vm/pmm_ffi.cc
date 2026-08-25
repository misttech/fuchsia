// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm_ffi.h"

#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/pmm.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE vm_page_t* cpp_paddr_to_vm_page(zx_paddr_t paddr) {
  return paddr_to_vm_page(paddr);
}

FFI_ALWAYS_INLINE PageQueues* cpp_pmm_page_queues() { return pmm_page_queues(); }

FFI_ALWAYS_INLINE zx_status_t cpp_pmm_alloc_page(uint32_t flags, vm_page_t** out_page,
                                                 zx_paddr_t* out_paddr) {
  vm_page_t* page = nullptr;
  paddr_t paddr = 0;
  zx_status_t status = pmm_alloc_page(flags, &page, &paddr);
  *out_page = page;
  *out_paddr = paddr;
  return status;
}

FFI_ALWAYS_INLINE void cpp_pmm_free_page(vm_page_t* page) { pmm_free_page(page); }

}  // extern "C"
