// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/types.h>

#include <arch/interrupt.h>
#include <arch/ops.h>

extern "C" {

bool cpp_ints_disabled();
void cpp_disable_ints();
void cpp_enable_ints();
interrupt_saved_state_t cpp_interrupt_save();
void cpp_interrupt_restore(interrupt_saved_state_t state);
cpu_num_t cpp_curr_cpu_num();
uint32_t cpp_max_num_cpus();

bool cpp_ints_disabled() { return arch_ints_disabled(); }
void cpp_disable_ints() { arch_disable_ints(); }
void cpp_enable_ints() { arch_enable_ints(); }
interrupt_saved_state_t cpp_interrupt_save() { return arch_interrupt_save(); }
void cpp_interrupt_restore(interrupt_saved_state_t state) {
  arch_interrupt_restore(state);
}
cpu_num_t cpp_curr_cpu_num() { return arch_curr_cpu_num(); }
uint32_t cpp_max_num_cpus() { return arch_max_num_cpus(); }

}  // extern "C"
