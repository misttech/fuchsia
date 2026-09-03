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

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void cpp_vm_page_splice_list_construct(VmPageSpliceList* list) {
  ktl::construct_at(list);
}

FFI_ALWAYS_INLINE void cpp_vm_page_splice_list_destroy(VmPageSpliceList* list) {
  ktl::destroy_at(list);
}

FFI_ALWAYS_INLINE bool cpp_vm_page_splice_list_is_processed(const VmPageSpliceList* list) {
  return list->IsProcessed();
}

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

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_find_or_allocate(VmPageListBtree* tree,
                                                                uint64_t node_offset) {
  // lookup the tree node that holds this page. Use lower_bound instead of find to optimize
  // later insertion in case of failed lookup.
  auto pln = tree->lower_bound(node_offset);
  if (pln.IsValid()) {
    auto [found_offset, node] = *pln;
    if (found_offset == node_offset) {
      return node;
    }
  }

  VmPlnOwner pl = VmPageListNode::Create();
  if (!pl) {
    return nullptr;
  }
  VmPageListNode* raw_ptr = pl.get();
  auto iter = tree->insert(pln, node_offset, ktl::move(pl));
  if (!iter.IsValid()) {
    return nullptr;
  }
  return raw_ptr;
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_erase_at(VmPageListBtree* tree,
                                                       VmPageListBtreeCursor* cursor) {
  DEBUG_ASSERT(cursor->iter.IsValid());
  tree->erase(cursor->iter);
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_cursor_init(VmPageListBtreeCursor* cursor,
                                                          VmPageListBtree* tree) {
  cursor->iter = tree->begin();
}

FFI_ALWAYS_INLINE void* cpp_vm_page_list_btree_cursor_next(VmPageListBtreeCursor* cursor,
                                                           uint64_t* out_offset) {
  if (!cursor->iter.IsValid()) {
    return nullptr;
  }
  auto [offset, node] = *cursor->iter;
  if (out_offset) {
    *out_offset = offset;
  }
  ++cursor->iter;
  return node;
}

FFI_ALWAYS_INLINE void cpp_vm_page_list_btree_const_cursor_init(VmPageListBtreeConstCursor* cursor,
                                                                const VmPageListBtree* tree) {
  cursor->iter = tree->begin();
}

FFI_ALWAYS_INLINE const void* cpp_vm_page_list_btree_const_cursor_next(
    VmPageListBtreeConstCursor* cursor, uint64_t* out_offset) {
  if (!cursor->iter.IsValid()) {
    return nullptr;
  }
  auto [offset, node] = *cursor->iter;
  if (out_offset) {
    *out_offset = offset;
  }
  ++cursor->iter;
  return node;
}

}  // extern "C"
