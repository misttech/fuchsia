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

#ifdef _KERNEL
#include <debug.h>
#include <lib/boot-options/boot-options.h>
#include <lib/power-management/pdev-power-level-controller.h>

#include <fbl/array.h>
#include <kernel/ffi.h>
#include <kernel/scheduler.h>
#else
#ifndef FFI_ALWAYS_INLINE
#ifdef __clang__
#define FFI_ALWAYS_INLINE [[gnu::always_inline]]
#else
#define FFI_ALWAYS_INLINE
#endif
#endif
#endif

namespace power_management {

zx::result<EnergyModel> EnergyModel::Create(
    std::span<const ProcessorPowerLevel> levels,
    std::span<const ProcessorPowerLevelTransition> transitions, uint64_t max_processing_rate) {
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

  if (max_processing_rate == 0) {
    for (const auto& level : levels) {
      max_processing_rate = std::max(max_processing_rate, level.processing_rate);
    }
    if (max_processing_rate == 0) {
      max_processing_rate = 1;
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
    power_levels.push_back(PowerLevel(static_cast<uint8_t>(i), levels[i], max_processing_rate),
                           &ac);
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
                            std::move(power_levels_lookup), idle_levels, max_processing_rate});
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
namespace power_management {

zx::result<PowerDomainSet> PowerDomainSet::Create(
    std::span<const power_domain_config_ffi> domain_configs) {
  if (domain_configs.empty()) {
    return zx::ok(PowerDomainSet{});
  }
  if (domain_configs.size() > kMaxPowerDomains) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }

  // Validate configs and find system-wide max processing rate across all domains.
  uint64_t max_system_processing_rate = 0;
  for (const auto& config : domain_configs) {
    if (!config.levels && config.level_count > 0) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    if (config.cpu_mask == 0) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    for (size_t j = 0; j < config.level_count; ++j) {
      max_system_processing_rate =
          std::max(max_system_processing_rate, config.levels[j].processing_rate);
    }
  }
  if (max_system_processing_rate == 0) {
    max_system_processing_rate = 1;
  }

  std::array<fbl::RefPtr<PowerDomain>, kMaxPowerDomains> domain_array;

  for (size_t i = 0; i < domain_configs.size(); ++i) {
    const auto& config = domain_configs[i];

    fbl::AllocChecker ac;
    auto levels = fbl::MakeArray<ProcessorPowerLevel>(&ac, config.level_count);
    if (!ac.check()) {
      dprintf(CRITICAL, "POWER: Failed to allocate power levels array for domain %u\n",
              config.domain_id);
      return zx::error(ZX_ERR_NO_MEMORY);
    }

    for (size_t j = 0; j < config.level_count; ++j) {
      const auto& ffi_level = config.levels[j];
      levels[j] = {
          .options = ffi_level.options,
          .processing_rate = ffi_level.processing_rate,
          .power_coefficient_nw = ffi_level.power_coefficient_nw,
          .control_interface = (ffi_level.control_interface == 0) ? ControlInterface::kArmWfi
                                                                  : ControlInterface::kCpuDriver,
          .control_argument = ffi_level.control_argument,
          .diagnostic_name = ffi_level.diagnostic_name ? ffi_level.diagnostic_name : "",
      };
    }

    auto energy_model_result =
        EnergyModel::Create(ktl::span<const ProcessorPowerLevel>(levels.data(), levels.size()), {},
                            max_system_processing_rate);
    if (energy_model_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to create energy model for domain %u: %d\n",
              config.domain_id, energy_model_result.status_value());
      return energy_model_result.take_error();
    }

    auto controller_result = PDevPowerLevelController::Get(config.domain_id);
    if (controller_result.is_error()) {
      dprintf(CRITICAL, "POWER: Failed to get PDevPowerLevelController for domain %u: %d\n",
              config.domain_id, controller_result.status_value());
      return controller_result.take_error();
    }

    auto domain = fbl::MakeRefCountedChecked<PowerDomain>(
        &ac, config.domain_id, static_cast<cpu_mask_t>(config.cpu_mask),
        std::move(energy_model_result).value(), std::move(controller_result).value());
    if (!ac.check()) {
      dprintf(CRITICAL, "POWER: Failed to allocate PowerDomain for domain %u\n", config.domain_id);
      return zx::error(ZX_ERR_NO_MEMORY);
    }
    domain->SetSchedulerControlEnabled(true);

    domain_array[i] = std::move(domain);
  }

  return Create(
      std::span<const fbl::RefPtr<PowerDomain>>{domain_array.data(), domain_configs.size()});
}

}  // namespace power_management

extern "C" zx_status_t cpp_power_management_register_domains(const power_domain_config_ffi* domains,
                                                             size_t domain_count) {
  if (!domains && domain_count > 0) {
    return ZX_ERR_INVALID_ARGS;
  }

  auto domain_set_result = power_management::PowerDomainSet::Create(
      ktl::span<const power_domain_config_ffi>{domains, domain_count});
  if (domain_set_result.is_error()) {
    dprintf(CRITICAL, "POWER: Failed to create PowerDomainSet: %d\n",
            domain_set_result.status_value());
    return domain_set_result.status_value();
  }

  Scheduler::SetPowerDomainSet(std::move(domain_set_result).value());
  dprintf(INFO, "POWER: Registered power domains in scheduler\n");
  return ZX_OK;
}

