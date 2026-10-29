// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "vm/vm_object_paged_ffi.h"

#include <zircon/types.h>

#include <new>

#include <kernel/ffi.h>
#include <ktl/memory.h>

#include "vm/vm_object_paged.h"

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {

FFI_ALWAYS_INLINE VmObjectPaged* cpp_vm_object_paged_create(uint32_t pmm_alloc_flags,
                                                            uint32_t options, uint64_t size,
                                                            zx_status_t* out_status) {
  fbl::RefPtr<VmObjectPaged> vmo;
  *out_status = VmObjectPaged::Create(pmm_alloc_flags, options, size, &vmo);
  return fbl::ExportToRawPtr(&vmo);
}

FFI_ALWAYS_INLINE VmObjectPaged* cpp_vm_object_paged_create_contiguous(uint32_t pmm_alloc_flags,
                                                                       uint64_t size,
                                                                       uint8_t alignment_log2,
                                                                       zx_status_t* out_status) {
  fbl::RefPtr<VmObjectPaged> vmo;
  *out_status = VmObjectPaged::CreateContiguous(pmm_alloc_flags, size, alignment_log2, &vmo);
  return fbl::ExportToRawPtr(&vmo);
}

FFI_ALWAYS_INLINE VmObjectPaged* cpp_vm_object_paged_create_external(PageSource* src,
                                                                     uint32_t options,
                                                                     uint64_t size,
                                                                     zx_status_t* out_status) {
  fbl::RefPtr<VmObjectPaged> vmo;
  *out_status = VmObjectPaged::CreateExternal(fbl::ImportFromRawPtr(src), options, size, &vmo);
  return fbl::ExportToRawPtr(&vmo);
}

FFI_ALWAYS_INLINE void cpp_vm_object_paged_reset_pager_vmo_stats(VmObjectPaged* vmo) {
  vmo->ResetPagerVmoStats();
}

FFI_ALWAYS_INLINE VmObjectPaged* cpp_vm_object_paged_create_from_wired_pages(
    const void* data, size_t size, bool exclusive, zx_status_t* out_status) {
  fbl::RefPtr<VmObjectPaged> vmo;
  *out_status = VmObjectPaged::CreateFromWiredPages(data, size, exclusive, &vmo);
  return fbl::ExportToRawPtr(&vmo);
}

FFI_ALWAYS_INLINE VmObject* cpp_vm_object_paged_as_vm_object(VmObjectPaged* vmo) {
  return static_cast<VmObject*>(vmo);
}

FFI_ALWAYS_INLINE uint64_t cpp_vm_object_paged_size_locked(const VmObjectPaged* vmo)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return vmo->size_locked();
}

FFI_ALWAYS_INLINE bool cpp_vm_object_paged_lock_is_held(const VmObjectPaged* vmo) {
  return vmo->lock_ref().lock().IsHeld();
}

FFI_ALWAYS_INLINE VmCowPages* cpp_vm_object_paged_debug_get_cow_pages(VmObjectPaged* vmo) {
  fbl::RefPtr<VmCowPages> cow = vmo->DebugGetCowPages();
  return fbl::ExportToRawPtr(&cow);
}

FFI_ALWAYS_INLINE const VmCowPages* cpp_vm_object_paged_cow_pages(const VmObjectPaged* vmo) {
  return vmo->cow_pages_.get();
}

FFI_ALWAYS_INLINE VmObjectPaged* cpp_vm_object_as_vm_object_paged(VmObject* vmo) {
  return DownCastVmObject<VmObjectPaged>(vmo);
}

FFI_ALWAYS_INLINE vm_page_t* cpp_vm_object_paged_debug_get_page(VmObjectPaged* vmo,
                                                                uint64_t offset) {
  return vmo->DebugGetPage(offset);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_paged_zero_range(const VmObjectPaged* vmo,
                                                             uint64_t offset, uint64_t length) {
  return const_cast<VmObjectPaged*>(vmo)->ZeroRange(offset, length);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_paged_zero_range_untracked(const VmObjectPaged* vmo,
                                                                       uint64_t offset,
                                                                       uint64_t length) {
  return const_cast<VmObjectPaged*>(vmo)->ZeroRangeUntracked(offset, length);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_paged_resize(VmObjectPaged* vmo, uint64_t size) {
  return vmo->Resize(size);
}

FFI_ALWAYS_INLINE void cpp_vm_object_paged_unmap_and_call(VmObjectPaged* vmo, uint64_t offset,
                                                          uint64_t len, void (*call_fn)(void* ctx),
                                                          void* call_ctx) {
  Guard<CriticalMutex> vmo_guard{vmo->lock()};
  vmo->ForwardRangeChangeUpdateLocked(offset, len, VmCowPages::RangeChangeOp::Unmap);
  if (call_fn) {
    call_fn(call_ctx);
  }
}

FFI_ALWAYS_INLINE void cpp_vm_object_paged_set_user_stream_size(VmObjectPaged* vmo,
                                                                StreamSizeManager* ssm) {
  vmo->SetUserStreamSize(fbl::ImportFromRawPtr(ssm));
}

FFI_ALWAYS_INLINE bool cpp_vm_object_paged_user_stream_size_locked(
    VmObjectPaged* vmo, uint64_t* out_stream_size) TA_NO_THREAD_SAFETY_ANALYSIS {
  auto result = vmo->user_stream_size_locked();
  if (result.has_value()) {
    *out_stream_size = result.value();
    return true;
  }
  return false;
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_object_paged_get_lookup_cursor_locked(
    ffi::Uninitialized<VmCowPagesLookupCursorResult>* out, VmObjectPaged* vmo, uint64_t offset,
    uint64_t max_len) TA_NO_THREAD_SAFETY_ANALYSIS {
  // Not out->Initialize(), which would move the result from a temporary on the stack.
  auto* cursor = new (out->GetAddressUnchecked())
      VmCowPagesLookupCursorResult{vmo->GetLookupCursorLocked(offset, max_len)};
  zx_status_t status = cursor->result.status_value();
  if (unlikely(status != ZX_OK)) {
    ktl::destroy_at(cursor);
  }
  return status;
}

FFI_ALWAYS_INLINE void cpp_vm_object_paged_mark_modified_locked(VmObjectPaged* vmo)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  vmo->mark_modified_locked();
}

}  // extern "C"
