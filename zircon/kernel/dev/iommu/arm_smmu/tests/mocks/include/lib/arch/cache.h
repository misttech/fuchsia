// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_ARCH_CACHE_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_ARCH_CACHE_H_

#include <stddef.h>
#include <stdint.h>

#include <vm/pmm.h>

namespace arch {

inline void CleanDataCacheRange(uint64_t addr, size_t size) {
  PmmMock::Get().CleanDataCacheRange(addr, size);
}

inline void OnPteWrite(const uint64_t* slot, uint64_t new_val, uint32_t level) {
  PmmMock::Get().OnPteWrite(slot, new_val, level);
}

inline void OnTableZero(uint64_t addr, size_t size) { PmmMock::Get().OnTableZero(addr, size); }
inline void Yield() {}

inline void DeviceMemoryBarrier() {}

}  // namespace arch

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_ARCH_CACHE_H_
