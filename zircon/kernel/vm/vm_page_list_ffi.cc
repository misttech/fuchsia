// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_page_list_ffi.h"

#include <lib/btree.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/utility.h>
#include <vm/vm_page_list.h>

static_assert(sizeof(VmPageListNode) == 64);
static_assert(alignof(VmPageListNode) == 4);

namespace {

template <typename NodePtr, typename Iterator>
inline NodePtr unwrap_entry(const Iterator& iter, uint64_t* out_offset) {
  if (!iter.IsValid()) {
    *out_offset = 0;
    return nullptr;
  }
  auto [offset, node] = *iter;
  *out_offset = offset;
  return node;
}

}  // namespace

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_init(ffi::Uninitialized<VmPageListBtree>* tree) {
  tree->Initialize();
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_destroy(VmPageListBtree* tree) {
  ktl::destroy_at(tree);
}

FFI_ALWAYS_INLINE bool cpp_vm_page_list_btree_is_empty(const VmPageListBtree* tree) {
  return tree->is_empty();
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_clear(VmPageListBtree* tree) { tree->clear(); }

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_find(VmPageListBtree* tree, uint64_t node_offset,
                                                    VmPageListBtreeCursor* out_cursor) {
  auto pln = tree->find(node_offset);
  if (!pln.IsValid()) {
    return nullptr;
  }
  if (out_cursor) {
    out_cursor->iter = pln;
  }
  return (*pln).second;
}

FFI_ALWAYS_INLINE const void* cpp_vm_page_list_btree_find_const(const VmPageListBtree* tree,
                                                                uint64_t node_offset) {
  auto pln = tree->find(node_offset);
  if (!pln.IsValid()) {
    return nullptr;
  }
  return (*pln).second;
}

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_lower_bound(VmPageListBtree* tree,
                                                           uint64_t node_offset,
                                                           VmPageListBtreeCursor* out_cursor,
                                                           uint64_t* out_offset) {
  auto iter = tree->lower_bound(node_offset);
  if (out_cursor) {
    out_cursor->iter = iter;
  }
  return unwrap_entry<void*>(iter, out_offset);
}

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_cursor_get(const VmPageListBtreeCursor* cursor,
                                                          uint64_t* out_offset) {
  return unwrap_entry<void*>(cursor->iter, out_offset);
}

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_insert(VmPageListBtree* tree, uint64_t node_offset,
                                                      VmPageListBtreeCursor* cursor) {
  VmPlnOwner pl = VmPageListNode::Create();
  if (!pl) {
    return nullptr;
  }
  VmPageListNode* raw_ptr = pl.get();
  auto hint = cursor ? cursor->iter : VmPageListBtree::iterator{};
  auto iter = tree->insert(hint, node_offset, ktl::move(pl));
  if (!iter.IsValid()) {
    return nullptr;
  }
  if (cursor) {
    cursor->iter = iter;
  }
  return raw_ptr;
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_erase_at(VmPageListBtree* tree,
                                                       VmPageListBtreeCursor* cursor) {
  DEBUG_ASSERT(cursor->iter.IsValid());
  cursor->iter = tree->erase(cursor->iter);
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_cursor_default_init(VmPageListBtreeCursor* cursor) {
  cursor->iter = {};
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_cursor_init(VmPageListBtreeCursor* cursor,
                                                          VmPageListBtree* tree) {
  cursor->iter = tree->begin();
}

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_cursor_next(VmPageListBtreeCursor* cursor,
                                                           uint64_t* out_offset) {
  void* node = unwrap_entry<void*>(cursor->iter, out_offset);
  if (node) {
    ++cursor->iter;
  }
  return node;
}

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_cursor_prev(VmPageListBtreeCursor* cursor,
                                                           uint64_t* out_offset) {
  --cursor->iter;
  return unwrap_entry<void*>(cursor->iter, out_offset);
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_const_cursor_init(VmPageListBtreeConstCursor* cursor,
                                                                const VmPageListBtree* tree) {
  cursor->iter = tree->begin();
}

FFI_ALWAYS_INLINE const void* cpp_vm_page_list_btree_const_cursor_next(
    VmPageListBtreeConstCursor* cursor, uint64_t* out_offset) {
  const void* node = unwrap_entry<const void*>(cursor->iter, out_offset);
  if (node) {
    ++cursor->iter;
  }
  return node;
}

FFI_ALWAYS_INLINE VmPageListBtreeUtilization
cpp_vm_page_list_btree_calculate_utilization_slow(const VmPageListBtree* tree) {
  auto utilization = tree->calculate_utilization_slow();
  return {
      .nodes_in_bytes = utilization.nodes_in_bytes(),
      .stored_values = utilization.stored_values,
  };
}

}  // extern "C"
