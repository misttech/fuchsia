// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/pmm_node.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
__BEGIN_CDECLS

FFI_ALWAYS_INLINE void cpp_pmm_node_destroy(PmmNode* node);
FFI_ALWAYS_INLINE void cpp_pmm_node_add_free_pages(PmmNode* node, VmPageDoublyLinkedList* list);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_PMM_NODE_FFI_H_
