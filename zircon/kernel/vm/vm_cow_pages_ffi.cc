// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_cow_pages_ffi.h"

#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/vm_cow_pages.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE zx_status_t cpp_vm_cow_pages_replace_page_with_loaned(VmCowPages* cow,
                                                                        vm_page_t* before_page,
                                                                        uint64_t offset) {
  return cow->ReplacePageWithLoaned(before_page, offset);
}

FFI_ALWAYS_INLINE void* cpp_vm_cow_pages_get_ref_counted(const VmCowPages* cow) {
  return const_cast<fbl::RefCountedUpgradeable<VmCowPages>*>(
      static_cast<const fbl::RefCountedUpgradeable<VmCowPages>*>(cow));
}

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_free(VmCowPages* cow) { delete cow; }

FFI_ALWAYS_INLINE void cpp_vm_cow_pages_initialize_page_cache(uint32_t level) {
  VmCowPages::InitializePageCache(level);
}

FFI_ALWAYS_INLINE PmmOptDelayReuse
cpp_vm_cow_pages_should_delay_reuse_on_free(const VmCowPages* cow) {
  return cow->should_delay_reuse_on_free();
}

FFI_ALWAYS_INLINE VmCowPages* cpp_vm_cow_pages_debug_get_parent(VmCowPages* cow) {
  auto parent = cow->DebugGetParent();
  return fbl::ExportToRawPtr(&parent);
}

FFI_ALWAYS_INLINE bool cpp_vm_cow_pages_dedup_zero_page(VmCowPages* cow, vm_page_t* page,
                                                        uint64_t offset) {
  return cow->DedupZeroPage(page, offset);
}

}  // extern "C"
