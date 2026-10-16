// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_FFI_H_

#include <zircon/compiler.h>
#include <zircon/types.h>

#include <kernel/ffi.h>
#include <vm/vm_address_region.h>
#include <vm/vm_address_region_enumerator.h>

struct CppVmAddressRegionEnumerator {
  using VmarsAndMappings =
      VmAddressRegionEnumerator<VmAddressRegionEnumeratorType::VmarsAndMappings, VmAddressRegion>;
  using MappingsOnly =
      VmAddressRegionEnumerator<VmAddressRegionEnumeratorType::MappingsOnly, VmAddressRegion>;

  CppVmAddressRegionEnumerator(VmAddressRegionEnumeratorType type, VmAddressRegion& vmar,
                               vaddr_t min_addr, vaddr_t max_addr) TA_REQ(vmar.lock());
  ~CppVmAddressRegionEnumerator();

  VmAddressRegionEnumeratorType type;
  union {
    VmarsAndMappings vmars_and_mappings;
    MappingsOnly mappings_only;
  };
};

__BEGIN_CDECLS

void cpp_vm_address_region_free(VmAddressRegion* vmar);
void* cpp_vm_address_region_state(VmAddressRegion* vmar);
void* cpp_vm_address_region_or_mapping_state(VmAddressRegionOrMapping* region_or_mapping);
zx_status_t cpp_vm_address_region_destroy(VmAddressRegion* vmar);
zx_status_t cpp_vm_address_region_set_memory_priority(VmAddressRegion* vmar,
                                                      VmAddressRegion::MemoryPriority priority);
zx_status_t cpp_vm_address_region_unmap(VmAddressRegion* vmar, vaddr_t base, size_t size,
                                        VmAddressRegionOpChildren op_children);
zx_status_t cpp_vm_address_region_protect(VmAddressRegion* vmar, vaddr_t base, size_t size,
                                          arch_mmu_flags_t new_arch_mmu_flags,
                                          VmAddressRegionOpChildren op_children);
zx_status_t cpp_vm_address_region_range_op(VmAddressRegion* vmar, VmAddressRegion::RangeOpType op,
                                           vaddr_t base, size_t len,
                                           VmAddressRegionOpChildren op_children, void* buffer,
                                           size_t buffer_size);
zx_status_t cpp_vm_address_region_reserve_space(VmAddressRegion* vmar, const char* name,
                                                size_t base, size_t size,
                                                arch_mmu_flags_t arch_mmu_flags);
VmAddressRegion* cpp_vm_address_region_create_sub_vmar(VmAddressRegion* vmar, size_t offset,
                                                       size_t size, uint8_t align_pow2,
                                                       uint32_t vmar_flags, const char* name,
                                                       zx_status_t* out_status);
VmMapping* cpp_vm_address_region_create_vm_mapping(
    VmAddressRegion* vmar, size_t mapping_offset, size_t size, uint8_t align_pow2,
    uint32_t vmar_flags, const VmObject* vmo, uint64_t vmo_offset, arch_mmu_flags_t arch_mmu_flags,
    const char* name, vaddr_t* out_base, zx_status_t* out_status);

void cpp_vm_address_region_enumerator_construct(
    ffi::Uninitialized<CppVmAddressRegionEnumerator>* out, VmAddressRegionEnumeratorType type,
    VmAddressRegion* vmar, vaddr_t min_addr, vaddr_t max_addr);
void cpp_vm_address_region_enumerator_destroy(CppVmAddressRegionEnumerator* enumerator);
VmAddressRegionOrMapping* cpp_vm_address_region_enumerator_next(
    CppVmAddressRegionEnumerator* enumerator, uint* out_depth);
void cpp_vm_address_region_enumerator_pause(CppVmAddressRegionEnumerator* enumerator);
void cpp_vm_address_region_enumerator_resume(CppVmAddressRegionEnumerator* enumerator);

void cpp_vm_mapping_free(VmMapping* mapping);
void* cpp_vm_mapping_state(VmMapping* mapping);
zx_status_t cpp_vm_mapping_destroy(VmMapping* mapping);
zx_status_t cpp_vm_mapping_map_range(VmMapping* mapping, size_t offset, size_t len, bool commit,
                                     bool ignore_existing);
zx_status_t cpp_vm_mapping_debug_unmap(VmMapping* mapping, vaddr_t base, size_t size);
zx_status_t cpp_vm_mapping_debug_protect(VmMapping* mapping, vaddr_t base, size_t size,
                                         arch_mmu_flags_t new_arch_mmu_flags);
zx_status_t cpp_vm_mapping_force_writable(VmMapping* mapping, VmMapping** out_mapping);
void cpp_vm_mapping_mark_mergeable(VmMapping* mapping);
typedef zx_status_t (*cpp_vm_mapping_enumerate_protection_ranges_cb)(void* ctx, vaddr_t base,
                                                                     size_t len,
                                                                     arch_mmu_flags_t flags);
zx_status_t cpp_vm_mapping_enumerate_protection_ranges_locked(
    const VmMapping* mapping, vaddr_t base, size_t size,
    cpp_vm_mapping_enumerate_protection_ranges_cb cb, void* ctx);
VmMapping::FlagsRange cpp_vm_mapping_arch_mmu_flags_range_locked(const VmMapping* mapping,
                                                                 vaddr_t offset);

__END_CDECLS

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ADDRESS_REGION_FFI_H_
