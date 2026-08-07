// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/page.h"

__BEGIN_CDECLS

uint64_t cpp_get_count(vm_page_state state);
void cpp_add_to_initial_count(vm_page_state state, uint64_t n);
bool cpp_vm_page_is_loaned(vm_page_t* page);
bool cpp_vm_page_is_loan_cancelled(vm_page_t* page);
void cpp_vm_page_set_is_loaned(vm_page_t* page);
void cpp_vm_page_clear_is_loaned(vm_page_t* page);
void cpp_vm_page_set_is_loan_cancelled(vm_page_t* page);
void cpp_vm_page_clear_is_loan_cancelled(vm_page_t* page);
void cpp_vm_page_dump(vm_page_t* page);
paddr_t cpp_vm_page_paddr(vm_page_t* page);
vm_page_state cpp_vm_page_state(vm_page_t* page);
void cpp_vm_page_set_state(vm_page_t* page, vm_page_state new_state);
void* cpp_vm_page_object_get_object(const vm_page_t* page);
uint64_t cpp_vm_page_object_get_page_offset(const vm_page_t* page);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PAGE_FFI_H_
