// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/ddk/platform-defs.h>
#include <lib/power-management/energy-model.h>
#include <lib/power-management/pdev-power-level-controller.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/arm64/periphmap.h>
#include <arch/arm64/smccc.h>
#include <dev/power/moonflower/init.h>
#include <dev/power/moonflower/moonflower_ffi.h>
#include <dev/psci.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <kernel/scheduler.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uintptr_t cpp_moonflower_get_opp_vaddr() {
  return reinterpret_cast<uintptr_t>(periph_paddr_to_vaddr(0xf521000));
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE int64_t cpp_moonflower_tz_config_hw_for_ram_dump(uint64_t disable_wd_dbg,
                                                                   uint64_t boot_partition_sel) {
  constexpr uint32_t kTzConfigHwForRamDumpFuncId = 0xc2000109;
  constexpr uint32_t kTzConfigHwForRamDumpParamId = 0x2;
  arm_smccc_result_t res = arm_smccc_smc(kTzConfigHwForRamDumpFuncId, kTzConfigHwForRamDumpParamId,
                                         disable_wd_dbg, boot_partition_sel, 0, 0, 0, 0);
  return static_cast<int64_t>(res.x0);
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE int64_t cpp_moonflower_tz_io_write(paddr_t paddr, uint32_t val) {
  constexpr uint32_t kTzIoAccessWriteFuncId = 0xc2000502;
  constexpr uint32_t kTzIoAccessWriteParamId = 0x2;
  arm_smccc_result_t res =
      arm_smccc_smc(kTzIoAccessWriteFuncId, kTzIoAccessWriteParamId, paddr, val, 0, 0, 0, 0);
  return static_cast<int64_t>(res.x0);
}

}  // extern "C"

void moonflower_power_init() {
  dprintf(INFO, "POWER: initializing moonflower power domain\n");

  constexpr power_management::ProcessorPowerLevel kPowerLevels[] = {
      {
          .options = power_management::kPowerLevelOptionsDomainIndependent,
          .processing_rate = 0,
          .power_coefficient_nw = 100'000,
          .control_interface = power_management::ControlInterface::kArmWfi,
          .control_argument = 0,
          .diagnostic_name = "WFI",
      },
      {
          .options = 0,
          .processing_rate = 360,
          .power_coefficient_nw = 20'000'000,
          .control_interface = power_management::ControlInterface::kCpuDriver,
          .control_argument = 3,
          .diagnostic_name = "LowSVS",
      },
      {
          .options = 0,
          .processing_rate = 506,
          .power_coefficient_nw = 31'000'000,
          .control_interface = power_management::ControlInterface::kCpuDriver,
          .control_argument = 2,
          .diagnostic_name = "SVS",
      },
      {
          .options = 0,
          .processing_rate = 798,
          .power_coefficient_nw = 66'000'000,
          .control_interface = power_management::ControlInterface::kCpuDriver,
          .control_argument = 1,
          .diagnostic_name = "Nominal",
      },
      {
          .options = 0,
          .processing_rate = 1000,
          .power_coefficient_nw = 102'000'000,
          .control_interface = power_management::ControlInterface::kCpuDriver,
          .control_argument = 0,
          .diagnostic_name = "Turbo",
      },
  };

  constexpr uint32_t kDomainId = 0;

  auto energy_model_result = power_management::EnergyModel::Create(kPowerLevels, {});
  if (energy_model_result.is_error()) {
    dprintf(CRITICAL, "POWER: Failed to create energy model: %d\n",
            energy_model_result.status_value());
    return;
  }

  auto controller_result = power_management::PDevPowerLevelController::Get(kDomainId);
  if (controller_result.is_error()) {
    dprintf(CRITICAL, "POWER: Failed to get PDevPowerLevelController for domain %u: %d\n",
            kDomainId, controller_result.status_value());
    return;
  }
  auto controller = std::move(controller_result).value();

  const cpu_mask_t domain_cpus_mask = 0xf;

  fbl::AllocChecker ac;
  auto domain = fbl::MakeRefCountedChecked<power_management::PowerDomain>(
      &ac, kDomainId, domain_cpus_mask, std::move(energy_model_result).value(), controller);
  if (!ac.check()) {
    dprintf(CRITICAL, "POWER: Failed to allocate PowerDomain\n");
    return;
  }

  power_management::PowerDomainSet domain_set;
  auto register_result = domain_set.Add(std::move(domain));
  if (register_result.is_error()) {
    dprintf(CRITICAL, "POWER: Failed to add power domain to set: %d\n",
            register_result.status_value());
  } else {
    Scheduler::SetPowerDomainSet(std::move(domain_set));
    dprintf(INFO, "POWER: Registered moonflower power domain\n");
  }
}
