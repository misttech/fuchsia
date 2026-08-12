// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/power-management/energy-model.h"

#include <zircon/errors.h>
// TODO(https://fxbug.dev/415033686): Stop using `syscalls-next.h` on host.
#define FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <zircon/syscalls-next.h>
#undef FUCHSIA_UNSUPPORTED_ALLOW_SYSCALLS_NEXT_ON_HOST
#include <lib/stdcompat/utility.h>
#include <zircon/types.h>

#include <algorithm>
#include <cstddef>
#include <cstdint>
#include <optional>
#include <utility>

#include <fbl/alloc_checker.h>
#include <fbl/ref_ptr.h>
#include <fbl/vector.h>

namespace power_management {

zx::result<EnergyModel> EnergyModel::Create(
    std::span<const ProcessorPowerLevel> levels,
    std::span<const ProcessorPowerLevelTransition> transitions) {
  // Allocations below would be UB.
  if (levels.size() < 1) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (transitions.size() > levels.size() * levels.size()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // Validate that transitions are to and from valid levels.
  for (const auto& transition : transitions) {
    if (transition.from >= levels.size()) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    if (transition.to >= levels.size()) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
  }

  fbl::AllocChecker ac;
  fbl::Vector<PowerLevel> power_levels;
  power_levels.reserve(levels.size(), &ac);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  fbl::Vector<size_t> power_levels_lookup;
  power_levels_lookup.reserve(levels.size(), &ac);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  // If we assume symmetry in the matrix, we could reduce the amount of space required to support
  // transitions by more than half, since any element below the main diagonal would be equivalent to
  // its mirror, and the diagonal itself would be 0.
  fbl::Vector<PowerLevelTransition> power_level_transitions;
  PowerLevelTransition default_value =
      transitions.empty() ? PowerLevelTransition::Zero() : PowerLevelTransition::Invalid();

  power_level_transitions.resize(levels.size() * levels.size(), default_value, &ac);
  if (!ac.check()) {
    return zx::error(ZX_ERR_NO_MEMORY);
  }

  size_t idle_levels = 0;
  // We assert below, because all the space required for these operation has been preallocated.
  for (size_t i = 0; i < levels.size(); ++i) {
    power_levels.push_back(PowerLevel(static_cast<uint8_t>(i), levels[i]), &ac);
    // These were preallocated above.
    ZX_ASSERT(ac.check());
    power_levels_lookup.push_back(i, &ac);
    // These were preallocated above.
    ZX_ASSERT(ac.check());
    if (power_levels[i].type() == PowerLevel::kIdle) {
      idle_levels++;
    }
  }

  // Generate lookup table based on original indexes.
  std::sort(power_levels_lookup.begin(), power_levels_lookup.end(),
            [&power_levels](const size_t& a, const size_t& b) constexpr {
              if (power_levels[a].processing_rate() == power_levels[b].processing_rate()) {
                return power_levels[a].power_coefficient_nw() <
                       power_levels[b].power_coefficient_nw();
              }
              return power_levels[a].processing_rate() < power_levels[b].processing_rate();
            });

  // This will naturally partition idle and active states, having all idle states at the beginning.
  std::sort(power_levels.begin(), power_levels.end(),
            [](const PowerLevel& a, const PowerLevel& b) constexpr {
              if (a.processing_rate() == b.processing_rate()) {
                return a.power_coefficient_nw() < b.power_coefficient_nw();
              }
              return a.processing_rate() < b.processing_rate();
            });

  // Fill up the square matrix from level i to level j, where i and j, are indexes into the
  // power_level array.
  for (const auto& transition : transitions) {
    size_t i = power_levels_lookup[transition.from];
    size_t j = power_levels_lookup[transition.to];

    // Double check that translation is correct.
    ZX_ASSERT(power_levels[i].level() == transition.from);
    ZX_ASSERT(power_levels[j].level() == transition.to);

    size_t transition_offset = i * power_levels.size() + j;
    power_level_transitions[transition_offset] = PowerLevelTransition(transition);
  }

  // Reset the power level lookup, and turn it into a lookup by control interface, such that the set
  // path doesnt have to traverse every level.
  for (size_t i = 0; i < power_levels_lookup.size(); ++i) {
    power_levels_lookup[i] = i;
  }

  // Generate lookup table based on control interface and control argument tuple..
  std::sort(power_levels_lookup.begin(), power_levels_lookup.end(),
            [&power_levels](size_t a, size_t b) constexpr {
              if (cpp23::to_underlying(power_levels[a].control()) ==
                  cpp23::to_underlying(power_levels[b].control())) {
                return power_levels[a].control_argument() < power_levels[b].control_argument();
              }

              return cpp23::to_underlying(power_levels[a].control()) <
                     cpp23::to_underlying(power_levels[b].control());
            });

  return zx::ok(EnergyModel{std::move(power_levels), std::move(power_level_transitions),
                            std::move(power_levels_lookup), idle_levels});
}

std::optional<uint8_t> EnergyModel::FindPowerLevel(ControlInterface interface_id,
                                                   uint64_t control_argument) const {
  const auto compare = [this](size_t i, const ProcessorPowerLevel& b) {
    const auto& a = power_levels_[i];
    return a.control() < b.control_interface ||
           (a.control() == b.control_interface && a.control_argument() < b.control_argument);
  };

  const ProcessorPowerLevel power_level{
      .control_interface = interface_id,
      .control_argument = control_argument,
  };

  auto it = std::lower_bound(control_lookup_.begin(), control_lookup_.end(), power_level, compare);
  if (it != control_lookup_.end() && power_levels_[*it].control() == interface_id &&
      power_levels_[*it].control_argument() == control_argument) {
    return *it;
  }

  return std::nullopt;
}

const PowerLevel* EnergyModel::FindActivePowerLevelForRate(ProcessingRate processing_rate) const {
  // Return the maximum power level if the processing rate exceeds the maximum
  // rate.
  processing_rate = std::min(processing_rate, max_processing_rate());

  const auto compare = [](const PowerLevel& power_level, ProcessingRate processing_rate) {
    return power_level.processing_rate() < processing_rate;
  };

  std::span levels = active_levels();
  auto iter = std::lower_bound(levels.begin(), levels.end(), processing_rate, compare);

  return iter != levels.end() ? &*iter : nullptr;
}

}  // namespace power_management

#ifdef _KERNEL
#include <debug.h>
#include <lib/power-management/pdev-power-level-controller.h>

#include <fbl/array.h>
#include <kernel/scheduler.h>

extern "C" zx_status_t cpp_power_management_register_domains(const power_domain_config_ffi* domains,
                                                             size_t domain_count) {
  if (!domains && domain_count > 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  power_management::PowerDomainSet domain_set;

  for (size_t i = 0; i < domain_count; ++i) {
    const auto& config = domains[i];
    if (!config.levels && config.level_count > 0) {
      return ZX_ERR_INVALID_ARGS;
    }

    fbl::AllocChecker ac;
    auto levels = fbl::MakeArray<power_management::ProcessorPowerLevel>(&ac, config.level_count);
    if (!ac.check()) {
      dprintf(CRITICAL, "POWER: Failed to allocate power levels array for domain %u\n",
              config.domain_id);
      return ZX_ERR_NO_MEMORY;
    }

    for (size_t j = 0; j < config.level_count; ++j) {
      const auto& ffi_level = config.levels[j];
      levels[j] = {
          .options = ffi_level.options,
          .processing_rate = ffi_level.processing_rate,
          .power_coefficient_nw = ffi_level.power_coefficient_nw,
          .control_interface = (ffi_level.control_interface == 0)
                                   ? power_management::ControlInterface::kArmWfi
                                   : power_management::ControlInterface::kCpuDriver,
          .control_argument = ffi_level.control_argument,
          .diagnostic_name = ffi_level.diagnostic_name ? ffi_level.diagnostic_name : "",
      };
    }

    auto energy_model_result = power_management::EnergyModel::Create(
        ktl::span<const power_management::ProcessorPowerLevel>(levels.data(), levels.size()), {});
    if (energy_model_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to create energy model for domain %u: %d\n",
              config.domain_id, energy_model_result.status_value());
      return energy_model_result.status_value();
    }

    auto controller_result = power_management::PDevPowerLevelController::Get(config.domain_id);
    if (controller_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to get PDevPowerLevelController for domain %u: %d\n",
              config.domain_id, controller_result.status_value());
      return controller_result.status_value();
    }

    auto domain = fbl::MakeRefCountedChecked<power_management::PowerDomain>(
        &ac, config.domain_id, static_cast<cpu_mask_t>(config.cpu_mask),
        std::move(energy_model_result).value(), std::move(controller_result).value());
    if (!ac.check()) {
      dprintf(CRITICAL, "POWER: Failed to allocate PowerDomain for domain %u\n", config.domain_id);
      return ZX_ERR_NO_MEMORY;
    }

    auto register_result = domain_set.Add(std::move(domain));
    if (register_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to add power domain %u to set: %d\n", config.domain_id,
              register_result.status_value());
      return register_result.status_value();
    }
  }

  Scheduler::SetPowerDomainSet(std::move(domain_set));
  dprintf(INFO, "POWER: Registered power domains in scheduler\n");
  return ZX_OK;
}
#else
extern "C" zx_status_t cpp_power_management_register_domains(const power_domain_config_ffi* domains,
                                                             size_t domain_count) {
  return ZX_ERR_NOT_SUPPORTED;
}
#endif
