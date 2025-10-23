// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/iommu_dispatcher.h"

#include <assert.h>
#include <inttypes.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls/iommu.h>

#include <new>

#include <dev/iommu.h>
#include <dev/iommu/dummy.h>
#if ARCH_X86
#include <dev/iommu/intel.h>
#endif

#define LOCAL_TRACE 0

zx_status_t IommuDispatcher::Create(uint32_t type, ktl::unique_ptr<const uint8_t[]> desc,
                                    size_t desc_len, KernelHandle<IommuDispatcher>* handle,
                                    zx_rights_t* rights) {
  zx::result<fbl::RefPtr<Iommu>> result;
  switch (type) {
    case ZX_IOMMU_TYPE_DUMMY:
      result = DummyIommu::Create(ktl::move(desc), desc_len);
      break;
#if ARCH_X86
    case ZX_IOMMU_TYPE_INTEL:
      result = IntelIommu::Create(ktl::move(desc), desc_len);
      break;
#endif
    default:
      return ZX_ERR_NOT_SUPPORTED;
  }
  if (result.is_error()) {
    return result.error_value();
  }

  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) IommuDispatcher(ktl::move(result.value()))));
  if (!ac.check())
    return ZX_ERR_NO_MEMORY;

  *rights = default_rights();
  *handle = ktl::move(new_handle);
  return ZX_OK;
}

IommuDispatcher::IommuDispatcher(fbl::RefPtr<Iommu> iommu) : iommu_(iommu) {}

IommuDispatcher::~IommuDispatcher() {}
