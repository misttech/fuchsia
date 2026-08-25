// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include "vm/pmm_node.h"

__BEGIN_CDECLS

uint32_t cpp_pmm_node_page_to_index(PmmNode* node, const vm_page_t* page);
vm_page_t* cpp_pmm_node_index_to_page(PmmNode* node, uint32_t index);
zx_paddr_t cpp_pmm_node_index_to_paddr(PmmNode* node, uint32_t index);
void cpp_pmm_node_add_free_pages(PmmNode* node, VmPageDoublyLinkedList* list);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_
