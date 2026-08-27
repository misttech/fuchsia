// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_PAGE_LIST_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_PAGE_LIST_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/vm_page_list.h>

__BEGIN_CDECLS

void cpp_vm_page_splice_list_construct(VmPageSpliceList* list);
void cpp_vm_page_splice_list_destroy(VmPageSpliceList* list);
bool cpp_vm_page_splice_list_is_processed(const VmPageSpliceList* list);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_PAGE_LIST_FFI_H_
