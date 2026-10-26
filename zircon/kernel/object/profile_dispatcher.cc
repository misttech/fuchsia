// Copyright 2018 The Fuchsia Authors
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
#include <object/profile_dispatcher.h>

#include <ktl/enforce.h>

extern "C" {
void rust_profile_dispatcher_state_init(void* state, void* disp, void* parsed_info);
void rust_profile_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_profile_dispatcher_state_get_lock(const void* state);

zx_status_t cpp_profile_dispatcher_create(
    void* parsed_info, ffi::Uninitialized<KernelHandle<ProfileDispatcher>>* handle_out) {
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) ProfileDispatcher(parsed_info));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  handle_out->Initialize(ktl::move(disp));
  return ZX_OK;
}
}

ProfileDispatcher::ProfileDispatcher(void* parsed_info) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(ProfileDispatcher, kProfileDispatcherStateOffset);
  rust_profile_dispatcher_state_init(&opaque_storage_, this, parsed_info);
}

IMPLEMENT_DISPATCHER_RUST_STATE(ProfileDispatcher, rust_profile_dispatcher_state_get_lock,
                                rust_profile_dispatcher_state_destroy)
