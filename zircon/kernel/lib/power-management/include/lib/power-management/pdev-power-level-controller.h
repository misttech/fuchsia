// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PDEV_POWER_LEVEL_CONTROLLER_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PDEV_POWER_LEVEL_CONTROLLER_H_

#include <lib/power-management/power-level-controller.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/types.h>

namespace power_management {

class PDevPowerLevelController final : public PowerLevelController {
 public:
  PDevPowerLevelController() : PowerLevelController(ControlInterface::kCpuDriver) {}
  ~PDevPowerLevelController() final = default;

  zx::result<uint32_t> Post(const PowerLevelUpdateRequest& pending) final;

  zx::result<uint64_t> GetCurrentPowerLevel(uint32_t domain_id) const final;

  // Return a koid that will never collide with a valid dispatcher koid (i.e.
  // the PortDispatcher registered when setting up the domain).
  //
  // IMPORTANT: This prevents userspace from updating the active power level
  // bookkeeping when this power level controller is being used, since this id
  // is compared when handling zx_system_set_processor_power_state. It should be
  // impossible for userspace to supply a port object with a koid that matches
  // this id.
  uint64_t id() const final { return ZX_KOID_INVALID; }

  bool is_fast_path() const final { return true; }

  static bool IsSupported();
};

}  // namespace power_management

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PORT_POWER_LEVEL_CONTROLLER_H_