extern "C" zx_status_t cpp_power_management_set_rate_limits(uint64_t cpu_mask_in, uint64_t min_rate,
                                                            uint64_t max_rate) {
  cpu_mask_t cpu_mask = static_cast<cpu_mask_t>(cpu_mask_in);
  const size_t cpu_count = std::popcount(cpu_mask);
  if (cpu_count == 0) {
    return ZX_OK;
  }

  fbl::AllocChecker checker;
  fbl::Array<zx_cpu_perf_limit_t> limits = fbl::MakeArray<zx_cpu_perf_limit_t>(&checker, cpu_count);
  if (!checker.check()) {
    return ZX_ERR_NO_MEMORY;
  }

  for (size_t i = 0; i < cpu_count; i++) {
    const cpu_num_t cpu = remove_cpu_from_mask(cpu_mask);
    DEBUG_ASSERT(cpu != INVALID_CPU);
    limits[i] = {
        .logical_cpu_number = cpu,
        .limit_type = ZX_CPU_PERF_LIMIT_TYPE_RATE,
        .min = min_rate,
        .max = max_rate,
    };
  }
  DEBUG_ASSERT(cpu_mask == 0);

  Scheduler::UpdateProcessingLimits(ktl::span{limits.data(), limits.size()});
  return ZX_OK;
}

extern "C" bool cpp_power_management_boot_boost_enabled() {
  const BootOptions* boot_options = BootOptions::Get();
  return boot_options ? boot_options->power_boot_boost : true;
}
#else
namespace power_management {
zx::result<PowerDomainSet> PowerDomainSet::Create(
    std::span<const power_domain_config_ffi> domain_configs) {
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}
}  // namespace power_management

extern "C" zx_status_t cpp_power_management_register_domains(const power_domain_config_ffi* domains,
                                                             size_t domain_count) {
  return ZX_ERR_NOT_SUPPORTED;
}

extern "C" zx_status_t cpp_power_management_set_rate_limits(uint64_t cpu_mask, uint64_t min_rate,
                                                            uint64_t max_rate) {
  return ZX_ERR_NOT_SUPPORTED;
}

extern "C" bool cpp_power_management_boot_boost_enabled() { return true; }
#endif

static_assert(sizeof(fbl::RefPtr<power_management::PowerDomain>) == 8);
static_assert(alignof(fbl::RefPtr<power_management::PowerDomain>) == 8);
static_assert(sizeof(power_management::PowerDomainSet) == 32);
static_assert(alignof(power_management::PowerDomainSet) == 8);

static_assert(sizeof(std::atomic<int64_t>) == sizeof(int64_t));
static_assert(alignof(std::atomic<int64_t>) == alignof(int64_t));
static_assert(std::atomic<int64_t>::is_always_lock_free);

static_assert(!std::is_polymorphic_v<power_management::PowerDomain>);

__BEGIN_CDECLS

void cpp_power_domain_recycle(power_management::PowerDomain* ptr) {
  ZX_DEBUG_ASSERT(static_cast<const void*>(
                      static_cast<const fbl::RefCounted<power_management::PowerDomain>*>(ptr)) ==
                  static_cast<const void*>(ptr));
  delete ptr;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint32_t cpp_power_domain_id(const power_management::PowerDomain* domain) {
  return domain->id();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE uint64_t cpp_power_domain_cpus(const power_management::PowerDomain* domain) {
  return domain->cpus();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const power_management::EnergyModel* cpp_power_domain_model(
    const power_management::PowerDomain* domain) {
  return &domain->model();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const power_management::PowerLevelController* cpp_power_domain_controller(
    const power_management::PowerDomain* domain) {
  return domain->controller().get();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_power_domain_scheduler_control_enabled(
    const power_management::PowerDomain* domain) {
  return domain->scheduler_control_enabled();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const std::atomic<int64_t>* cpp_power_domain_total_normalized_utilization_ptr(
    const power_management::PowerDomain* domain) {
  return &domain->total_normalized_utilization_;
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_power_level_controller_is_serving(
    const power_management::PowerLevelController* controller) {
  return controller->is_serving();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE power_management::ControlInterface cpp_power_level_controller_control_interface(
    const power_management::PowerLevelController* controller) {
  return controller->control_interface();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE zx_status_t cpp_power_level_controller_get_current_power_level(
    const power_management::PowerLevelController* controller, uint32_t domain_id,
    uint64_t* out_level) {
  const zx::result<uint64_t> result = controller->GetCurrentPowerLevel(domain_id);
  if (result.is_ok()) {
    *out_level = result.value();
  }
  return result.status_value();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE const power_management::PowerLevel* cpp_energy_model_levels_data(
    const power_management::EnergyModel* model) {
  return model->levels().data();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t cpp_energy_model_levels_size(const power_management::EnergyModel* model) {
  return model->levels().size();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE size_t
cpp_energy_model_idle_levels_size(const power_management::EnergyModel* model) {
  return model->idle_levels().size();
}

// TODO(https://fxbug.dev/537458631): Remove the annotations once cross-language inlining works.
FFI_ALWAYS_INLINE bool cpp_energy_model_find_power_level(
    const power_management::EnergyModel* model, power_management::ControlInterface interface_id,
    uint64_t control_argument, uint8_t* out_level) {
  const std::optional<uint8_t> level = model->FindPowerLevel(interface_id, control_argument);
  if (level.has_value()) {
    *out_level = *level;
    return true;
  }
  return false;
}

__END_CDECLS
