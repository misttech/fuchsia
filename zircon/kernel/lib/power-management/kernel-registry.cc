// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include "lib/power-management/kernel-registry.h"

#include <zircon/errors.h>
#include <zircon/types.h>

#include <kernel/cpu.h>
#include <kernel/percpu.h>

namespace power_management {

void KernelPowerDomainRegistry::UpdateAllCpuPowerDomainSets(
    const PowerDomainSet& power_domain_set) {
  percpu::ForEachPreemptDisable([&power_domain_set](percpu* percpu) {
    percpu->scheduler.ExchangePowerDomainSet(power_domain_set);
  });
}

zx::result<uint32_t> KernelPowerDomainRegistry::UpdatePowerLevel(uint32_t domain_id,
                                                                 uint64_t controller_id,
                                                                 ControlInterface interface,
                                                                 uint64_t arg) {
  std::optional<uint8_t> power_level;
  zx_cpu_set_t cpus;

  {
    Guard<SpinLock, IrqSave> guard(Lock::Get());

    PowerDomain* domain = registry_.Find(domain_id);
    if (!domain) {
      return zx::error(ZX_ERR_NOT_FOUND);
    }

    if (domain->controller()->id() != controller_id) {
      return zx::error(ZX_ERR_ACCESS_DENIED);
    }

    power_level = domain->model().FindPowerLevel(interface, arg);
    if (!power_level) {
      return zx::error(ZX_ERR_NOT_FOUND);
    }

    // Only the active power level may be affected by the power level
    // controller. It is invalid for the power level controller to specify that
    // a domain is now operating at an idle power level.
    if (*power_level < domain->model().idle_levels().size()) {
      return zx::error(ZX_ERR_OUT_OF_RANGE);
    }

    cpus = domain->cpus();
  }

  // Update the scheduling rate for each CPU in the affected domain, then
  // reschedule them to update their internal accounting.
  cpu_mask_t cpus_to_reschedule_mask = 0;
  percpu::ForEach([&cpus, power_level = *power_level, &cpus_to_reschedule_mask](cpu_num_t cpu_num,
                                                                                percpu* percpu) {
    const size_t bit_num = cpu_num % ZX_CPU_SET_BITS_PER_WORD;
    const size_t index = cpu_num / ZX_CPU_SET_BITS_PER_WORD;
    DEBUG_ASSERT(index < ZX_CPU_SET_MAX_CPUS / ZX_CPU_SET_BITS_PER_WORD);

    if (cpus.mask[index] & uint64_t{1} << bit_num) {
      cpus_to_reschedule_mask |= cpu_num_to_mask(cpu_num);
      zx::result result = percpu->scheduler.UpdateActivePowerLevel(power_level);
      ZX_ASSERT_MSG(result.is_ok(), "Unexpected error setting power level: %d",
                    result.status_value());
    }
  });

  return zx::ok(cpus_to_reschedule_mask);
}

}  // namespace power_management
