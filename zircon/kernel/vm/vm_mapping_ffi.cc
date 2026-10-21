// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/types.h>

#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <vm/vm_address_region.h>
#include <vm/vm_address_region_ffi.h>
#include <vm/vm_object.h>

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {
FFI_ALWAYS_INLINE void cpp_vm_mapping_free(VmMapping* mapping) { delete mapping; }
FFI_ALWAYS_INLINE void* cpp_vm_mapping_state(VmMapping* mapping) {
  return &mapping->opaque_storage_;
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_mapping_map_range(VmMapping* mapping, size_t offset,
                                                       size_t len, bool commit,
                                                       bool ignore_existing) {
  return mapping->MapRange(offset, len, commit, ignore_existing);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_mapping_debug_unmap(VmMapping* mapping, vaddr_t base,
                                                         size_t size) {
  return mapping->DebugUnmap(base, size);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_mapping_debug_protect(VmMapping* mapping, vaddr_t base,
                                                           size_t size,
                                                           arch_mmu_flags_t new_arch_mmu_flags) {
  return mapping->DebugProtect(base, size, new_arch_mmu_flags);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_mapping_enumerate_protection_ranges_locked(
    const VmMapping* mapping, vaddr_t base, size_t size,
    cpp_vm_mapping_enumerate_protection_ranges_cb cb, void* ctx) TA_NO_THREAD_SAFETY_ANALYSIS {
  return mapping->EnumerateProtectionRangesLocked(
      base, size, [cb, ctx](vaddr_t base, size_t len, arch_mmu_flags_t flags) {
        return cb(ctx, base, len, flags);
      });
}
FFI_ALWAYS_INLINE VmMapping::FlagsRange cpp_vm_mapping_arch_mmu_flags_range_locked(
    const VmMapping* mapping, vaddr_t offset) {
  AssertHeld(mapping->lock_ref());
  return mapping->arch_mmu_flags_range_locked(offset);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_mapping_force_writable(VmMapping* mapping,
                                                            VmMapping** out_mapping) {
  auto result = mapping->ForceWritable();
  if (result.is_error()) {
    return result.error_value();
  }
  auto res = *result;
  *out_mapping = fbl::ExportToRawPtr(&res);
  return ZX_OK;
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_mark_mergeable(VmMapping* mapping) {
  VmMapping::MarkMergeable(fbl::ImportFromRawPtr(mapping));
}
FFI_ALWAYS_INLINE arch_mmu_flags_t cpp_vm_mapping_arch_mmu_flags_locked(
    const VmMapping* mapping, vaddr_t offset) TA_NO_THREAD_SAFETY_ANALYSIS {
  return mapping->arch_mmu_flags_locked(offset);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_mapping_page_fault_locked(
    VmMapping* mapping, vaddr_t va, uint32_t pf_flags, size_t additional_pages,
    MultiPageRequest* page_request, uint32_t* out_mapped) TA_NO_THREAD_SAFETY_ANALYSIS {
  auto [status, mapped] = mapping->PageFaultLocked(va, pf_flags, additional_pages, page_request);
  *out_mapped = mapped;
  return status;
}
}  // extern "C"
