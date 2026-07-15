// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/counter_dispatcher.h"

#include <zircon/errors.h>
#include <zircon/rights.h>

#include <new>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <ktl/utility.h>
#include <object/handle.h>
#include <object/process_dispatcher.h>

#include <ktl/enforce.h>

extern "C" {
void rust_counter_dispatcher_state_init(void* state, void* disp);
void rust_counter_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_counter_dispatcher_state_get_lock(const void* state);

}  // extern "C"

CounterDispatcher::CounterDispatcher() : Dispatcher(0u) {
  // We suppress -Winvalid-offsetof because CounterDispatcher inherits from Dispatcher and has
  // virtual functions, making it a non-standard-layout class. While offsetof on
  // non-standard-layout types is conditionally supported in C++, in our compiler and ABI the
  // layout is deterministic, and we must validate the exact byte offset of opaque_storage_ for
  // direct pointer arithmetic in Rust FFI.
#pragma GCC diagnostic push
#pragma GCC diagnostic ignored "-Winvalid-offsetof"
  static_assert(
      offsetof(CounterDispatcher, opaque_storage_) == kCounterDispatcherStateOffset,
      "kCounterDispatcherStateOffset must match offsetof(CounterDispatcher, opaque_storage_)");
#pragma GCC diagnostic pop
  rust_counter_dispatcher_state_init(&opaque_storage_, this);
}

CounterDispatcher::~CounterDispatcher() { rust_counter_dispatcher_state_destroy(&opaque_storage_); }

Lock<CriticalMutex>* CounterDispatcher::get_lock() const {
  return rust_counter_dispatcher_state_get_lock(&opaque_storage_);
}

zx_status_t CounterDispatcher::user_signal_self(uint32_t clear_mask, uint32_t set_mask) {
  const zx_signals_t kAllowedSignals = ZX_USER_SIGNAL_ALL | ZX_COUNTER_SIGNALED;
  if ((set_mask & ~kAllowedSignals) || (clear_mask & ~kAllowedSignals)) {
    return ZX_ERR_INVALID_ARGS;
  }
  UpdateState(clear_mask, set_mask);
  return ZX_OK;
}
