// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/power-management/power-level-controller.h>
#include <zircon/types.h>

#include <kernel/cpu.h>
#include <kernel/ffi.h>
#include <kernel/scheduler.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_scheduler_update_power_level(
    uint32_t domain_id, uint64_t controller_id, power_management::ControlInterface interface,
    uint64_t arg, cpu_mask_t* out_mask) {
  zx::result<cpu_mask_t> result =
      Scheduler::UpdatePowerLevel(domain_id, controller_id, interface, arg);
  if (result.is_error()) {
    return result.error_value();
  }
  *out_mask = result.value();
  return ZX_OK;
}

}  // extern "C"
