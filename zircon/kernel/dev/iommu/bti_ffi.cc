// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/types.h>

#include <dev/iommu/bti.h>
#include <kernel/ffi.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_bti_recycle(iommu::Bti* bti) { delete bti; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_bti_release_quarantine(iommu::Bti* bti) { bti->ReleaseQuarantine(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_bti_on_dispatcher_zero_handles(iommu::Bti* bti) {
  bti->OnDispatcherZeroHandles();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_minimum_contiguity(const iommu::Bti* bti) {
  return bti->minimum_contiguity();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_aspace_size(const iommu::Bti* bti) { return bti->aspace_size(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_pmo_count(const iommu::Bti* bti) { return bti->pmo_count(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_quarantine_count(const iommu::Bti* bti) {
  return bti->quarantine_count();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_bti_in_fault_state(const iommu::Bti* bti) {
  return bti->in_fault_state();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_bti_bti_id(const iommu::Bti* bti) { return bti->bti_id(); }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_bti_set_name(iommu::Bti* bti, const char* name, size_t len) {
  return bti->set_name(name, len);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_bti_get_name(const iommu::Bti* bti,
                                               char out_name[ZX_MAX_NAME_LEN]) {
  return bti->get_name(*reinterpret_cast<char (*)[ZX_MAX_NAME_LEN]>(out_name));
}

}  // extern "C"
