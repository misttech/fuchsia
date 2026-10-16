// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <vm/vm_aspace_ffi.h>

extern "C" {
// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language
// inlining works.

FFI_ALWAYS_INLINE fbl::RefCounted<VmAspace>* cpp_vm_aspace_get_ref_counted(VmAspace* aspace) {
  return aspace;
}

FFI_ALWAYS_INLINE VmAspace* cpp_vm_aspace_create(VmAspace::Type type, const char* name) {
  auto aspace = VmAspace::Create(type, name);
  return fbl::ExportToRawPtr(&aspace);
}

FFI_ALWAYS_INLINE VmAspace* cpp_vm_aspace_create_with_opts(vaddr_t base, size_t size,
                                                           VmAspace::Type type, const char* name,
                                                           VmAspace::ShareOpt share_opt) {
  auto aspace = VmAspace::Create(base, size, type, name, share_opt);
  return fbl::ExportToRawPtr(&aspace);
}

FFI_ALWAYS_INLINE VmAspace* cpp_vm_aspace_create_unified(VmAspace* shared, VmAspace* restricted,
                                                         const char* name) {
  auto aspace = VmAspace::CreateUnified(shared, restricted, name);
  return fbl::ExportToRawPtr(&aspace);
}

FFI_ALWAYS_INLINE VmAspace* cpp_vm_aspace_kernel_aspace() { return VmAspace::kernel_aspace(); }

FFI_ALWAYS_INLINE VmAddressRegion* cpp_vm_aspace_root_vmar(VmAspace* aspace) {
  fbl::RefPtr<VmAddressRegion> vmar = aspace->RootVmar();
  return fbl::ExportToRawPtr(&vmar);
}

FFI_ALWAYS_INLINE VmAddressRegion* cpp_vm_aspace_root_vmar_locked(VmAspace* aspace)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  fbl::RefPtr<VmAddressRegion> vmar = aspace->RootVmarLocked();
  return fbl::ExportToRawPtr(&vmar);
}

FFI_ALWAYS_INLINE vaddr_t cpp_vm_aspace_base(VmAspace* aspace) { return aspace->base(); }

FFI_ALWAYS_INLINE size_t cpp_vm_aspace_size(VmAspace* aspace) { return aspace->size(); }

FFI_ALWAYS_INLINE const char* cpp_vm_aspace_name(VmAspace* aspace) { return aspace->name(); }

FFI_ALWAYS_INLINE bool cpp_vm_aspace_is_user(VmAspace* aspace) { return aspace->is_user(); }

FFI_ALWAYS_INLINE bool cpp_vm_aspace_is_aslr_enabled(VmAspace* aspace) {
  return aspace->is_aslr_enabled();
}

