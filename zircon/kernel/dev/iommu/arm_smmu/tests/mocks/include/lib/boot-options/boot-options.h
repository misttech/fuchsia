// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_BOOT_OPTIONS_BOOT_OPTIONS_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_BOOT_OPTIONS_BOOT_OPTIONS_H_

#include <lib/boot-options/arm64.h>
#include <stdio.h>

#include <array>

struct BootOptions {
  std::array<char, 128> arm_smmu_mode{"enforced"};

  static BootOptions* Get() {
    static BootOptions instance;
    return &instance;
  }

  void SetArmSmmuMode(const char* mode) {
    snprintf(arm_smmu_mode.data(), arm_smmu_mode.size(), "%s", mode ? mode : "");
  }
};

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_LIB_BOOT_OPTIONS_BOOT_OPTIONS_H_
