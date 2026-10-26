// Copyright 2016 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "object/event_dispatcher.h"

#include <zircon/types.h>

extern "C" {
void rust_event_dispatcher_state_init(void* state, void* disp);
void rust_event_dispatcher_state_destroy(void* state);
Lock<CriticalMutex>* rust_event_dispatcher_state_get_lock(const void* state);
void rust_memory_stall_event_dispatcher_state_init(void* state, uint32_t kind);
void rust_memory_stall_event_dispatcher_state_destroy(void* state);
}  // extern "C"

EventDispatcher::EventDispatcher() : Dispatcher(0u) {
  DISPATCHER_VERIFY_OFFSET(EventDispatcher, kEventDispatcherStateOffset);
  rust_event_dispatcher_state_init(&opaque_storage_, this);
}

IMPLEMENT_DISPATCHER_RUST_STATE(EventDispatcher, rust_event_dispatcher_state_get_lock,
                                rust_event_dispatcher_state_destroy)

zx_status_t EventDispatcher::user_signal_self(uint32_t clear_mask, uint32_t set_mask) {
  return UserSignalSelfSolo(this, clear_mask, set_mask, ZX_EVENT_SIGNALED);
}

MemoryStallEventDispatcher::MemoryStallEventDispatcher(uint32_t kind) : EventDispatcher() {
  DISPATCHER_VERIFY_OFFSET(MemoryStallEventDispatcher, kMemoryStallEventDispatcherStateOffset);
  rust_memory_stall_event_dispatcher_state_init(&opaque_storage_, kind);
}

MemoryStallEventDispatcher::~MemoryStallEventDispatcher() {
  rust_memory_stall_event_dispatcher_state_destroy(&opaque_storage_);
}
