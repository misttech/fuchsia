// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <dev/iommu/bti.h>
#include <dev/iommu/stub/stub.h>
#include <fbl/alloc_checker.h>

namespace iommu {

void Bti::PrintQuarantineWarning(BtiPageLeakReason reason, uint64_t total_leaked_pages,
                                 size_t total_leaked_vmos) {}

zx::result<fbl::RefPtr<Iommu>> StubIommu::Create() {
  fbl::AllocChecker ac;
  fbl::RefPtr<StubIommu> instance = fbl::AdoptRef<StubIommu>(new (&ac) StubIommu());
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }
  return zx::ok(std::move(instance));
}

zx::result<fbl::RefPtr<Bti>> StubIommu::CreateBti(uint64_t bti_id) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

}  // namespace iommu
