// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/cpu.h>
#include <kernel/ffi.h>
#include <kernel/scheduler_state.h>
#include <kernel/thread_ffi.h>

extern "C" {

cpu_num_t cpp_scheduler_state_curr_cpu(const SchedulerState* state);
cpu_num_t cpp_scheduler_state_last_cpu(const SchedulerState* state);

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_num_t cpp_scheduler_state_curr_cpu(const SchedulerState* state) {
  return state->curr_cpu();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_num_t cpp_scheduler_state_last_cpu(const SchedulerState* state) {
  return state->last_cpu();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE cpu_mask_t cpp_scheduler_state_hard_affinity(const SchedulerState* state) {
  return state->hard_affinity();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_instant_mono_t cpp_scheduler_state_finish_time(const SchedulerState* state) {
  return state->finish_time().raw_value();
}
}  // extern "C"
