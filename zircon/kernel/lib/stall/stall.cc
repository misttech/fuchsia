// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/stall.h"

StallAccumulator::StallAccumulator() { rust_stall_accumulator_init(this); }

void StallAccumulator::Update(int op_contributors_progressing, int op_contributors_stalling) {
  rust_stall_accumulator_update(this, op_contributors_progressing, op_contributors_stalling);
}

void StallAccumulator::ApplyContextSwitch(Thread *current_thread, Thread *next_thread) {
  rust_stall_apply_context_switch(current_thread, next_thread);
}
