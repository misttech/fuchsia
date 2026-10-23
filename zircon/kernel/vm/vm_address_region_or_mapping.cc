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
  return rust_vm_address_region_or_mapping_destroy(this);
}

VmAddressRegionOrMapping::~VmAddressRegionOrMapping() {
  rust_vm_address_region_or_mapping_destruct(this);
}

zx_status_t VmAddressRegionOrMapping::DestroyLocked() {
  return rust_vm_address_region_or_mapping_destroy_locked(this);
}

zx_status_t VmAddressRegionOrMapping::Activate() {
  return rust_vm_address_region_or_mapping_activate(this);
}

void VmAddressRegionOrMapping::DumpLocked(uint depth, bool verbose) const {
  rust_vm_address_region_or_mapping_dump_locked(this, depth, verbose);
}

void VmAddressRegionOrMapping::CommitHighMemoryPriority() {
  rust_vm_address_region_or_mapping_commit_high_memory_priority(this);
}

void VmAddressRegionOrMapping::fbl_recycle() { rust_vm_address_region_or_mapping_recycle(this); }
