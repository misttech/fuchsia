// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_ROOT_RESOURCE_FILTER_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_ROOT_RESOURCE_FILTER_H_

#include <stddef.h>
#include <stdint.h>
#include <zircon/syscalls/resource.h>

inline void root_resource_filter_add_deny_region(uintptr_t base, size_t size, zx_rsrc_kind_t kind) {
}

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_ROOT_RESOURCE_FILTER_H_
