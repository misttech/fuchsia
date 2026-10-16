// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_FFI_H_
#define ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_FFI_H_

#include <zircon/types.h>

#include <vm/vm_address_region.h>
#include <vm/vm_object.h>
#include <vm/vm_object_paged.h>
#include <vm/vm_object_physical.h>

#include "vm/vm_aspace.h"

extern "C" {

fbl::RefCounted<VmAspace>* cpp_vm_aspace_get_ref_counted(VmAspace* aspace);
VmAspace* cpp_vm_aspace_create(VmAspace::Type type, const char* name);
VmAspace* cpp_vm_aspace_create_with_opts(vaddr_t base, size_t size, VmAspace::Type type,
                                         const char* name, VmAspace::ShareOpt share_opt);
VmAspace* cpp_vm_aspace_create_unified(VmAspace* shared, VmAspace* restricted, const char* name);
VmAspace* cpp_vm_aspace_kernel_aspace();
VmAddressRegion* cpp_vm_aspace_root_vmar(VmAspace* aspace);
VmAddressRegion* cpp_vm_aspace_root_vmar_locked(VmAspace* aspace);
vaddr_t cpp_vm_aspace_base(VmAspace* aspace);
size_t cpp_vm_aspace_size(VmAspace* aspace);
const char* cpp_vm_aspace_name(VmAspace* aspace);
bool cpp_vm_aspace_is_user(VmAspace* aspace);
bool cpp_vm_aspace_is_aslr_enabled(VmAspace* aspace);
bool cpp_vm_aspace_is_destroyed(VmAspace* aspace);
zx_status_t cpp_vm_aspace_destroy(VmAspace* aspace);
void cpp_vm_aspace_rename(VmAspace* aspace, const char* name);
void cpp_vm_aspace_dump(VmAspace* aspace, bool verbose);
void cpp_vm_aspace_attach_to_thread(VmAspace* aspace, Thread* thread);
uintptr_t cpp_vm_aspace_vdso_base_address(VmAspace* aspace);
uintptr_t cpp_vm_aspace_vdso_code_address(VmAspace* aspace);
bool cpp_vm_aspace_is_high_memory_priority(VmAspace* aspace);
zx_status_t cpp_vm_aspace_accessed_fault(VmAspace* aspace, vaddr_t va);
zx_status_t cpp_vm_aspace_page_fault(VmAspace* aspace, vaddr_t va, uint flags);
zx_status_t cpp_vm_aspace_soft_fault(VmAspace* aspace, vaddr_t va, uint flags);
zx_status_t cpp_vm_aspace_soft_fault_in_range(VmAspace* aspace, vaddr_t va, uint flags, size_t len);
void cpp_vm_aspace_drop_user_page_tables(VmAspace* aspace);
void cpp_vm_aspace_drop_all_user_page_tables();
void cpp_vm_aspace_dump_all_aspaces(bool verbose);
void cpp_vm_aspace_harvest_all_user_accessed_bits(
    ArchVmAspaceInterface::NonTerminalAction non_terminal_action,
    ArchVmAspaceInterface::TerminalAction terminal_action);
zx_status_t cpp_vm_aspace_alloc_physical(VmAspace* aspace, const char* name, size_t size,
                                         void** ptr, uint8_t align_pow2, paddr_t paddr,
                                         uint vmm_flags, arch_mmu_flags_t arch_mmu_flags);
zx_status_t cpp_vm_aspace_alloc_contiguous(VmAspace* aspace, const char* name, size_t size,
                                           void** ptr, uint8_t align_pow2, uint vmm_flags,
                                           arch_mmu_flags_t arch_mmu_flags);
FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_map_object_internal(
    VmAspace* aspace, VmObject* vmo, const char* name, uint64_t offset, size_t size, void** ptr,
    uint8_t align_pow2, uint32_t vmm_flags, arch_mmu_flags_t arch_mmu_flags);
zx_status_t cpp_vm_aspace_free_region(VmAspace* aspace, vaddr_t va);
void cpp_vm_aspace_free(VmAspace* aspace);
ArchVmAspace* cpp_vm_aspace_arch_aspace(VmAspace* aspace);
VmMapping* cpp_vm_aspace_find_mapping(VmAspace* aspace, zx_vaddr_t vaddr);
void* cpp_vm_aspace_lock(const VmAspace* aspace);
void* cpp_vm_aspace_region_lock(const VmAspace* aspace);
}

#endif  // ZIRCON_KERNEL_VM_INCLUDE_VM_VM_ASPACE_FFI_H_
