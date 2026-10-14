// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/ffi.h>
#include <kernel/idle_power_thread.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE wake_vector::WakeResult cpp_idle_power_thread_trigger_system_wake_event() {
  return IdlePowerThread::TriggerSystemWakeEvent();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE void cpp_idle_power_thread_acknowledge_system_wake_event() {
  IdlePowerThread::AcknowledgeSystemWakeEvent();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_instant_boot_t
cpp_idle_power_thread_transition_all_active_to_suspend(zx_instant_boot_t resume_deadline) {
  return IdlePowerThread::TransitionAllActiveToSuspend(resume_deadline);
}

}  // extern "C"
