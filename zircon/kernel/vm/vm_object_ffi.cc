// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_object_ffi.h"

#include <zircon/types.h>

#include <kernel/ffi.h>

#include "vm/page_source.h"
#include "vm/vm_object.h"
#include "vm/vm_object_paged.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE void* cpp_vm_object_get_ref_counted(const VmObject* vmo) {
  return const_cast<fbl::RefCountedUpgradeable<VmObject>*>(
      static_cast<const fbl::RefCountedUpgradeable<VmObject>*>(vmo));
}

FFI_ALWAYS_INLINE void cpp_vm_object_free(VmObject* vmo) { delete vmo; }

FFI_ALWAYS_INLINE uint64_t cpp_vm_object_size(const VmObject* vmo) { return vmo->size(); }

FFI_ALWAYS_INLINE bool cpp_vm_object_is_resizable(const VmObject* vmo) {
  return vmo->is_resizable();
}

FFI_ALWAYS_INLINE bool cpp_vm_object_is_contiguous(const VmObject* vmo) {
  return vmo->is_contiguous();
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_resize(VmObject* vmo, uint64_t size) {
  return vmo->Resize(size);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_write(VmObject* vmo, const void* ptr, uint64_t offset,
                                                  size_t len) {
  return vmo->Write(ptr, offset, len);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_set_name(VmObject* vmo, const char* name, size_t len) {
  return vmo->set_name(name, len);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_decommit_range(VmObject* vmo, uint64_t offset,
                                                           uint64_t len) {
  return vmo->DecommitRange(offset, len);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_commit_range(VmObject* vmo, uint64_t offset,
                                                         uint64_t len) {
  return vmo->CommitRange(offset, len);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_commit_range_pinned(VmObject* vmo, uint64_t offset,
                                                                uint64_t len, bool write) {
  return vmo->CommitRangePinned(offset, len, write);
}

FFI_ALWAYS_INLINE void cpp_vm_object_unpin(VmObject* vmo, uint64_t offset, uint64_t len) {
  vmo->Unpin(offset, len);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_hint_range(VmObject* vmo, uint64_t offset, uint64_t len,
                                                       VmObject::EvictionHint hint) {
  return vmo->HintRange(offset, len, hint);
}

FFI_ALWAYS_INLINE uint8_t cpp_vm_object_get_mapping_cache_policy(const VmObject* vmo) {
  return vmo->GetMappingCachePolicy();
}

FFI_ALWAYS_INLINE VmObject* cpp_vm_object_create_clone(VmObject* vmo, Resizability resizable,
                                                       SnapshotType snapshot_type, uint64_t offset,
                                                       uint64_t size, bool copy_name,
                                                       zx_status_t* out_status) {
  fbl::RefPtr<VmObject> child;
  zx_status_t status = vmo->CreateClone(resizable, snapshot_type, offset, size, copy_name, &child);
  *out_status = status;
  return fbl::ExportToRawPtr(&child);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_get_page_blocking(VmObject* vmo, uint64_t offset,
                                                              uint32_t pf_flags) {
  return vmo->GetPageBlocking(offset, pf_flags, nullptr, nullptr);
}

FFI_ALWAYS_INLINE void cpp_vm_object_set_user_id(VmObject* vmo, uint64_t user_id) {
  vmo->set_user_id(user_id);
}

FFI_ALWAYS_INLINE uint64_t cpp_vm_object_user_id(const VmObject* vmo) { return vmo->user_id(); }

FFI_ALWAYS_INLINE uint64_t cpp_vm_object_parent_user_id(const VmObject* vmo) {
  return vmo->parent_user_id();
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_lookup(VmObject* vmo, uint64_t offset, uint64_t len,
                                                   void* ctx, cpp_vm_object_lookup_fn callback) {
  return vmo->Lookup(offset, len, [ctx, callback](uint64_t offset, paddr_t pa) {
    return callback(ctx, offset, pa);
  });
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_get_page(VmObject* vmo, uint64_t offset,
                                                     uint32_t pf_flags,
                                                     MultiPageRequest* page_request,
                                                     vm_page_t** out_page, paddr_t* out_pa) {
  return vmo->GetPage(offset, pf_flags, page_request, out_page, out_pa);
}

}  // extern "C"
