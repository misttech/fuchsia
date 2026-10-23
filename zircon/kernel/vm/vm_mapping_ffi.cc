// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/types.h>

#include <kernel/ffi.h>
#include <ktl/tuple.h>
#include <ktl/type_traits.h>
#include <ktl/utility.h>
#include <vm/vm_address_region.h>
#include <vm/vm_address_region_ffi.h>
#include <vm/vm_object.h>

static_assert(ktl::is_trivially_copy_constructible_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_move_constructible_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_copy_assignable_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_move_assignable_v<VmMappingProtectionRangesCursor>);
static_assert(ktl::is_trivially_destructible_v<VmMappingProtectionRangesCursor>);
static_assert(sizeof(ffi::Uninitialized<VmMappingProtectionRangesCursor>) ==
              sizeof(VmMappingProtectionRangesCursor));
static_assert(alignof(ffi::Uninitialized<VmMappingProtectionRangesCursor>) ==
              alignof(VmMappingProtectionRangesCursor));

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {
FFI_ALWAYS_INLINE void cpp_vm_mapping_construct(ffi::Uninitialized<VmMapping>* slot,
                                                VmAddressRegion* parent, bool private_clone,
                                                vaddr_t base, size_t size, uint32_t vmar_flags,
                                                VmObject* vmo, uint64_t vmo_offset,
                                                arch_mmu_flags_t arch_mmu_flags, bool mergeable) {
  slot->Initialize(*parent, private_clone, base, size, vmar_flags, fbl::ImportFromRawPtr(vmo),
                   vmo_offset, arch_mmu_flags, static_cast<VmMapping::Mergeable>(mergeable));
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_construct_with_ranges(
    ffi::Uninitialized<VmMapping>* slot, VmAddressRegion* parent, bool private_clone, vaddr_t base,
    size_t size, uint32_t vmar_flags, VmObject* vmo, uint64_t vmo_offset,
    arch_mmu_flags_t first_mmu_flags, VmMappingProtectionRanges* ranges, bool mergeable) {
  slot->Initialize(*parent, private_clone, base, size, vmar_flags, fbl::ImportFromRawPtr(vmo),
                   vmo_offset, first_mmu_flags, ktl::move(*ranges),
                   static_cast<VmMapping::Mergeable>(mergeable));
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_destruct(VmMapping* mapping) { mapping->~VmMapping(); }
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
FFI_ALWAYS_INLINE VmMappingProtectionRanges* cpp_vm_mapping_rest_protection_ranges(
    VmMapping* mapping) TA_NO_THREAD_SAFETY_ANALYSIS {
  return &mapping->rest_protection_ranges_;
}
FFI_ALWAYS_INLINE const VmMappingProtectionRanges* cpp_vm_mapping_rest_protection_ranges_const(
    const VmMapping* mapping) TA_NO_THREAD_SAFETY_ANALYSIS {
  return &mapping->rest_protection_ranges_;
}
FFI_ALWAYS_INLINE bool cpp_vm_mapping_protection_ranges_is_empty(
    const VmMappingProtectionRanges* ranges) {
  return ranges->is_empty();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_clear(VmMappingProtectionRanges* ranges) {
  ranges->clear();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_upper_bound(
    const VmMappingProtectionRanges* ranges, vaddr_t key,
    ffi::Uninitialized<VmMappingProtectionRangesCursor>* out_cursor) {
  out_cursor->Initialize(ranges->upper_bound(key));
}
FFI_ALWAYS_INLINE bool cpp_vm_mapping_protection_ranges_cursor_is_valid(
    const VmMappingProtectionRangesCursor* cursor) {
  return cursor->IsValid();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_cursor_get(
    const VmMappingProtectionRangesCursor* cursor, vaddr_t* out_key, arch_mmu_flags_t* out_flags) {
  ktl::tie(*out_key, *out_flags) = **cursor;
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_cursor_next(
    VmMappingProtectionRangesCursor* cursor) {
  ++*cursor;
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_protection_ranges_cursor_prev(
    VmMappingProtectionRangesCursor* cursor) {
  --*cursor;
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
FFI_ALWAYS_INLINE zx_status_t cpp_vm_mapping_page_fault_locked(
    VmMapping* mapping, vaddr_t va, uint32_t pf_flags, size_t additional_pages,
    MultiPageRequest* page_request, uint32_t* out_mapped) TA_NO_THREAD_SAFETY_ANALYSIS {
  auto [status, mapped] = mapping->PageFaultLocked(va, pf_flags, additional_pages, page_request);
  *out_mapped = mapped;
  return status;
}
}  // extern "C"
