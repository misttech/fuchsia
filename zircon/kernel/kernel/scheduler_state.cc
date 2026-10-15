// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <kernel/scheduler.h>
#include <kernel/scheduler_state.h>
#include <ktl/algorithm.h>

void SchedulerState::RecomputeEffectiveProfile() {
  rust_kernel_scheduler_state_recompute_effective_profile(this);
}

int32_t SchedulerState::EffectiveProfile::GetWeightOrPackedDeadlineParams() {
  return rust_kernel_effective_profile_get_weight_or_packed_deadline_params(this);
}
