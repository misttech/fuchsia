// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/bus_transaction_initiator_dispatcher.h"

#include <align.h>
#include <lib/object-constants.h>
#include <lib/page/size.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <new>

#include <dev/iommu/iommu.h>
#include <kernel/ffi.h>
#include <object/pinned_memory_token_dispatcher.h>
#include <vm/pinned_vm_object.h>
#include <vm/vm_object.h>

BusTransactionInitiatorDispatcher::BusTransactionInitiatorDispatcher(fbl::RefPtr<Bti> bti)
    : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(BusTransactionInitiatorDispatcher,
                           kBusTransactionInitiatorDispatcherStateOffset);
  rust_bus_transaction_initiator_dispatcher_state_init(&opaque_storage_, this,
                                                       fbl::ExportToRawPtr(&bti));
}

IMPLEMENT_DISPATCHER_RUST_STATE(BusTransactionInitiatorDispatcher,
                                rust_bus_transaction_initiator_dispatcher_state_get_lock,
                                rust_bus_transaction_initiator_dispatcher_state_destroy)

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
  // quarantined pages). In the case that the driver-level BTI is in a fault
  // state, user-mode driver code is expected to take the steps to stop their
  // DMA, and then call `zx_bti_release_quarantine` before proceeding to pin new
  // memory.
  if (rust_bus_transaction_initiator_dispatcher_zero_handles_locked(this) ||
      bti().in_fault_state()) {
    return ZX_ERR_BAD_STATE;
  }

  return PinnedMemoryTokenDispatcher::Create(fbl::RefPtr(this), ktl::move(pinned_vmo), perms,
                                             pmt_handle, pmt_rights);
}

void BusTransactionInitiatorDispatcher::on_zero_handles() {
  rust_bus_transaction_initiator_dispatcher_on_zero_handles(this);
}

iommu::Bti& BusTransactionInitiatorDispatcher::bti() const {
  return *rust_bus_transaction_initiator_dispatcher_get_bti(this);
}
