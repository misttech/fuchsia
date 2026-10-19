// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#ifndef ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PDEV_POWER_LEVEL_CONTROLLER_H_
#define ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PDEV_POWER_LEVEL_CONTROLLER_H_

#include <lib/power-management/power-level-controller.h>
#include <lib/power-management/power-state.h>
#include <lib/zx/result.h>
#include <zircon/assert.h>
#include <zircon/types.h>

#include <utility>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>

namespace power_management {

struct RustPDevPowerLevelController;

extern "C" {

void rust_pdev_power_level_controller_reset_for_test();
zx_status_t rust_pdev_power_level_controller_get(
    uint32_t domain_id, const RustPDevPowerLevelController** out_controller);
void rust_pdev_power_level_controller_release(const RustPDevPowerLevelController* controller);
zx_status_t rust_pdev_power_level_controller_post(const RustPDevPowerLevelController* controller,
                                                  const PowerLevelUpdateRequest* pending,
                                                  uint32_t* out_cpu_mask);
zx_status_t rust_pdev_power_level_controller_get_current_power_level(
    const RustPDevPowerLevelController* controller, uint32_t domain_id, uint64_t* out_level);
size_t rust_pdev_power_level_controller_domain_count(
    const RustPDevPowerLevelController* controller);
bool rust_pdev_power_level_controller_is_supported();

}  // extern "C"

class PDevPowerLevelController final : public PowerLevelController {
  enum PrivateConstructorTag : bool { PrivateConstructorValue };

 public:
  static zx::result<fbl::RefPtr<PDevPowerLevelController>> Get(uint32_t domain_id) {
    const RustPDevPowerLevelController* inner = nullptr;
    const zx_status_t status = rust_pdev_power_level_controller_get(domain_id, &inner);
    if (status != ZX_OK) {
      return zx::error(status);
    }

    fbl::AllocChecker ac;
    fbl::RefPtr<PDevPowerLevelController> instance =
        fbl::MakeRefCountedChecked<PDevPowerLevelController>(&ac, PrivateConstructorValue, inner);
    if (!ac.check()) {
      rust_pdev_power_level_controller_release(inner);
      return zx::error(ZX_ERR_NO_MEMORY);
    }

    return zx::ok(std::move(instance));
  }

  static void ResetForTest() { rust_pdev_power_level_controller_reset_for_test(); }

  explicit PDevPowerLevelController(PrivateConstructorTag tag,
                                    const RustPDevPowerLevelController* inner)
      : PowerLevelController(ControlInterface::kCpuDriver), inner_{inner} {}

  ~PDevPowerLevelController() final { rust_pdev_power_level_controller_release(inner_); }

  zx::result<uint32_t> Post(const PowerLevelUpdateRequest& pending) final {
    uint32_t cpu_mask = 0;
    const zx_status_t status = rust_pdev_power_level_controller_post(inner_, &pending, &cpu_mask);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(cpu_mask);
  }

  zx::result<uint64_t> GetCurrentPowerLevel(uint32_t domain_id) const final {
    uint64_t level = 0;
    const zx_status_t status =
        rust_pdev_power_level_controller_get_current_power_level(inner_, domain_id, &level);
    if (status != ZX_OK) {
      return zx::error(status);
    }
    return zx::ok(level);
  }

  // Return a koid that will never collide with a valid dispatcher koid (i.e.
  // the PortDispatcher registered when setting up the domain).
  //
  // IMPORTANT: This prevents userspace from updating the active power level
  // bookkeeping when this power level controller is being used, since this id
  // is compared when handling zx_system_set_processor_power_state. It should be
  // impossible for userspace to supply a port object with a koid that matches
  // this id.
  uint64_t id() const final { return ZX_KOID_INVALID; }

  static bool IsSupported() { return rust_pdev_power_level_controller_is_supported(); }

  size_t domain_count() const { return rust_pdev_power_level_controller_domain_count(inner_); }

 private:
  const RustPDevPowerLevelController* const inner_;
};

#ifdef UNITTESTS_ENABLED
extern "C" {
void cpp_save_and_clear_power_domains_for_test();
void cpp_restore_power_domains_for_test();
}  // extern "C"
#endif

}  // namespace power_management

#endif  // ZIRCON_KERNEL_LIB_POWER_MANAGEMENT_INCLUDE_LIB_POWER_MANAGEMENT_PDEV_POWER_LEVEL_CONTROLLER_H_
