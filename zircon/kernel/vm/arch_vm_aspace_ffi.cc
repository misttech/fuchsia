// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/types.h>

#include <arch/aspace.h>
#include <vm/arch_vm_aspace.h>

extern "C" {
zx_status_t cpp_arch_vm_aspace_init(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_init_shared(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_init_restricted(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_init_unified(ArchVmAspace* aspace, ArchVmAspace* shared,
                                            ArchVmAspace* restricted);
void cpp_arch_vm_aspace_disable_updates(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_destroy(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_map_contiguous(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t paddr,
                                              size_t count, arch_mmu_flags_t mmu_flags);
zx_status_t cpp_arch_vm_aspace_map(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t* phys, size_t count,
                                   arch_mmu_flags_t mmu_flags,
                                   ArchVmAspaceInterface::ExistingEntryAction existing_action);
zx_status_t cpp_arch_vm_aspace_unmap(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                                     ArchVmAspaceInterface::ArchUnmapOptions enlarge);
bool cpp_arch_vm_aspace_unmap_only_enlarge_on_oom(ArchVmAspace* aspace);
zx_status_t cpp_arch_vm_aspace_protect(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                                       arch_mmu_flags_t mmu_flags,
                                       ArchVmAspaceInterface::ArchUnmapOptions enlarge);
zx_status_t cpp_arch_vm_aspace_query(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t* paddr,
                                     arch_mmu_flags_t* mmu_flags);
vaddr_t cpp_arch_vm_aspace_pick_spot(ArchVmAspace* aspace, vaddr_t base, vaddr_t end, vaddr_t align,
                                     size_t size, arch_mmu_flags_t mmu_flags);
zx_status_t cpp_arch_vm_aspace_harvest_accessed(
    ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
    ArchVmAspaceInterface::NonTerminalAction non_terminal_action,
    ArchVmAspaceInterface::TerminalAction terminal_action);
zx_status_t cpp_arch_vm_aspace_mark_accessed(ArchVmAspace* aspace, vaddr_t vaddr, size_t count);
bool cpp_arch_vm_aspace_accessed_since_last_check(ArchVmAspace* aspace, bool clear);
paddr_t cpp_arch_vm_aspace_arch_table_phys(ArchVmAspace* aspace);

zx_status_t cpp_arch_vm_aspace_init(ArchVmAspace* aspace) { return aspace->Init(); }
zx_status_t cpp_arch_vm_aspace_init_shared(ArchVmAspace* aspace) { return aspace->InitShared(); }
zx_status_t cpp_arch_vm_aspace_init_restricted(ArchVmAspace* aspace) {
  return aspace->InitRestricted();
}
zx_status_t cpp_arch_vm_aspace_init_unified(ArchVmAspace* aspace, ArchVmAspace* shared,
                                            ArchVmAspace* restricted) {
  return aspace->InitUnified(*shared, *restricted);
}
void cpp_arch_vm_aspace_disable_updates(ArchVmAspace* aspace) { aspace->DisableUpdates(); }
zx_status_t cpp_arch_vm_aspace_destroy(ArchVmAspace* aspace) { return aspace->Destroy(); }
zx_status_t cpp_arch_vm_aspace_map_contiguous(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t paddr,
                                              size_t count, arch_mmu_flags_t mmu_flags) {
  return aspace->MapContiguous(vaddr, paddr, count, mmu_flags);
}
zx_status_t cpp_arch_vm_aspace_map(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t* phys, size_t count,
                                   arch_mmu_flags_t mmu_flags,
                                   ArchVmAspaceInterface::ExistingEntryAction existing_action) {
  return aspace->Map(vaddr, phys, count, mmu_flags, existing_action);
}
zx_status_t cpp_arch_vm_aspace_unmap(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                                     ArchVmAspaceInterface::ArchUnmapOptions enlarge) {
  return aspace->Unmap(vaddr, count, enlarge);
}
bool cpp_arch_vm_aspace_unmap_only_enlarge_on_oom(ArchVmAspace* aspace) {
  return aspace->UnmapOnlyEnlargeOnOom();
}
zx_status_t cpp_arch_vm_aspace_protect(ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
                                       arch_mmu_flags_t mmu_flags,
                                       ArchVmAspaceInterface::ArchUnmapOptions enlarge) {
  return aspace->Protect(vaddr, count, mmu_flags, enlarge);
}
zx_status_t cpp_arch_vm_aspace_query(ArchVmAspace* aspace, vaddr_t vaddr, paddr_t* paddr,
                                     arch_mmu_flags_t* mmu_flags) {
  return aspace->Query(vaddr, paddr, mmu_flags);
}
vaddr_t cpp_arch_vm_aspace_pick_spot(ArchVmAspace* aspace, vaddr_t base, vaddr_t end, vaddr_t align,
                                     size_t size, arch_mmu_flags_t mmu_flags) {
  return aspace->PickSpot(base, end, align, size, mmu_flags);
}
zx_status_t cpp_arch_vm_aspace_harvest_accessed(
    ArchVmAspace* aspace, vaddr_t vaddr, size_t count,
    ArchVmAspaceInterface::NonTerminalAction non_terminal_action,
    ArchVmAspaceInterface::TerminalAction terminal_action) {
  return aspace->HarvestAccessed(vaddr, count, non_terminal_action, terminal_action);
}
zx_status_t cpp_arch_vm_aspace_mark_accessed(ArchVmAspace* aspace, vaddr_t vaddr, size_t count) {
  return aspace->MarkAccessed(vaddr, count);
}
bool cpp_arch_vm_aspace_accessed_since_last_check(ArchVmAspace* aspace, bool clear) {
  return aspace->AccessedSinceLastCheck(clear);
}
paddr_t cpp_arch_vm_aspace_arch_table_phys(ArchVmAspace* aspace) {
  return aspace->arch_table_phys();
}
}
