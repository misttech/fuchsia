// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/pmm_node_ffi.h"

#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/pmm_node.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE uint32_t cpp_pmm_node_page_to_index(PmmNode* node, const vm_page_t* page) {
  return node->PageToIndex(page);
}

FFI_ALWAYS_INLINE vm_page_t* cpp_pmm_node_index_to_page(PmmNode* node, uint32_t index) {
  return node->IndexToPage(index);
}

FFI_ALWAYS_INLINE zx_paddr_t cpp_pmm_node_index_to_paddr(PmmNode* node, uint32_t index) {
  return node->IndexToPaddr(index);
}

}  // extern "C"