FFI_ALWAYS_INLINE bool cpp_vm_aspace_is_destroyed(VmAspace* aspace) {
  return aspace->is_destroyed();
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_destroy(VmAspace* aspace) { return aspace->Destroy(); }

FFI_ALWAYS_INLINE void cpp_vm_aspace_rename(VmAspace* aspace, const char* name) {
  aspace->Rename(name);
}

FFI_ALWAYS_INLINE void cpp_vm_aspace_dump(VmAspace* aspace, bool verbose) { aspace->Dump(verbose); }

FFI_ALWAYS_INLINE void cpp_vm_aspace_attach_to_thread(VmAspace* aspace, Thread* thread) {
  aspace->AttachToThread(thread);
}

FFI_ALWAYS_INLINE uintptr_t cpp_vm_aspace_vdso_base_address(VmAspace* aspace) {
  return aspace->vdso_base_address();
}

FFI_ALWAYS_INLINE uintptr_t cpp_vm_aspace_vdso_code_address(VmAspace* aspace) {
  return aspace->vdso_code_address();
}

FFI_ALWAYS_INLINE bool cpp_vm_aspace_is_high_memory_priority(VmAspace* aspace) {
  return aspace->IsHighMemoryPriority();
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_accessed_fault(VmAspace* aspace, vaddr_t va) {
  return aspace->AccessedFault(va);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_page_fault(VmAspace* aspace, vaddr_t va, uint flags) {
  return aspace->PageFault(va, flags);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_soft_fault(VmAspace* aspace, vaddr_t va, uint flags) {
  return aspace->SoftFault(va, flags);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_soft_fault_in_range(VmAspace* aspace, vaddr_t va,
                                                                uint flags, size_t len) {
  return aspace->SoftFaultInRange(va, flags, len);
}

FFI_ALWAYS_INLINE void cpp_vm_aspace_drop_user_page_tables(VmAspace* aspace) {
  aspace->DropUserPageTables();
}

FFI_ALWAYS_INLINE void cpp_vm_aspace_drop_all_user_page_tables() {
  VmAspace::DropAllUserPageTables();
}

FFI_ALWAYS_INLINE void cpp_vm_aspace_dump_all_aspaces(bool verbose) {
  VmAspace::DumpAllAspaces(verbose);
}

FFI_ALWAYS_INLINE void cpp_vm_aspace_harvest_all_user_accessed_bits(
    ArchVmAspaceInterface::NonTerminalAction non_terminal_action,
    ArchVmAspaceInterface::TerminalAction terminal_action) {
  VmAspace::HarvestAllUserAccessedBits(non_terminal_action, terminal_action);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_alloc_physical(VmAspace* aspace, const char* name,
                                                           size_t size, void** ptr,
                                                           uint8_t align_pow2, paddr_t paddr,
                                                           uint vmm_flags,
                                                           arch_mmu_flags_t arch_mmu_flags) {
  return aspace->AllocPhysical(name, size, ptr, align_pow2, paddr, vmm_flags, arch_mmu_flags);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_alloc_contiguous(VmAspace* aspace, const char* name,
                                                             size_t size, void** ptr,
                                                             uint8_t align_pow2, uint vmm_flags,
                                                             arch_mmu_flags_t arch_mmu_flags) {
  return aspace->AllocContiguous(name, size, ptr, align_pow2, vmm_flags, arch_mmu_flags);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_map_object_internal(
    VmAspace* aspace, VmObject* vmo, const char* name, uint64_t offset, size_t size, void** ptr,
    uint8_t align_pow2, uint32_t vmm_flags, arch_mmu_flags_t arch_mmu_flags) {
  fbl::RefPtr<VmObject> vmo_ref = fbl::ImportFromRawPtr(vmo);
  return aspace->MapObjectInternal(ktl::move(vmo_ref), name, offset, size, ptr, align_pow2,
                                   vmm_flags, arch_mmu_flags);
}

FFI_ALWAYS_INLINE zx_status_t cpp_vm_aspace_free_region(VmAspace* aspace, vaddr_t va) {
  return aspace->FreeRegion(va);
}

FFI_ALWAYS_INLINE void cpp_vm_aspace_free(VmAspace* aspace) { delete aspace; }

FFI_ALWAYS_INLINE ArchVmAspace* cpp_vm_aspace_arch_aspace(VmAspace* aspace) {
  return &aspace->arch_aspace();
}

FFI_ALWAYS_INLINE VmMapping* cpp_vm_aspace_find_mapping(VmAspace* aspace, zx_vaddr_t vaddr) {
  if (!aspace) {
    return nullptr;
  }
  auto region = aspace->FindRegion(vaddr);
  if (!region) {
    return nullptr;
  }
  auto vm_mapping = region->as_vm_mapping();
  if (!vm_mapping) {
    return nullptr;
  }
  return fbl::ExportToRawPtr(&vm_mapping);
}
FFI_ALWAYS_INLINE void* cpp_vm_aspace_lock(const VmAspace* aspace) { return aspace->lock(); }
FFI_ALWAYS_INLINE void* cpp_vm_aspace_region_lock(const VmAspace* aspace) {
  return aspace->region_lock();
}
}
