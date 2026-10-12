// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include <assert.h>
#include <inttypes.h>
#include <stddef.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <vm/vm.h>
#include <vm/vm_aspace.h>
#include <vm/vm_constants.h>

#include "vm/vm_address_region.h"
#include "vm_priv.h"

#define LOCAL_TRACE VM_GLOBAL_TRACE(0)

VmAddressRegionOrMapping::VmAddressRegionOrMapping(vaddr_t base, size_t size, uint32_t flags,
                                                   VmAspace* aspace, VmAddressRegion* parent,
                                                   bool is_mapping) {
  rust_vm_address_region_or_mapping_state_init(&opaque_storage_, base, size, flags, aspace, parent,
                                               is_mapping);
  LTRACEF("%p\n", this);
}

zx_status_t VmAddressRegionOrMapping::Destroy() {
  AssertRegionOrMappingCanary();
  Guard<CriticalMutex> region_guard{aspace()->region_lock()};
  Guard<CriticalMutex> guard{aspace()->lock()};
  if (state_locked() != LifeCycleState::ALIVE) {
    return ZX_ERR_BAD_STATE;
  }

  return DestroyLocked();
}

VmAddressRegionOrMapping::~VmAddressRegionOrMapping() {
  LTRACEF("%p\n", this);

  ASSERT([&]() TA_NO_THREAD_SAFETY_ANALYSIS { return state_locked() != LifeCycleState::ALIVE; }());

  DEBUG_ASSERT([&]() TA_NO_THREAD_SAFETY_ANALYSIS {
    return memory_priority_locked() == MemoryPriority::DEFAULT;
  }());

  rust_vm_address_region_or_mapping_destruct(&opaque_storage_);
}

zx_status_t VmAddressRegionOrMapping::DestroyLocked() {
  if (is_mapping()) {
    return static_cast<VmMapping*>(this)->DestroyLockedImpl();
  }
  return static_cast<VmAddressRegion*>(this)->DestroyLockedImpl();
}

zx_status_t VmAddressRegionOrMapping::Activate() {
  if (is_mapping()) {
    return static_cast<VmMapping*>(this)->ActivateImpl();
  }
  return static_cast<VmAddressRegion*>(this)->ActivateImpl();
}

void VmAddressRegionOrMapping::DumpLocked(uint depth, bool verbose) const {
  if (is_mapping()) {
    static_cast<const VmMapping*>(this)->DumpLockedImpl(depth, verbose);
  } else {
    static_cast<const VmAddressRegion*>(this)->DumpLockedImpl(depth, verbose);
  }
}

void VmAddressRegionOrMapping::CommitHighMemoryPriority() {
  if (is_mapping()) {
    static_cast<VmMapping*>(this)->CommitHighMemoryPriorityImpl();
  } else {
    static_cast<VmAddressRegion*>(this)->CommitHighMemoryPriorityImpl();
  }
}

void VmAddressRegionOrMapping::fbl_recycle() {
  if (is_mapping()) {
    delete static_cast<VmMapping*>(this);
  } else {
    delete static_cast<VmAddressRegion*>(this);
  }
}
