// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <debug.h>
#include <lib/power-management/energy-model.h>
#include <lib/power-management/pdev-power-level-controller.h>
#include <zircon/errors.h>
#include <zircon/types.h>

#include <arch/arm64/periphmap.h>
#include <dev/power/iris/init.h>
#include <fbl/alloc_checker.h>
#include <fbl/array.h>
#include <fbl/ref_ptr.h>
#include <kernel/ffi.h>
#include <kernel/scheduler.h>
#include <ktl/algorithm.h>
#include <ktl/span.h>

extern "C" {

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uintptr_t cpp_iris_get_opp_vaddr() {
  return reinterpret_cast<uintptr_t>(periph_paddr_to_vaddr(0x200c0790));
}

}  // extern "C"

void iris_power_init() {
  if (!IRIS_REGISTER_ENERGY_MODEL) {
    dprintf(INFO, "POWER: Iris energy model registration disabled\n");
    return;
  }

  dprintf(INFO, "POWER: initializing iris power domains\n");

  constexpr uint32_t kFrequencyLittle[] = {
      2246400, 2169600, 2092800, 2054400, 2016000, 1996800, 1881600, 1766400,
      1632000, 1555200, 1459200, 1363200, 1286400, 1190400, 1036800, 883200,
      729600,  533000,  460800,  422400,  345600,  268800,
  };
  constexpr uint32_t kFrequencyMedium[] = {
      3052800, 2937600, 2841600, 2688000, 2534400, 2400000, 2284800, 2188800,
      2092800, 1939200, 1862400, 1785600, 1670400, 1536000, 1401600, 1267200,
      1075200, 921600,  729600,  652800,  533000,  400000,  266500,  177600,
  };
  constexpr uint32_t kFrequencyBig[] = {
      3782400, 3590400, 3398400, 3168000, 2937600, 2707200, 2592000, 2457600,
      2342400, 2208000, 2073600, 1920000, 1766400, 1593600, 1420800, 1305600,
      1152000, 1036800, 883200,  800000,  533000,  400000,  266500,
  };

  struct DomainConfig {
    uint32_t domain_id;
    cpu_mask_t cpu_mask;
    uint32_t max_rate;
    ktl::span<const uint32_t> frequencies;
  };

  const DomainConfig kDomains[] = {
      // Domain 0: Little (CPUs 0-1)
      {.domain_id = 0, .cpu_mask = 0x03, .max_rate = 150, .frequencies = kFrequencyLittle},
      // Domain 1: Medium 1 (CPUs 2-4)
      {.domain_id = 1, .cpu_mask = 0x1c, .max_rate = 703, .frequencies = kFrequencyMedium},
      // Domain 2: Medium 2 (CPUs 5-6)
      {.domain_id = 2, .cpu_mask = 0x60, .max_rate = 703, .frequencies = kFrequencyMedium},
      // Domain 3: Big (CPU 7)
      {.domain_id = 3, .cpu_mask = 0x80, .max_rate = 1000, .frequencies = kFrequencyBig},
  };

  power_management::PowerDomainSet domain_set;

  for (const auto& config : kDomains) {
    fbl::AllocChecker ac;
    auto levels =
        fbl::MakeArray<power_management::ProcessorPowerLevel>(&ac, config.frequencies.size() + 1);
    if (!ac.check()) {
      dprintf(CRITICAL, "POWER: Failed to allocate power levels array for domain %u\n",
              config.domain_id);
      return;
    }

    levels[0] = {
        .options = power_management::kPowerLevelOptionsDomainIndependent,
        .processing_rate = 0,
        .power_coefficient_nw = 100'000,
        .control_interface = power_management::ControlInterface::kArmWfi,
        .control_argument = 0,
        .diagnostic_name = "WFI",
    };

    uint64_t max_freq = config.frequencies[0];
    for (uint32_t opp = 0; opp < config.frequencies.size(); ++opp) {
      uint64_t freq = config.frequencies[opp];
      uint64_t rate = ((freq * config.max_rate) + max_freq - 1) / max_freq;
      levels[opp + 1] = {
          .options = 0,
          .processing_rate = rate,
          .power_coefficient_nw = (rate * 200'000) + 10'000'000,
          .control_interface = power_management::ControlInterface::kCpuDriver,
          .control_argument = opp,
          .diagnostic_name = "OPP",
      };
    }

    auto energy_model_result = power_management::EnergyModel::Create(
        ktl::span<const power_management::ProcessorPowerLevel>(levels.data(), levels.size()), {});
    if (energy_model_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to create energy model for domain %u: %d\n",
              config.domain_id, energy_model_result.status_value());
      return;
    }

    auto controller_result = power_management::PDevPowerLevelController::Get(config.domain_id);
    if (controller_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to get PDevPowerLevelController for domain %u: %d\n",
              config.domain_id, controller_result.status_value());
      return;
    }

    auto domain = fbl::MakeRefCountedChecked<power_management::PowerDomain>(
        &ac, config.domain_id, config.cpu_mask, std::move(energy_model_result).value(),
        std::move(controller_result).value());
    if (!ac.check()) {
      dprintf(CRITICAL, "POWER: Failed to allocate PowerDomain for domain %u\n", config.domain_id);
      return;
    }

    auto register_result = domain_set.Add(std::move(domain));
    if (register_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to add power domain %u to set: %d\n", config.domain_id,
              register_result.status_value());
      return;
    }
  }

  Scheduler::SetPowerDomainSet(std::move(domain_set));
  dprintf(INFO, "POWER: Registered iris power domains\n");
}
