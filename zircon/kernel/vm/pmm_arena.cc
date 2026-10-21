// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include "vm/pmm_arena.h"

#include <lib/page/size.h>
#include <zircon/types.h>

#include <fbl/intrusive_double_list.h>
#include <vm/page.h>
#include <vm/pmm_node.h>

extern "C" {
void rust_pmm_arena_init(PmmArena* arena, uint64_t selected_arena_base,
                         uint64_t selected_arena_size, uint64_t selected_bookkeeping_base,
                         uint64_t selected_bookkeeping_size, PmmNode* node);
void rust_pmm_arena_init_for_test(PmmArena* arena, const pmm_arena_info_t* info,
                                  vm_page_t* page_array);
vm_page_t* rust_pmm_arena_find_free_contiguous(PmmArena* arena, size_t count,
                                               uint8_t alignment_log2);
}

void PmmArena::Init(const PmmArenaSelection& selected, PmmNode* node) {
  rust_pmm_arena_init(this, selected.arena.base, selected.arena.size, selected.bookkeeping.base,
                      selected.bookkeeping.size, node);
}

void PmmArena::InitForTest(const pmm_arena_info_t& info, vm_page_t* page_array) {
  rust_pmm_arena_init_for_test(this, &info, page_array);
}

vm_page_t* PmmArena::FindFreeContiguous(size_t count, uint8_t alignment_log2) {
  return rust_pmm_arena_find_free_contiguous(this, count, alignment_log2);
}
