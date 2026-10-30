// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2009 Corey Tabaka
// Copyright (c) 2015 Intel Corporation
// Copyright (c) 2016 Travis Geiselbrecht
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <platform.h>
#include <zircon/types.h>

#include <dev/power.h>
#include <kernel/cpu.h>

#include <ktl/enforce.h>

extern "C" {

void rust_platform_early_init();
void rust_platform_prevm_init();
void rust_platform_init();
zx_status_t rust_platform_get_cpu_state(cpu_num_t cpu_id, power_cpu_state* out_state);

}  // extern "C"

void platform_early_init() { rust_platform_early_init(); }

void platform_prevm_init() { rust_platform_prevm_init(); }

void platform_init() { rust_platform_init(); }

zx::result<power_cpu_state> platform_get_cpu_state(cpu_num_t cpu_id) {
  power_cpu_state state{};
  if (zx_status_t status = rust_platform_get_cpu_state(cpu_id, &state); status != ZX_OK) {
    return zx::error(status);
  }
  return zx::ok(state);
}
