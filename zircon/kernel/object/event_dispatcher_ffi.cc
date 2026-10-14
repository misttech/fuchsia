// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <object/event_dispatcher.h>

extern "C" {

zx_status_t cpp_event_dispatcher_create(
    uint32_t options, ffi::Uninitialized<KernelHandle<EventDispatcher>>* handle_out) {
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) EventDispatcher(options));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  handle_out->Initialize(ktl::move(disp));
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_event_dispatcher_get_mem_pressure_event(
    uint32_t kind, fbl::RefPtr<EventDispatcher>* out_event) {
  *out_event = GetMemPressureEvent(kind);
}

zx_status_t cpp_memory_stall_event_dispatcher_create(
    uint32_t kind, ffi::Uninitialized<KernelHandle<EventDispatcher>>* out_handle) {
  fbl::AllocChecker ac;
  auto disp = fbl::AdoptRef(new (&ac) MemoryStallEventDispatcher(kind));
  if (!ac.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  out_handle->Initialize(ktl::move(disp));
  return ZX_OK;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_event_dispatcher_user_signal_self(EventDispatcher* event,
                                                                    uint32_t clear_mask,
                                                                    uint32_t set_mask) {
  return event->user_signal_self(clear_mask, set_mask);
}

}  // extern "C"
