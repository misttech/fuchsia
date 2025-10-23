// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <dev/iommu/intel.h>
#include <ktl/utility.h>

#include "iommu_impl.h"

#include <ktl/enforce.h>

zx::result<fbl::RefPtr<Iommu>> IntelIommu::Create(ktl::unique_ptr<const uint8_t[]> desc_bytes,
                                                  size_t desc_len) {
  return intel_iommu::IommuImpl::Create(ktl::move(desc_bytes), desc_len);
}
