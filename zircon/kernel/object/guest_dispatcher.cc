// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/guest_dispatcher.h"

#include <lib/object-constants.h>

#include <arch/hypervisor.h>
#include <kernel/ffi.h>

GuestDispatcher::GuestDispatcher(ktl::unique_ptr<Guest> guest) : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(GuestDispatcher, kGuestDispatcherStateOffset);
  rust_guest_dispatcher_state_init(&opaque_storage_, this, guest.release());
}

IMPLEMENT_DISPATCHER_RUST_STATE(GuestDispatcher, rust_guest_dispatcher_state_get_lock,
                                rust_guest_dispatcher_state_destroy)
