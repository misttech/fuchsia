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

extern "C" {

zx_status_t cpp_arm_smmu_sleep_relative(zx_duration_t delay_ns);

#if defined(ARM_SMMU_HOST_TEST)
void cpp_arm_smmu_clean_dcache_range(uintptr_t start, size_t len);
void cpp_arm_smmu_on_pte_write(const uint64_t* slot, uint64_t new_val, uint32_t level);
#endif  // defined(ARM_SMMU_HOST_TEST)

}  // extern "C"

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_SMMU_ENV_FFI_H_
