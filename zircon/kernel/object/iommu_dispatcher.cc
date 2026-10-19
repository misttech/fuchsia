// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/iommu_dispatcher.h"

#include <lib/object-constants.h>

#include <dev/iommu/iommu.h>
#include <kernel/ffi.h>

IommuDispatcher::IommuDispatcher(fbl::RefPtr<Iommu> iommu) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(IommuDispatcher, kIommuDispatcherStateOffset);
  rust_iommu_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&iommu));
}

IMPLEMENT_DISPATCHER_RUST_STATE(IommuDispatcher, rust_iommu_dispatcher_state_get_lock,
                                rust_iommu_dispatcher_state_destroy)

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void cpp_iommu_recycle(iommu::Iommu* iommu) { delete iommu; }

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE void* cpp_iommu_get_ref_counted(const iommu::Iommu* iommu) {
  return const_cast<fbl::RefCounted<iommu::Iommu>*>(
      static_cast<const fbl::RefCounted<iommu::Iommu>*>(iommu));
}
