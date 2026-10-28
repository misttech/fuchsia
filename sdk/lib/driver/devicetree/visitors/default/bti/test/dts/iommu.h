// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEVICETREE_VISITORS_DEFAULT_BTI_TEST_DTS_IOMMU_H_
#define LIB_DRIVER_DEVICETREE_VISITORS_DEFAULT_BTI_TEST_DTS_IOMMU_H_

// Giving a random phandle to check that iommu index is different from phandle.
#define TEST_IOMMU_PHANDLE 20
#define TEST_ARM_SMMU_PHANDLE 21

#define TEST_ARM_SMMU_REG_HIGH 0x1
#define TEST_ARM_SMMU_REG_LOW 0x00000000
#define TEST_ARM_SMMU_BASE \
  ((uint64_t{TEST_ARM_SMMU_REG_HIGH} << 32) | uint64_t{TEST_ARM_SMMU_REG_LOW})

#define TEST_BTI_ID1 6
#define TEST_BTI_ID2 3
#define TEST_BTI_ID3 9

#define TEST_BTI_ID2_NAME "device2"
#define TEST_BTI_ID3_NAME "device3"

#endif  // LIB_DRIVER_DEVICETREE_VISITORS_DEFAULT_BTI_TEST_DTS_IOMMU_H_
