// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_PAGE_LIST_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_PAGE_LIST_FFI_H_

#include <lib/btree.h>
#include <stdbool.h>
#include <stdint.h>
#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/vm_page_list.h>

// Underlying BTree representation for VmPageList, mapping node offsets to owned VmPageListNodes.
using VmPageListBtree = btree::BTree<uint64_t, VmPlnOwner>;

static_assert(sizeof(VmPageListBtree) == sizeof(VmPageList));

// Cursor types for iterating over B-Tree nodes in ascending order.
struct VmPageListBtreeCursor {
  VmPageListBtree::iterator iter;
};
struct VmPageListBtreeConstCursor {
  VmPageListBtree::const_iterator iter;
};

__BEGIN_CDECLS

void cpp_vm_page_splice_list_construct(VmPageSpliceList* list);
void cpp_vm_page_splice_list_destroy(VmPageSpliceList* list);
bool cpp_vm_page_splice_list_is_processed(const VmPageSpliceList* list);

// Constructs the VmPageListBtree container in-place at `tree`.
void cpp_vm_page_list_btree_init(ffi::Uninitialized<VmPageListBtree>* tree);

// Destroys the VmPageListBtree container.
void cpp_vm_page_list_btree_destroy(VmPageListBtree* tree);

// Returns true if the B-Tree is empty.
bool cpp_vm_page_list_btree_is_empty(const VmPageListBtree* tree);

// Clears the B-Tree of all nodes.
void cpp_vm_page_list_btree_clear(VmPageListBtree* tree);

// Finds the VmPageListNode at node_offset. Returns nullptr if not found.
//
// If `out_cursor` is non-null, it is initialized with the B-Tree iterator pointing to the found
// node. This allows callers (e.g., during slot removal) to subsequently erase the node in O(1)
// amortized time using `cpp_vm_page_list_btree_erase_at` without performing a redundant tree
// search. Callers that only inspect or mutate the node without erasing may pass nullptr.
void* cpp_vm_page_list_btree_find(VmPageListBtree* tree, uint64_t node_offset,
                                  VmPageListBtreeCursor* out_cursor);
const void* cpp_vm_page_list_btree_find_const(const VmPageListBtree* tree, uint64_t node_offset);

// Finds or allocates and inserts a VmPageListNode at node_offset using lower_bound hint.
// Returns the node pointer, or nullptr if allocation failed.
void* cpp_vm_page_list_btree_find_or_allocate(VmPageListBtree* tree, uint64_t node_offset);

// Erases the node at the cursor position in O(1) amortized time without re-searching the tree.
// The cursor must have been initialized by a prior call (e.g., `cpp_vm_page_list_btree_find`).
void cpp_vm_page_list_btree_erase_at(VmPageListBtree* tree, VmPageListBtreeCursor* cursor);

// Cursors for iterating over nodes in the B-Tree in ascending key order.
void cpp_vm_page_list_btree_cursor_init(VmPageListBtreeCursor* cursor, VmPageListBtree* tree);
void* cpp_vm_page_list_btree_cursor_next(VmPageListBtreeCursor* cursor, uint64_t* out_offset);

void cpp_vm_page_list_btree_const_cursor_init(VmPageListBtreeConstCursor* cursor,
                                              const VmPageListBtree* tree);
const void* cpp_vm_page_list_btree_const_cursor_next(VmPageListBtreeConstCursor* cursor,
                                                     uint64_t* out_offset);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_PAGE_LIST_FFI_H_
