// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <align.h>
#include <lib/fit/defer.h>
#include <lib/page/size.h>
#include <lib/user_copy/user_ptr.h>
#include <platform.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <trace.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/syscalls/iommu.h>
#include <zircon/syscalls/smc.h>
#include <zircon/types.h>

#include <new>

#include <dev/interrupt.h>
#include <dev/iommu/iommu.h>
#include <fbl/inline_array.h>
#include <object/bus_transaction_initiator_dispatcher.h>
#include <object/handle.h>
#include <object/iommu_dispatcher.h>
#include <object/process_dispatcher.h>
#include <object/resource.h>
#include <object/vcpu_dispatcher.h>
#include <object/vm_object_dispatcher.h>
#include <vm/vm.h>
#include <vm/vm_object_paged.h>
#include <vm/vm_object_physical.h>

#if ARCH_X86
#include <platform/pc/smbios.h>
#endif

#include <lib/syscalls/forward.h>

#define LOCAL_TRACE 0

// zx_status_t zx_vmo_create_contiguous
zx_status_t sys_vmo_create_contiguous(zx_handle_t bti, size_t size, uint32_t alignment_log2,
                                      zx_handle_t* out) {
  LTRACEF("size 0x%zu\n", size);

  if (size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (!IsPageRounded(size)) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (alignment_log2 == 0) {
    alignment_log2 = kPageShift;
  }
  // catch obviously wrong values
  if (alignment_log2 < kPageShift || alignment_log2 >= (8 * sizeof(uint64_t))) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto up = ProcessDispatcher::GetCurrent();
  zx_status_t status = up->EnforceBasicPolicy(ZX_POL_NEW_VMO);
  if (status != ZX_OK) {
    return status;
  }

  fbl::RefPtr<BusTransactionInitiatorDispatcher> bti_dispatcher;
  status = up->handle_table().GetDispatcherWithRights(*up, bti, ZX_RIGHT_MAP, &bti_dispatcher);
  if (status != ZX_OK) {
    return status;
  }

  auto align_log2_arg = static_cast<uint8_t>(alignment_log2);

  // create a vm object
  fbl::RefPtr<VmObjectPaged> vmo;
  status = VmObjectPaged::CreateContiguous(PMM_ALLOC_FLAG_ANY, size, align_log2_arg, &vmo);
  if (status != ZX_OK) {
    return status;
  }

  // create a Vm Object dispatcher
  KernelHandle<VmObjectDispatcher> kernel_handle;
  zx_rights_t rights;
  status = VmObjectDispatcher::Create(ktl::move(vmo), size,
                                      VmObjectDispatcher::InitialMutability::kMutable,
                                      &kernel_handle, &rights);
  if (status != ZX_OK) {
    return status;
  }

  // create a handle and attach the dispatcher to it
  return up->MakeAndAddHandle(ktl::move(kernel_handle), rights, out);
}

// zx_status_t zx_vmo_create_physical
zx_status_t sys_vmo_create_physical(zx_handle_t hrsrc, zx_paddr_t paddr, size_t size,
                                    zx_handle_t* out) {
  LTRACEF("size 0x%zu\n", size);

  if (size == 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  if (!IsPageRounded(size)) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto up = ProcessDispatcher::GetCurrent();
  zx_status_t status = up->EnforceBasicPolicy(ZX_POL_NEW_VMO);
  if (status != ZX_OK) {
    return status;
  }

  // Memory should be subtracted from the PhysicalAspace allocators, so it's
  // safe to assume that if the caller has access to a resource for this specified
  // region of MMIO space then it is safe to allow the vmo to be created.
  if ((status = validate_resource_mmio(hrsrc, paddr, size)) != ZX_OK) {
    return status;
  }

  // create a vm object
  fbl::RefPtr<VmObjectPhysical> vmo;
  status = VmObjectPhysical::Create(paddr, size, &vmo);
  if (status != ZX_OK) {
    return status;
  }

  // create a Vm Object dispatcher
  KernelHandle<VmObjectDispatcher> kernel_handle;
  zx_rights_t rights;
  status = VmObjectDispatcher::Create(ktl::move(vmo), size,
                                      VmObjectDispatcher::InitialMutability::kMutable,
                                      &kernel_handle, &rights);
  if (status != ZX_OK) {
    return status;
  }

  // create a handle and attach the dispatcher to it
  return up->MakeAndAddHandle(ktl::move(kernel_handle), rights, out);
}

#if ARCH_X86
#include <arch/x86/descriptor.h>
#include <arch/x86/ioport.h>

// zx_status_t zx_ioports_request
zx_status_t sys_ioports_request(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  zx_status_t status;
  if ((status = validate_resource_ioport(hrsrc, io_addr, len)) != ZX_OK) {
    return status;
  }

  LTRACEF("addr 0x%x len 0x%x\n", io_addr, len);

  return IoBitmap::GetCurrent()->SetIoBitmap(io_addr, len, /*enable=*/true);
}

// zx_status_t zx_ioports_release
zx_status_t sys_ioports_release(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  zx_status_t status;
  if ((status = validate_resource_ioport(hrsrc, io_addr, len)) != ZX_OK) {
    return status;
  }

  LTRACEF("addr 0x%x len 0x%x\n", io_addr, len);

  return IoBitmap::GetCurrent()->SetIoBitmap(io_addr, len, /*enable=*/false);
}

#else
// zx_status_t zx_ioports_request
zx_status_t sys_ioports_request(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  // doesn't make sense on non-x86
  return ZX_ERR_NOT_SUPPORTED;
}

// zx_status_t zx_ioports_release
zx_status_t sys_ioports_release(zx_handle_t hrsrc, uint16_t io_addr, uint32_t len) {
  return ZX_ERR_NOT_SUPPORTED;
}
#endif
