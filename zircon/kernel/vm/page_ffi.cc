// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/page_ffi.h"

#include <assert.h>
#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/page.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE uint64_t cpp_get_count(vm_page_state state) {
  return vm_page_t::get_count(state);
}

FFI_ALWAYS_INLINE void cpp_add_to_initial_count(vm_page_state state, uint64_t n) {
  vm_page_t::add_to_initial_count(state, n);
}

FFI_ALWAYS_INLINE void cpp_vm_page_dump(vm_page_t* page) { page->dump(); }

}  // extern "C"
