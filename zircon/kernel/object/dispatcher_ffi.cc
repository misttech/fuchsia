// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <object/dispatcher.h>

extern "C" {

void cpp_dispatcher_on_zero_handles(Dispatcher* disp) { disp->on_zero_handles(); }

void cpp_dispatcher_update_state(Dispatcher* disp, zx_signals_t clear_mask, zx_signals_t set_mask) {
  disp->UpdateState(clear_mask, set_mask);
}

void cpp_dispatcher_update_state_locked(Dispatcher* disp, zx_signals_t clear_mask,
                                        zx_signals_t set_mask) TA_NO_THREAD_SAFETY_ANALYSIS {
  disp->UpdateStateLocked(clear_mask, set_mask);
}

zx_signals_t cpp_dispatcher_signals_state_locked(const Dispatcher* disp)
    TA_NO_THREAD_SAFETY_ANALYSIS {
  return disp->GetSignalsStateLocked();
}

void* cpp_dispatcher_get_ref_counted(const Dispatcher* disp) {
  return disp->get_ref_counted_base();
}

zx_obj_type_t cpp_dispatcher_get_type(const Dispatcher* disp) { return disp->get_type(); }

zx_koid_t cpp_dispatcher_get_koid(const Dispatcher* disp) { return disp->get_koid(); }

void cpp_dispatcher_recycle(Dispatcher* disp) {
  fbl::internal::recycler<Dispatcher>::recycle(disp);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_koid_t cpp_dispatcher_get_related_koid(const Dispatcher* disp) {
  return disp->get_related_koid();
}

}  // extern "C"
