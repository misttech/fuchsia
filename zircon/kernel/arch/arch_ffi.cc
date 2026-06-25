// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <sys/types.h>

#include <arch/ops.h>

extern "C" {

void cpp_enable_ints();
void cpp_disable_ints();
bool cpp_ints_disabled();
cpu_num_t cpp_curr_cpu_num();
uint32_t cpp_max_num_cpus();

void cpp_enable_ints() { arch_enable_ints(); }
void cpp_disable_ints() { arch_disable_ints(); }
bool cpp_ints_disabled() { return arch_ints_disabled(); }
cpu_num_t cpp_curr_cpu_num() { return arch_curr_cpu_num(); }
uint32_t cpp_max_num_cpus() { return arch_max_num_cpus(); }

}  // extern "C"
