// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

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

void* cpp_dispatcher_get_ref_counted(const Dispatcher* disp) {
  return disp->get_ref_counted_base();
}

zx_obj_type_t cpp_dispatcher_get_type(const Dispatcher* disp) { return disp->get_type(); }

void cpp_dispatcher_recycle(Dispatcher* disp) {
  fbl::internal::recycler<Dispatcher>::recycle(disp);
}

}  // extern "C"
