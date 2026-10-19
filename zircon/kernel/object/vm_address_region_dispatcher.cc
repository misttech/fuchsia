// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/vm_address_region_dispatcher.h"

#include <lib/object-constants.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <vm/vm_address_region.h>

extern "C" zx_status_t cpp_vmar_dispatcher_create(
    VmAddressRegion* vmar, arch_mmu_flags_t base_arch_mmu_flags,
    ffi::Uninitialized<KernelHandle<VmAddressRegionDispatcher>>* handle_out) {
  fbl::RefPtr<VmAddressRegion> vmar_ref = fbl::ImportFromRawPtr(vmar);
  fbl::AllocChecker ac;
  KernelHandle new_handle(
      fbl::AdoptRef(new (&ac) VmAddressRegionDispatcher(ktl::move(vmar_ref), base_arch_mmu_flags)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  handle_out->Initialize(ktl::move(new_handle));
  return ZX_OK;
}

VmAddressRegionDispatcher::VmAddressRegionDispatcher(fbl::RefPtr<VmAddressRegion> vmar,
                                                     arch_mmu_flags_t base_arch_mmu_flags)
    : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(VmAddressRegionDispatcher, kVmAddressRegionDispatcherStateOffset);
  rust_vm_address_region_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&vmar),
                                               base_arch_mmu_flags);
}

IMPLEMENT_DISPATCHER_RUST_STATE(VmAddressRegionDispatcher,
                                rust_vm_address_region_dispatcher_state_get_lock,
                                rust_vm_address_region_dispatcher_state_destroy)

zx_status_t VmAddressRegionDispatcher::Create(fbl::RefPtr<VmAddressRegion> vmar,
                                              arch_mmu_flags_t base_arch_mmu_flags,
                                              KernelHandle<VmAddressRegionDispatcher>* handle,
                                              zx_rights_t* rights) {
  ffi::Uninitialized<KernelHandle<VmAddressRegionDispatcher>> uninit_handle;
  ffi::Uninitialized<zx_rights_t> uninit_rights;
  zx_status_t status = rust_vmar_dispatcher_create(fbl::ExportToRawPtr(&vmar), base_arch_mmu_flags,
                                                   &uninit_handle, &uninit_rights);
  if (status != ZX_OK) {
    return status;
  }
  *handle = ktl::move(uninit_handle.Get());
  *rights = uninit_rights.Get();
  return ZX_OK;
}
