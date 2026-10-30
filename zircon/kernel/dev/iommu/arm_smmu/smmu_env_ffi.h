// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_SMMU_ENV_FFI_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_SMMU_ENV_FFI_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/types.h>

#include <vm/page.h>
#include <vm/pinned_vm_object.h>

extern "C" {

#if defined(ARM_SMMU_HOST_TEST)
uintptr_t cpp_arm_smmu_periph_paddr_to_vaddr(paddr_t paddr);
paddr_t cpp_arm_smmu_periph_vaddr_to_paddr(uintptr_t vaddr);
zx_status_t cpp_arm_smmu_sleep_relative(zx_duration_t delay_ns);
void cpp_arm_smmu_clean_dcache_range(uintptr_t start, size_t len);
void cpp_arm_smmu_on_pte_write(const uint64_t* slot, uint64_t new_val, uint32_t level);
void cpp_arm_smmu_on_table_zero(uintptr_t table_vaddr, size_t len);
zx_status_t cpp_arm_smmu_pmm_alloc_page(uint32_t alloc_flags, vm_page_t** out_page);
void cpp_arm_smmu_pmm_free_page(vm_page_t* page);
vm_page_t* cpp_arm_smmu_paddr_to_vm_page(paddr_t pa);
paddr_t cpp_arm_smmu_vm_page_paddr(const vm_page_t* page);
uintptr_t cpp_arm_smmu_paddr_to_physmap(paddr_t pa);
paddr_t cpp_arm_smmu_physmap_to_paddr(uintptr_t vaddr);
uint64_t cpp_arm_smmu_pinned_vmo_offset(const PinnedVmObject* pinned_vmo);
uint64_t cpp_arm_smmu_pinned_vmo_size(const PinnedVmObject* pinned_vmo);
zx_status_t cpp_arm_smmu_pinned_vmo_lookup_contiguous(const PinnedVmObject* pinned_vmo,
                                                      uint64_t offset, size_t size,
                                                      paddr_t* out_paddr);
void cpp_arm_smmu_pinned_vmo_reset(void* pinned_vmo);
void cpp_arm_smmu_pinned_vmo_move_construct(void* dst, void* src);
size_t cpp_arm_smmu_get_boot_option_mode(uint8_t* out_buf, size_t out_cap);
void cpp_arm_smmu_root_resource_filter_add_deny_region(uint64_t base, size_t size, uint32_t kind);
zx_status_t cpp_arm_smmu_configure_interrupt(uint32_t vector, uint32_t trigger_mode,
                                             uint32_t polarity);
zx_status_t cpp_arm_smmu_mask_interrupt(uint32_t vector);
zx_status_t cpp_arm_smmu_unmask_interrupt(uint32_t vector);
zx_status_t cpp_arm_smmu_register_int_handler(uint32_t vector, void* cookie,
                                              void (*handler)(void*));
zx_status_t cpp_arm_smmu_unregister_int_handler(uint32_t vector);
uint32_t cpp_arm_smmu_mmio_read32(uintptr_t base, uint32_t offset);
void cpp_arm_smmu_mmio_write32(uintptr_t base, uint32_t offset, uint32_t val);
uint64_t cpp_arm_smmu_mmio_read64(uintptr_t base, uint32_t offset);
void cpp_arm_smmu_mmio_write64(uintptr_t base, uint32_t offset, uint64_t val);
size_t cpp_arm_smmu_pmm_allocated_page_count(void);
void cpp_arm_smmu_pmm_set_allocation_limit(uint64_t limit);
void cpp_arm_smmu_pmm_clear_allocation_limit(void);
bool cpp_arm_smmu_pmm_read_poc_dram64(uint64_t pa, uint64_t* out_val);
bool cpp_arm_smmu_pmm_read_cpu_cache64(uint64_t pa, uint64_t* out_val);
#endif  // defined(ARM_SMMU_HOST_TEST)

}  // extern "C"

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_SMMU_ENV_FFI_H_
