// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/channel_dispatcher.h"

#include <zircon/errors.h>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <ktl/utility.h>
#include <object/handle.h>

#include <ktl/enforce.h>

extern "C" {
void rust_channel_dispatcher_state_init(void* state, void* holder);
void rust_channel_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_channel_dispatcher_state_get_lock(const void* state);

zx_status_t cpp_channel_dispatcher_create(
    void* holder, ffi::Uninitialized<KernelHandle<ChannelDispatcher>>* handle_out) {
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) ChannelDispatcher(holder));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }
  handle_out->Initialize(ktl::move(disp));
  return ZX_OK;
}
}  // extern "C"

ChannelDispatcher::ChannelDispatcher(void* holder) : Dispatcher(ZX_CHANNEL_WRITABLE) {
  DISPATCHER_VERIFY_OFFSET(ChannelDispatcher, kChannelDispatcherStateOffset);
  rust_channel_dispatcher_state_init(&opaque_storage_, holder);
}

IMPLEMENT_DISPATCHER_RUST_STATE(ChannelDispatcher, rust_channel_dispatcher_state_get_lock,
                                rust_channel_dispatcher_state_destroy)
