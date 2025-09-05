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

zx::result<> KernelPowerDomainRegistry::UpdatePowerLevel(uint32_t domain_id, uint64_t controller_id,
                                                         ControlInterface interface, uint64_t arg) {
  Guard<Mutex> guard(Lock::Get());

  PowerDomain* domain = registry_.Find(domain_id);
  if (!domain) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (domain->controller()->id() != controller_id) {
    return zx::error(ZX_ERR_ACCESS_DENIED);
  }

  std::optional<uint8_t> power_level = domain->model().FindPowerLevel(interface, arg);
  if (!power_level) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (*power_level < domain->model().idle_levels().size()) {
    return zx::error(ZX_ERR_OUT_OF_RANGE);
  }

  percpu::ForEach([domain, power_level](cpu_num_t cpu_num, percpu* percpu) {
    const size_t bit_num = cpu_num % ZX_CPU_SET_BITS_PER_WORD;
    const size_t index = cpu_num / ZX_CPU_SET_BITS_PER_WORD;
    DEBUG_ASSERT(index < ZX_CPU_SET_MAX_CPUS / ZX_CPU_SET_BITS_PER_WORD);

    if (domain->cpus().mask[index] & uint64_t{1} << bit_num) {
      zx::result result = percpu->scheduler.UpdateActivePowerLevel(*power_level);
      ZX_ASSERT_MSG(result.is_ok(), "Unexpected error setting power level: %d",
                    result.status_value());
    }
  });

  return zx::ok();
}

}  // namespace power_management
