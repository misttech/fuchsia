// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_INTEL_INCLUDE_DEV_IOMMU_INTEL_H_
#define ZIRCON_KERNEL_DEV_IOMMU_INTEL_INCLUDE_DEV_IOMMU_INTEL_H_

#include <zircon/syscalls/iommu.h>

#include <dev/iommu.h>
#include <fbl/ref_ptr.h>

class IntelIommu {
 public:
  static zx::result<fbl::RefPtr<Iommu>> Create(ktl::unique_ptr<const uint8_t[]> desc_bytes,
                                               size_t desc_len);
};

#endif  // ZIRCON_KERNEL_DEV_IOMMU_INTEL_INCLUDE_DEV_IOMMU_INTEL_H_
