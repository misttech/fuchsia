// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_ARCH_ARM64_PERIPHMAP_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_ARCH_ARM64_PERIPHMAP_H_

#include <lib/arch/cache.h>
#include <sys/types.h>

vaddr_t periph_paddr_to_vaddr(paddr_t paddr);
paddr_t periph_vaddr_to_paddr(vaddr_t vaddr);

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_ARCH_ARM64_PERIPHMAP_H_
