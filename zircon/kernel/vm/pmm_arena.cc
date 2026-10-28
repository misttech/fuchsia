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
}

void PmmArena::Init(const PmmArenaSelection& selected, PmmNode* node) {
  rust_pmm_arena_init(this, selected.arena.base, selected.arena.size, selected.bookkeeping.base,
                      selected.bookkeeping.size, node);
}
