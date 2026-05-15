// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_ZIRCON_ASSERT_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_ZIRCON_ASSERT_H_

#include <stdio.h>

// In the mocks we use to test kernel code as a host test, make sure that all of
// our asserts are always enabled.
#define ZX_DEBUG_ASSERT_IMPLEMENTED (1)

#define ZX_DEBUG_ASSERT(x)                                 \
  do {                                                     \
    if (!(x)) {                                            \
      printf("ASSERT FAILED %s:%d\n", __FILE__, __LINE__); \
      __builtin_trap();                                    \
    }                                                      \
  } while (0)

#define ZX_DEBUG_ASSERT_MSG(x, msg, ...)                                           \
  do {                                                                             \
    if (!(x)) {                                                                    \
      printf("ASSERT FAILED %s:%d\n" msg "\n", __FILE__, __LINE__, ##__VA_ARGS__); \
      __builtin_trap();                                                            \
    }                                                                              \
  } while (0)

#define ZX_DEBUG_ASSERT_COND ZX_DEBUG_ASSERT
#define ZX_ASSERT ZX_DEBUG_ASSERT
#define ZX_ASSERT_MSG ZX_DEBUG_ASSERT_MSG
#define ZX_ASSERT_COND ZX_DEBUG_ASSERT
#define DEBUG_ASSERT ZX_DEBUG_ASSERT
#define DEBUG_ASSERT_MSG ZX_DEBUG_ASSERT_MSG
#define ASSERT ZX_DEBUG_ASSERT
#define ASSERT_MSG ZX_DEBUG_ASSERT_MSG

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_ZIRCON_ASSERT_H_
