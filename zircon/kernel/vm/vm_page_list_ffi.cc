// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_page_list_ffi.h"

#include <kernel/ffi.h>
#include <ktl/memory.h>

#include "vm/vm_page_list.h"

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

}  // extern "C"
