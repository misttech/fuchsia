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
#include <arch/arm64/smccc.h>
#include <dev/power.h>
#include <dev/power/moonflower/init.h>
#include <dev/psci.h>
#include <lk/init.h>
#include <pdev/power.h>
#include <phys/handoff.h>
#include <vm/physmap.h>

#define LOCAL_TRACE 0

namespace {

// SMC64 fast call (0xc), SiP Service Call (0x2), boot service (0x01), function 0x09
constexpr uint32_t kTzConfigHwForRamDumpFuncId = 0xc2000109;
// 2 value type parameters, see encoding in lib/qualcomm/smc/smc.h
constexpr uint32_t kTzConfigHwForRamDumpParamId = 0x2;

// Vendor-specific (bit 31) SYSTEM_RESET2 reset type to request a warm reset on Moonflower.
// The architectural SYSTEM_WARM_RESET type (0x0) is not supported by the firmware.
constexpr uint32_t kVendorSpecificWarmResetType = 0x80000000;

int64_t moonflower_config_hw_for_ram_dump(uint64_t disable_wd_dbg, uint64_t boot_partition_sel) {
  arm_smccc_result_t res = arm_smccc_smc(kTzConfigHwForRamDumpFuncId, kTzConfigHwForRamDumpParamId,
                                         disable_wd_dbg, boot_partition_sel, 0, 0, 0, 0);
  return static_cast<int64_t>(res.x0);
}

// Configures hardware registers before a controlled shutdown/reboot to ensure
// the hardware shuts down correctly.
void moonflower_configure_hw_for_shutdown() {
  int64_t r = moonflower_config_hw_for_ram_dump(1 /* disable_wd_dbg */, 0 /* boot_partition_sel */);
  if (r)
    dprintf(INFO, "POWER: Failed to configure moonflower for shutdown/reboot: %" PRId64 "\n", r);
}

zx_status_t moonflower_power_reboot(power_reboot_flags flags) {
  // Reboot reason is written to spmi-sdam nvmem cell inside the qcom-reboot-reason driver
  // (nothing to do here)
  LTRACEF("flags %#x\n", static_cast<uint32_t>(flags));

  // Hit the reboot switch
  // TODO(https://fxbug.dev/489021658): Reboot using cold/hard reset (PSCI SYSTEM_RESET)
  // for graceful reboots.
  return psci_system_reset2_raw(kVendorSpecificWarmResetType, 0);
}

zx_status_t moonflower_power_shutdown() {
  moonflower_configure_hw_for_shutdown();
  return psci_system_off();
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
    .shutdown = moonflower_power_shutdown,
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
