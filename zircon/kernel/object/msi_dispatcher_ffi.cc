// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <zircon/errors.h>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <object/handle.h>
#include <object/msi_dispatcher.h>

MsiDispatcher::MsiDispatcher(MsiAllocation* msi_alloc) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(MsiDispatcher, kMsiDispatcherStateOffset);
  rust_msi_dispatcher_state_init(&opaque_storage_, this, msi_alloc);
}

IMPLEMENT_DISPATCHER_RUST_STATE(MsiDispatcher, rust_msi_dispatcher_state_get_lock,
                                rust_msi_dispatcher_state_destroy)

extern "C" {

zx_status_t cpp_msi_dispatcher_create(MsiAllocation* msi_alloc_raw,
                                      ffi::Uninitialized<KernelHandle<MsiDispatcher>>* handle_out) {
  fbl::AllocChecker ac;
  KernelHandle new_handle(fbl::AdoptRef(new (&ac) MsiDispatcher(msi_alloc_raw)));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  handle_out->Initialize(ktl::move(new_handle));
  return ZX_OK;
}

}  // extern "C"
