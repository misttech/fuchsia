// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/bus_transaction_initiator_dispatcher.h"

#include <align.h>
#include <lib/counters.h>
#include <lib/debuglog.h>
#include <lib/page/size.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <new>

#include <dev/iommu/iommu.h>
#include <object/process_dispatcher.h>
#include <object/root_job_observer.h>
#include <object/thread_dispatcher.h>
#include <vm/pinned_vm_object.h>
#include <vm/vm_object.h>

KCOUNTER(dispatcher_bti_create_count, "dispatcher.bti.create")
KCOUNTER(dispatcher_bti_destroy_count, "dispatcher.bti.destroy")

zx_status_t BusTransactionInitiatorDispatcher::Create(
    Iommu& iommu, uint64_t bti_id, KernelHandle<BusTransactionInitiatorDispatcher>* handle,
    zx_rights_t* rights) {
  zx::result<fbl::RefPtr<iommu::Bti>> maybe_bti = iommu.CreateBti(bti_id);
  if (!maybe_bti.is_ok()) {
    return maybe_bti.error_value();
  }

  fbl::AllocChecker ac;
  KernelHandle new_handle(
      fbl::AdoptRef(new (&ac) BusTransactionInitiatorDispatcher(ktl::move(maybe_bti.value()))));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  *rights = default_rights();
  *handle = ktl::move(new_handle);
  return ZX_OK;
}

BusTransactionInitiatorDispatcher::BusTransactionInitiatorDispatcher(fbl::RefPtr<Bti> bti)
    : bti_(ktl::move(bti)) {
  DEBUG_ASSERT(bti_ != nullptr);
  kcounter_add(dispatcher_bti_create_count, 1);
}

BusTransactionInitiatorDispatcher::~BusTransactionInitiatorDispatcher() {
  kcounter_add(dispatcher_bti_destroy_count, 1);
}

zx_status_t BusTransactionInitiatorDispatcher::Pin(
    fbl::RefPtr<VmObject> vmo, uint64_t offset, uint64_t size, uint32_t perms,
    KernelHandle<PinnedMemoryTokenDispatcher>* pmt_handle, zx_rights_t* pmt_rights) {
  DEBUG_ASSERT(IsPageRounded(offset));
  DEBUG_ASSERT(IsPageRounded(size));

  if (size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  PinnedVmObject pinned_vmo;
  zx_status_t status =
      PinnedVmObject::Create(vmo, offset, size, perms & IOMMU_FLAG_PERM_WRITE, &pinned_vmo);
  if (status != ZX_OK) {
    return status;
  }

  Guard<CriticalMutex> guard{get_lock()};
  // User may not pin new memory if either our BTI has hit zero handles, or if
  // the underlying driver is in a fault state (usually because the BTI has
  // quarantined pages).  In the case that the driver-level BTI is in a fault
  // state, user-mode driver code is expected to take the steps to stop their
  // DMA, and then call `zx_bti_release_quarantine` before proceeding to pin new
  // memory.
  if (zero_handles_ || bti_->in_fault_state()) {
    return ZX_ERR_BAD_STATE;
  }

  return PinnedMemoryTokenDispatcher::Create(fbl::RefPtr(this), ktl::move(pinned_vmo), perms,
                                             pmt_handle, pmt_rights);
}

void BusTransactionInitiatorDispatcher::on_zero_handles() {
  {
    Guard<CriticalMutex> guard{get_lock()};
    // Prevent new pinning from happening.  The Dispatcher will stick around
    // until all of the PMTs are closed.
    zero_handles_ = true;
  }

  bti().OnDispatcherZeroHandles();
}

zx_info_bti_t BusTransactionInitiatorDispatcher::GetInfo() const {
  // TODO(johngro): Consider refactoring this so that the GetInfo operation can
  // be made in an atomic fashion.
  zx_info_bti_t info = {
      .minimum_contiguity = minimum_contiguity(),
      .aspace_size = aspace_size(),
      .pmo_count = pmo_count(),
      .quarantine_count = quarantine_count(),
  };
  return info;
}
