// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_FFI_H_

#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/vm_cow_pages.h"

__BEGIN_CDECLS

zx_status_t cpp_vm_cow_pages_replace_page_with_loaned(VmCowPages* cow, vm_page_t* before_page,
                                                      uint64_t offset);
void* cpp_vm_cow_pages_get_ref_counted(const VmCowPages* cow);
void cpp_vm_cow_pages_free(VmCowPages* cow);
void cpp_vm_cow_pages_initialize_page_cache(uint32_t level);
PmmOptDelayReuse cpp_vm_cow_pages_should_delay_reuse_on_free(const VmCowPages* cow);
VmCowPages* cpp_vm_cow_pages_debug_get_parent(VmCowPages* cow);
bool cpp_vm_cow_pages_dedup_zero_page(VmCowPages* cow, vm_page_t* page, uint64_t offset);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_COW_PAGES_FFI_H_
