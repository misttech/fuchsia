// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/pinned_memory_token_dispatcher.h"

#include <align.h>
#include <assert.h>
#include <lib/counters.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <new>

#include <ktl/algorithm.h>
#include <ktl/bit.h>
#include <object/bus_transaction_initiator_dispatcher.h>
#include <vm/pinned_vm_object.h>
#include <vm/vm.h>
#include <vm/vm_object.h>

#include <ktl/enforce.h>

#define LOCAL_TRACE 0

KCOUNTER(dispatcher_pinned_memory_token_create_count, "dispatcher.pinned_memory_token.create")
KCOUNTER(dispatcher_pinned_memory_token_destroy_count, "dispatcher.pinned_memory_token.destroy")

zx_status_t PinnedMemoryTokenDispatcher::Create(fbl::RefPtr<BusTransactionInitiatorDispatcher> bti,
                                                PinnedVmObject pinned_vmo, uint32_t perms,
                                                KernelHandle<PinnedMemoryTokenDispatcher>* handle,
                                                zx_rights_t* rights) {
  LTRACE_ENTRY;
  DEBUG_ASSERT(IsPageRounded(pinned_vmo.offset()) && IsPageRounded(pinned_vmo.size()));
  DEBUG_ASSERT(pinned_vmo.vmo() != nullptr);

  // Note: Do not move our BTI reference into the PMT dispatcher we are
  // creating.  Instead, give the new PMT dispatcher its own reference.  We
  // still need to hold onto our reference for now so that we can access the
  // underlying driver BTI instance to perform the map operation.
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) PinnedMemoryTokenDispatcher(bti)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  // TODO(b/502262026) : It feels like whether or not we require a contiguous
  // mapping should come as a flag from user mode, not as an expectation based
  // on whether or not the underlying VMO is actually physically contiguous.
  const bool contiguous_vmo = pinned_vmo.vmo()->is_contiguous();
  zx::result<fbl::RefPtr<iommu::Pmt>> maybe_pmt = bti->bti().Map(
      ktl::move(pinned_vmo), perms,
      contiguous_vmo ? iommu::RequireContiguousMapping::Yes : iommu::RequireContiguousMapping::No);
  if (!maybe_pmt.is_ok()) {
    return maybe_pmt.error_value();
  }

  {
    Guard<CriticalMutex> guard{new_handle.dispatcher()->get_lock()};
    new_handle.dispatcher()->pmt_ = ktl::move(maybe_pmt.value());
  }

  *handle = ktl::move(new_handle);
  *rights = default_rights();
  return ZX_OK;
}

void PinnedMemoryTokenDispatcher::on_zero_handles() {
  Guard<CriticalMutex> guard{get_lock()};
  // We may not have a driver level PMT if we never fully constructed
  // successfully.  In this case, we will be dropping the kernel handle to the
  // PMT Dispatcher without ever having assigned an iommu::Pmt to it.
  if (pmt_ != nullptr) {
    pmt_->OnDispatcherZeroHandles();
  }
}

PinnedMemoryTokenDispatcher::~PinnedMemoryTokenDispatcher() {
  kcounter_add(dispatcher_pinned_memory_token_destroy_count, 1);
}

PinnedMemoryTokenDispatcher::PinnedMemoryTokenDispatcher(
    fbl::RefPtr<BusTransactionInitiatorDispatcher> bti)
    : bti_(ktl::move(bti)) {
  kcounter_add(dispatcher_pinned_memory_token_create_count, 1);
}
