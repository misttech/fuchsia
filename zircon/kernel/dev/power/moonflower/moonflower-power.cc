// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include <lib/ddk/platform-defs.h>
#include <lib/mmio-ptr/mmio-ptr.h>
#include <lib/zbi-format/driver-config.h>
#include <reg.h>
#include <trace.h>
#include <zircon/types.h>

#include <arch/arm64/periphmap.h>
#include <dev/power.h>
#include <dev/power/moonflower/init.h>
#include <dev/psci.h>
#include <lk/init.h>
#include <pdev/power.h>
#include <phys/handoff.h>
#include <vm/physmap.h>

#define LOCAL_TRACE 0

namespace {

// Reboot modes, as understood by the moonflower bootloader
// These need to be written to spmi-sdam nvmem cell restart_reason
enum class MOONFLOWER_REBOOT_MODE : uint8_t {
  NORMAL = 0,
  RECOVERY = 1,
  FASTBOOT = 2,
  RTC = 3,
  DMV_CORRUPT = 4,
  DMV_ENFORCING = 5,
  KEYS_CLEAR = 6,
  SHIP_MODE = 32,
};

zx_status_t moonflower_power_reboot(power_reboot_flags flags) {
  LTRACEF("flags %#x\n", static_cast<uint32_t>(flags));

  // Set the moonflower bootloader recovery mode
  [[maybe_unused]] MOONFLOWER_REBOOT_MODE mode = MOONFLOWER_REBOOT_MODE::NORMAL;
  switch (flags) {
    case power_reboot_flags::REBOOT_NORMAL:
      mode = MOONFLOWER_REBOOT_MODE::NORMAL;
      break;
    case power_reboot_flags::REBOOT_BOOTLOADER:
      mode = MOONFLOWER_REBOOT_MODE::FASTBOOT;
      break;
    case power_reboot_flags::REBOOT_RECOVERY:
      mode = MOONFLOWER_REBOOT_MODE::RECOVERY;
      break;
  }

  // TODO(https://fxbug.dev//383788491): Set reboot reason in the populate nvmem here.

  // Hit the reboot switch
  // Call through to SYSTEM_RESET2 with a vendor specific reset type (bit 31 set).
  // TODO(drewry, travisg): figure out if the reboot flags above can be combined to
  // influence this at all.
  psci_system_reset2_raw(0x80000000, 0);
  for (;;) {
    __wfi();
  }

  return ZX_ERR_NOT_SUPPORTED;
}

constexpr size_t kPowerDomainCount = 1;
constexpr uint32_t kDomainId = 0;
constexpr uint64_t kMaxOppIndex = 3;

vaddr_t opp_index_reg = 0;

zx_status_t moonflower_opp_set(uint32_t domain_id, uint64_t opp) {
  if (opp_index_reg == 0) {
    return ZX_ERR_BAD_STATE;
  }
  if (domain_id != kDomainId || opp > kMaxOppIndex) {
    return ZX_ERR_INVALID_ARGS;
  }

  writel(static_cast<uint32_t>(kMaxOppIndex - opp), opp_index_reg);
  return ZX_OK;
}

zx::result<uint64_t> moonflower_opp_get(uint32_t domain_id) {
  if (opp_index_reg == 0) {
    return zx::error(ZX_ERR_BAD_STATE);
  }
  if (domain_id != kDomainId) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  return zx::ok<uint32_t>(kMaxOppIndex - readl(opp_index_reg));
}

zx::result<size_t> moonflower_opp_get_domain_count() { return zx::ok(kPowerDomainCount); }

void init_opp_reg() {
  opp_index_reg = periph_paddr_to_vaddr(0xf521000 + 0x920);
  dprintf(INFO, "POWER: current opp %" PRIu64 "\n", moonflower_opp_get(kDomainId).value_or(-1));
}

// Set up standard pdev power looks except for reboot, which needs to tweak the
// arguments passed to the PSCI reboot call
constexpr pdev_power_ops moonflower_power_ops = {
    .reboot = moonflower_power_reboot,
    .shutdown = psci_system_off,
    .cpu_off = psci_cpu_off,
    .cpu_on = psci_cpu_on,
    .get_cpu_state = psci_get_cpu_state,
    .opp_set = moonflower_opp_set,
    .opp_get = moonflower_opp_get,
    .opp_get_domain_count = moonflower_opp_get_domain_count,
};

}  // anonymous namespace

void moonflower_power_init_early() {
  dprintf(INFO, "POWER: registering moonflower power hooks\n");
  init_opp_reg();
  pdev_register_power(&moonflower_power_ops);
}
