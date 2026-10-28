// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_DEV_IOMMU_STUB_STUB_H_
#define ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_DEV_IOMMU_STUB_STUB_H_

#include <lib/zx/result.h>
#include <zircon/compiler.h>
#include <zircon/syscalls/iommu.h>

#include <dev/iommu/iommu.h>
#include <fbl/ref_ptr.h>

namespace iommu {

class StubIommu final : public Iommu {
 public:
  StubIommu(const StubIommu&) = delete;
  StubIommu(StubIommu&&) = delete;
  StubIommu& operator=(const StubIommu&) = delete;
  StubIommu& operator=(StubIommu&&) = delete;

  static zx::result<fbl::RefPtr<Iommu>> Create();
  zx::result<fbl::RefPtr<Bti>> CreateBti(uint64_t bti_id) override;

 private:
  friend class fbl::RefPtr<StubIommu>;

  StubIommu() = default;
  ~StubIommu() final = default;
};

}  // namespace iommu

using StubIommu = ::iommu::StubIommu;

#endif  // ZIRCON_KERNEL_DEV_IOMMU_ARM_SMMU_TESTS_MOCKS_INCLUDE_DEV_IOMMU_STUB_STUB_H_
