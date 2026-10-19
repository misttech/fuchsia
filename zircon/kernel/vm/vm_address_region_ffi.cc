// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/types.h>

#include <kernel/ffi.h>
#include <ktl/memory.h>
#include <ktl/utility.h>
#include <vm/vm_address_region.h>
#include <vm/vm_address_region_ffi.h>
#include <vm/vm_object.h>

CppVmAddressRegionEnumerator::CppVmAddressRegionEnumerator(VmAddressRegionEnumeratorType type,
                                                           VmAddressRegion& vmar, vaddr_t min_addr,
                                                           vaddr_t max_addr)
    : type(type) {
  switch (type) {
    case VmAddressRegionEnumeratorType::VmarsAndMappings:
      new (&vmars_and_mappings) VmarsAndMappings(vmar, min_addr, max_addr);
      break;
    case VmAddressRegionEnumeratorType::MappingsOnly:
      new (&mappings_only) MappingsOnly(vmar, min_addr, max_addr);
      break;
  }
}

CppVmAddressRegionEnumerator::~CppVmAddressRegionEnumerator() {
  switch (type) {
    case VmAddressRegionEnumeratorType::VmarsAndMappings:
      vmars_and_mappings.~VmarsAndMappings();
      break;
    case VmAddressRegionEnumeratorType::MappingsOnly:
      mappings_only.~MappingsOnly();
      break;
  }
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" {
FFI_ALWAYS_INLINE void cpp_vm_address_region_free(VmAddressRegion* vmar) { delete vmar; }
FFI_ALWAYS_INLINE void* cpp_vm_address_region_state(VmAddressRegion* vmar) {
  return &vmar->opaque_storage_;
}
FFI_ALWAYS_INLINE void* cpp_vm_address_region_or_mapping_state(
    VmAddressRegionOrMapping* region_or_mapping) {
  return &region_or_mapping->opaque_storage_;
}
FFI_ALWAYS_INLINE bool cpp_vm_address_region_or_mapping_lock_is_held(
    const VmAddressRegionOrMapping* region_or_mapping) {
  return region_or_mapping->lock_ref().lock().IsHeld();
}
FFI_ALWAYS_INLINE bool cpp_vm_address_region_or_mapping_region_lock_is_held(
    const VmAddressRegionOrMapping* region_or_mapping) {
  return region_or_mapping->region_lock_ref().lock().IsHeld();
}
FFI_ALWAYS_INLINE VmAddressRegion* cpp_vm_address_region_or_mapping_as_vm_address_region(
    VmAddressRegionOrMapping* region_or_mapping) {
  return static_cast<VmAddressRegion*>(region_or_mapping);
}
FFI_ALWAYS_INLINE VmMapping* cpp_vm_address_region_or_mapping_as_vm_mapping(
    VmAddressRegionOrMapping* region_or_mapping) {
  return static_cast<VmMapping*>(region_or_mapping);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_destroy(VmAddressRegion* vmar) {
  return vmar->Destroy();
}
FFI_ALWAYS_INLINE VmAddressRegionOrMapping* cpp_vm_address_region_subregions_find_region(
    const VmAddressRegion* vmar, vaddr_t addr) TA_NO_THREAD_SAFETY_ANALYSIS {
  // The subregions are const as with only holding lock_ we are not permitted to modify the
  // subregions_ tree, but we are allowed to manipulate the objects in the tree, hence can remove
  // const.
  return const_cast<VmAddressRegionOrMapping*>(vmar->subregions_.FindRegion(addr));
}
FFI_ALWAYS_INLINE VmAddressRegionOrMapping* cpp_vm_address_region_subregions_include_or_higher(
    const VmAddressRegion* vmar, vaddr_t base) TA_NO_THREAD_SAFETY_ANALYSIS {
  auto it = vmar->subregions_.IncludeOrHigher(base);
  if (it == vmar->subregions_.end()) {
    return nullptr;
  }
  return const_cast<VmAddressRegionOrMapping*>((*it).second);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_subregions_insert_region(
    VmAddressRegion* vmar, VmAddressRegionOrMapping* region) TA_NO_THREAD_SAFETY_ANALYSIS {
  return vmar->subregions_.InsertRegion(fbl::RefPtr(region));
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_remove_region(
    VmAddressRegion* vmar, VmAddressRegionOrMapping* region) TA_NO_THREAD_SAFETY_ANALYSIS {
  vmar->subregions_.RemoveRegion(region);
}
FFI_ALWAYS_INLINE bool cpp_vm_mapping_rest_protection_ranges_is_empty(const VmMapping* mapping)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return mapping->rest_protection_ranges_.is_empty();
}
FFI_ALWAYS_INLINE void cpp_vm_mapping_rest_protection_ranges_clear(VmMapping* mapping)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  mapping->rest_protection_ranges_.clear();
}
FFI_ALWAYS_INLINE size_t cpp_vm_address_region_subregions_size_slow(const VmAddressRegion* vmar)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return vmar->subregions_.size_slow();
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_subregions_for_each(
    const VmAddressRegion* vmar, void (*cb)(VmAddressRegionOrMapping*, void*),
    void* ctx) TA_NO_THREAD_SAFETY_ANALYSIS {
  for (const auto& child : vmar->subregions_) {
    cb(const_cast<VmAddressRegionOrMapping*>(child.second), ctx);
  }
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_set_memory_priority(
    VmAddressRegion* vmar, VmAddressRegion::MemoryPriority priority) {
  return vmar->SetMemoryPriority(priority);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_unmap(VmAddressRegion* vmar, vaddr_t base,
                                                          size_t size,
                                                          VmAddressRegionOpChildren op_children) {
  return vmar->Unmap(base, size, op_children);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_protect(VmAddressRegion* vmar, vaddr_t base,
                                                            size_t size,
                                                            arch_mmu_flags_t new_arch_mmu_flags,
                                                            VmAddressRegionOpChildren op_children) {
  return vmar->Protect(base, size, new_arch_mmu_flags, op_children);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_range_op(VmAddressRegion* vmar,
                                                             VmAddressRegion::RangeOpType op,
                                                             vaddr_t base, size_t len,
                                                             VmAddressRegionOpChildren op_children,
                                                             void* buffer, size_t buffer_size) {
  return vmar->RangeOp(op, base, len, op_children, make_user_inout_ptr(buffer), buffer_size);
}
FFI_ALWAYS_INLINE zx_status_t cpp_vm_address_region_reserve_space(VmAddressRegion* vmar,
                                                                  const char* name, size_t base,
                                                                  size_t size,
                                                                  arch_mmu_flags_t arch_mmu_flags) {
  return vmar->ReserveSpace(name, base, size, arch_mmu_flags);
}
FFI_ALWAYS_INLINE VmAddressRegion* cpp_vm_address_region_create_sub_vmar(
    VmAddressRegion* vmar, size_t offset, size_t size, uint8_t align_pow2, uint32_t vmar_flags,
    const char* name, zx_status_t* out_status) {
  fbl::RefPtr<VmAddressRegion> sub_vmar;
  *out_status = vmar->CreateSubVmar(offset, size, align_pow2, vmar_flags, name, &sub_vmar);
  return fbl::ExportToRawPtr(&sub_vmar);
}
FFI_ALWAYS_INLINE VmMapping* cpp_vm_address_region_create_vm_mapping(
    VmAddressRegion* vmar, size_t mapping_offset, size_t size, uint8_t align_pow2,
    uint32_t vmar_flags, const VmObject* vmo, uint64_t vmo_offset, arch_mmu_flags_t arch_mmu_flags,
    const char* name, vaddr_t* out_base, zx_status_t* out_status) {
  fbl::RefPtr<VmObject> vmo_ref = fbl::ImportFromRawPtr(const_cast<VmObject*>(vmo));
  auto result = vmar->CreateVmMapping(mapping_offset, size, align_pow2, vmar_flags,
                                      ktl::move(vmo_ref), vmo_offset, arch_mmu_flags, name);
  if (result.is_error()) {
    *out_status = result.status_value();
    return nullptr;
  }
  *out_status = ZX_OK;
  *out_base = result->base;
  fbl::RefPtr<VmMapping> mapping = ktl::move(result->mapping);
  return fbl::ExportToRawPtr(&mapping);
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_enumerator_construct(
    ffi::Uninitialized<CppVmAddressRegionEnumerator>* out, VmAddressRegionEnumeratorType type,
    VmAddressRegion* vmar, vaddr_t min_addr, vaddr_t max_addr) {
  AssertHeld(vmar->lock_ref());
  new (out->GetAddressUnchecked()) CppVmAddressRegionEnumerator(type, *vmar, min_addr, max_addr);
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_enumerator_destroy(
    CppVmAddressRegionEnumerator* enumerator) {
  ktl::destroy_at(enumerator);
}
FFI_ALWAYS_INLINE VmAddressRegionOrMapping* cpp_vm_address_region_enumerator_next(
    CppVmAddressRegionEnumerator* enumerator, uint* out_depth) {
  auto next = [&](auto& impl) {
    AssertHeld(impl.lock_ref());
    if (auto result = impl.next()) {
      *out_depth = result->depth;
      return result->region_or_mapping;
    }
    return static_cast<VmAddressRegionOrMapping*>(nullptr);
  };
  switch (enumerator->type) {
    case VmAddressRegionEnumeratorType::VmarsAndMappings:
      return next(enumerator->vmars_and_mappings);
    case VmAddressRegionEnumeratorType::MappingsOnly:
      return next(enumerator->mappings_only);
  }
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_enumerator_pause(
    CppVmAddressRegionEnumerator* enumerator) {
  switch (enumerator->type) {
    case VmAddressRegionEnumeratorType::VmarsAndMappings:
      AssertHeld(enumerator->vmars_and_mappings.lock_ref());
      enumerator->vmars_and_mappings.pause();
      break;
    case VmAddressRegionEnumeratorType::MappingsOnly:
      AssertHeld(enumerator->mappings_only.lock_ref());
      enumerator->mappings_only.pause();
      break;
  }
}
FFI_ALWAYS_INLINE void cpp_vm_address_region_enumerator_resume(
    CppVmAddressRegionEnumerator* enumerator) {
  switch (enumerator->type) {
    case VmAddressRegionEnumeratorType::VmarsAndMappings:
      AssertHeld(enumerator->vmars_and_mappings.lock_ref());
      enumerator->vmars_and_mappings.resume();
      break;
    case VmAddressRegionEnumeratorType::MappingsOnly:
      AssertHeld(enumerator->mappings_only.lock_ref());
      enumerator->mappings_only.resume();
      break;
  }
}
}  // extern "C"
