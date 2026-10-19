// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/vm_object_dispatcher.h"

#include <lib/object-constants.h>
#include <zircon/errors.h>
#include <zircon/rights.h>
#include <zircon/types.h>

#include <fbl/alloc_checker.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <vm/vm_object.h>

extern "C" zx_status_t cpp_vm_object_dispatcher_create(
    VmObject* raw_vmo, StreamSizeManager* raw_ssm, uint32_t raw_initial_mutability,
    ffi::Uninitialized<KernelHandle<VmObjectDispatcher>>* out_handle) {
  fbl::RefPtr<VmObject> vmo = fbl::ImportFromRawPtr(raw_vmo);
  fbl::RefPtr<StreamSizeManager> ssm = fbl::ImportFromRawPtr(raw_ssm);
  auto initial_mutability =
      static_cast<VmObjectDispatcher::InitialMutability>(raw_initial_mutability);
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(
      new (&ac) VmObjectDispatcher(ktl::move(vmo), ktl::move(ssm), initial_mutability)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  out_handle->Initialize(ktl::move(new_handle));
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
extern "C" FFI_ALWAYS_INLINE VmObjectChildObserver* cpp_vm_object_dispatcher_as_child_observer(
    VmObjectDispatcher* disp) {
  return disp;
}

VmObjectDispatcher::VmObjectDispatcher(fbl::RefPtr<VmObject> vmo,
                                       fbl::RefPtr<StreamSizeManager> stream_size_manager,
                                       InitialMutability initial_mutability)
    : Dispatcher(ZX_VMO_ZERO_CHILDREN) {  // NOLINT(bugprone-signed-bitwise)
  DISPATCHER_VERIFY_OFFSET(VmObjectDispatcher, kVmObjectDispatcherStateOffset);
  rust_vm_object_dispatcher_state_init(&opaque_storage_, this, fbl::ExportToRawPtr(&vmo),
                                       fbl::ExportToRawPtr(&stream_size_manager),
                                       std::to_underlying(initial_mutability));
}

IMPLEMENT_DISPATCHER_RUST_STATE(VmObjectDispatcher, rust_vm_object_dispatcher_state_get_lock,
                                rust_vm_object_dispatcher_state_destroy)

zx_status_t VmObjectDispatcher::Create(fbl::RefPtr<VmObject> vmo, uint64_t stream_size,
                                       InitialMutability initial_mutability,
                                       KernelHandle<VmObjectDispatcher>* handle,
                                       zx_rights_t* rights) {
  ffi::Uninitialized<KernelHandle<VmObjectDispatcher>> uninit_handle;
  ffi::Uninitialized<zx_rights_t> uninit_rights;
  zx_status_t status = rust_vm_object_dispatcher_create(fbl::ExportToRawPtr(&vmo), stream_size,
                                                        std::to_underlying(initial_mutability),
                                                        &uninit_handle, &uninit_rights);
  if (status != ZX_OK) {
    return status;
  }
  *handle = ktl::move(uninit_handle.Get());
  *rights = uninit_rights.Get();
  return ZX_OK;
}

zx::result<fbl::RefPtr<StreamSizeManager>> VmObjectDispatcher::stream_size_manager() const {
  ffi::Uninitialized<fbl::RefPtr<StreamSizeManager>> uninit_ssm;
  zx_status_t status = rust_vm_object_dispatcher_stream_size_manager(this, &uninit_ssm);
  if (status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(ktl::move(uninit_ssm.Get()));
}

zx_status_t VmObjectDispatcher::CreateChild(uint32_t options, uint64_t offset, uint64_t size,
                                            bool copy_name,
                                            fbl::RefPtr<VmObject>* child_vmo) const {
  ffi::Uninitialized<fbl::RefPtr<VmObject>> uninit_child;
  zx_status_t status =
      rust_vm_object_dispatcher_create_child(this, options, offset, size, copy_name, &uninit_child);
  if (status != ZX_OK) {
    return status;
  }
  *child_vmo = ktl::move(uninit_child.Get());
  return ZX_OK;
}
