// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/power-management/pdev-power-level-controller.h"

#include <lib/power-management/kernel-registry.h>
#include <lib/power-management/power-state.h>

#include <dev/power.h>

namespace power_management {

zx::result<uint32_t> PDevPowerLevelController::Post(const PowerLevelUpdateRequest& pending) {
  const zx_status_t status = power_opp_set(pending.domain_id, pending.control_argument);
  if (status != ZX_OK) {
    return zx::error(status);
  }

  return KernelPowerDomainRegistry::UpdatePowerLevel(pending.domain_id, id(), control_interface(),
                                                     pending.control_argument);
}

zx::result<uint64_t> PDevPowerLevelController::GetCurrentPowerLevel(uint32_t domain_id) const {
  return power_opp_get(domain_id);
}

bool PDevPowerLevelController::IsSupported() {
  const zx::result<size_t> result = power_opp_get_domain_count();
  return result.is_ok() && result.value() > 0;
}

}  // namespace power_management
