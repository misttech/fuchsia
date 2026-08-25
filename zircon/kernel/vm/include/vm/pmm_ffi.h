// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/pmm.h"

__BEGIN_CDECLS

vm_page_t* cpp_paddr_to_vm_page(zx_paddr_t paddr);
PageQueues* cpp_pmm_page_queues();
zx_status_t cpp_pmm_alloc_page(uint32_t flags, vm_page_t** out_page, zx_paddr_t* out_paddr);
void cpp_pmm_free_page(vm_page_t* page);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_FFI_H_
