// Copyright 2024 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT
#include <lib/ddk/platform-defs.h>
#include <lib/mmio-ptr/mmio-ptr.h>
#include <lib/power-management/energy-model.h>
#include <lib/power-management/pdev-power-level-controller.h>
#include <lib/zbi-format/driver-config.h>
#include <trace.h>
#include <zircon/types.h>

#include <arch/arm64/periphmap.h>
#include <arch/arm64/smccc.h>
#include <dev/power.h>
#include <dev/power/moonflower/init.h>
#include <dev/psci.h>
#include <fbl/ref_ptr.h>
#include <kernel/scheduler.h>
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

// SMC64 fast call (0xc), SiP Service Call (0x2), I/O service (0x05), function 0x02
constexpr uint32_t kTzIoAccessWriteFuncId = 0xc2000502;
constexpr uint32_t kTzIoAccessWriteParamId = 0x2;

// Download/debug mode that is entered after a warm reset.
enum class MoonflowerDownloadMode : uint8_t {
  kNoDump = 0x00,
  kEdl = 0x01,
  kFullDump = 0x10,
  kMiniDump = 0x20,
};
constexpr paddr_t kMoonflowerDownloadModeAddr = 0x003d3000;

// Vendor-specific (bit 31) SYSTEM_RESET2 reset type to request a warm reset on Moonflower.
// The architectural SYSTEM_WARM_RESET type (0x0) is not supported by the firmware.
constexpr uint32_t kVendorSpecificWarmResetType = 0x80000000;

int64_t moonflower_config_hw_for_ram_dump(uint64_t disable_wd_dbg, uint64_t boot_partition_sel) {
  arm_smccc_result_t res = arm_smccc_smc(kTzConfigHwForRamDumpFuncId, kTzConfigHwForRamDumpParamId,
                                         disable_wd_dbg, boot_partition_sel, 0, 0, 0, 0);
  return static_cast<int64_t>(res.x0);
}

int64_t moonflower_io_write(paddr_t paddr, uint32_t val) {
  arm_smccc_result_t res =
      arm_smccc_smc(kTzIoAccessWriteFuncId, kTzIoAccessWriteParamId, paddr, val, 0, 0, 0, 0);
  return static_cast<int64_t>(res.x0);
}

// Configures hardware registers before a controlled shutdown/reboot to ensure
// the hardware shuts down correctly.
void moonflower_configure_hw_for_shutdown() {
  int64_t r = moonflower_config_hw_for_ram_dump(1 /* disable_wd_dbg */, 0 /* boot_partition_sel */);
  if (r)
    dprintf(INFO, "POWER: Failed to configure moonflower for shutdown/reboot: %" PRId64 "\n", r);
}

// Configures the download/debug mode that is entered after a warm reset.
void moonflower_set_download_mode(MoonflowerDownloadMode mode) {
  uint32_t val = static_cast<uint32_t>(mode);
  int64_t r = moonflower_io_write(kMoonflowerDownloadModeAddr, val);
  if (r)
    dprintf(INFO, "POWER: Failed to set moonflower download mode to 0x%x: %" PRId64 "\n", val, r);
}

zx_status_t moonflower_power_reboot(power_reboot_flags flags) {
  // Reboot reason is written to spmi-sdam nvmem cell inside the qcom-reboot-reason driver
  // (nothing to do here)
  LTRACEF("flags %#x\n", static_cast<uint32_t>(flags));

  // Avoid entering ramdump/debug mode for graceful reboots
  moonflower_set_download_mode(MoonflowerDownloadMode::kNoDump);
  moonflower_configure_hw_for_shutdown();

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

MMIO_PTR volatile uint32_t* opp_index_reg = nullptr;

zx_status_t moonflower_opp_set(uint32_t domain_id, uint64_t opp) {
  if (opp_index_reg == nullptr) {
    return ZX_ERR_BAD_STATE;
  }
  if (domain_id != kDomainId || opp > kMaxOppIndex) {
    return ZX_ERR_INVALID_ARGS;
  }

  MmioWrite32(static_cast<uint32_t>(kMaxOppIndex - opp), opp_index_reg);
  return ZX_OK;
}

zx_status_t moonflower_opp_get(uint32_t domain_id, uint64_t* out_opp) {
  if (opp_index_reg == nullptr) {
    return ZX_ERR_BAD_STATE;
  }
  if (domain_id != kDomainId || out_opp == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }

  *out_opp = kMaxOppIndex - MmioRead32(opp_index_reg);
  return ZX_OK;
}

zx_status_t moonflower_opp_get_domain_count(size_t* out_count) {
  if (out_count == nullptr) {
    return ZX_ERR_INVALID_ARGS;
  }
  *out_count = kPowerDomainCount;
  return ZX_OK;
}

void init_opp_reg() {
  opp_index_reg =
      reinterpret_cast<MMIO_PTR volatile uint32_t*>(periph_paddr_to_vaddr(0xf521000 + 0x920));
  uint64_t opp = 0;
  moonflower_opp_get(kDomainId, &opp);
  dprintf(INFO, "POWER: current opp %" PRIu64 "\n", opp);
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

void moonflower_power_init() {
  dprintf(INFO, "POWER: initializing moonflower power domain\n");

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

  // The moonflower board uses an SoC with a single cluster of 4 cores. All
  // cores reside in the same physical power domain, so we can hardcode the CPU
  // mask instead of querying the topology. If this driver is extended to
  // support other Monaco-based SoCs in the future, this should be revisited and
  // moved to a more generic driver, since this is a board-specific
  // configuration.
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
