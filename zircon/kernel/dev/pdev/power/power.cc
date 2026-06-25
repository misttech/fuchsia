// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <lib/arch/intrin.h>

#include <dev/power.h>
#include <pdev/power.h>

static const struct pdev_power_ops default_ops = {
    .reboot = [](power_reboot_flags flags) -> zx_status_t { return ZX_OK; },
    .shutdown = []() -> zx_status_t { return ZX_OK; },
    .cpu_off = []() -> zx_status_t { return ZX_OK; },
    .cpu_on = [](uint64_t mpid, paddr_t entry, uint64_t context) -> zx_status_t { return ZX_OK; },
    .get_cpu_state = [](uint64_t hw_cpu_id) -> zx::result<power_cpu_state> {
      return zx::error(ZX_ERR_NOT_SUPPORTED);
    },
    .opp_set = nullptr,
    .opp_get = nullptr,
    .opp_get_domain_count = nullptr,
};

static const struct pdev_power_ops* power_ops = &default_ops;

void power_reboot(power_reboot_flags flags) { power_ops->reboot(flags); }
void power_shutdown() { power_ops->shutdown(); }

zx_status_t power_cpu_off() { return power_ops->cpu_off(); }
zx_status_t power_cpu_on(uint64_t hw_cpu_id, paddr_t entry, uint64_t context) {
  return power_ops->cpu_on(hw_cpu_id, entry, context);
}

zx::result<power_cpu_state> power_get_cpu_state(uint64_t hw_cpu_id) {
  return power_ops->get_cpu_state(hw_cpu_id);
}

zx_status_t power_opp_set(uint32_t domain_id, uint64_t opp) {
  if (power_ops->opp_set) {
    return power_ops->opp_set(domain_id, opp);
  }
  return ZX_ERR_NOT_SUPPORTED;
}
zx::result<uint64_t> power_opp_get(uint32_t domain_id) {
  if (power_ops->opp_get) {
    return power_ops->opp_get(domain_id);
  }
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

zx::result<size_t> power_opp_get_domain_count() {
  if (power_ops->opp_get_domain_count) {
    return power_ops->opp_get_domain_count();
  }
  return zx::error(ZX_ERR_NOT_SUPPORTED);
}

void pdev_register_power(const struct pdev_power_ops* ops) {
  power_ops = ops;
  arch::ThreadMemoryBarrier();
}

const pdev_power_ops* pdev_swap_power_for_test(const pdev_power_ops* ops) {
  const pdev_power_ops* old = power_ops;
  power_ops = ops;
  arch::ThreadMemoryBarrier();
  return old;
}
