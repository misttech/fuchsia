// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

#include <debug.h>

#include <dev/power/iris/init.h>
#include <dev/psci.h>
#include <pdev/power.h>

namespace {

// Vendor-specific (bit 31) SYSTEM_RESET2 reset type to request a warm reset on Iris.
constexpr uint32_t kVendorSpecificWarmResetType = 0x80000000;

// Reboot modes passed as the cookie in SYSTEM_RESET2, recognized by the bootloader.
enum class RebootMode : uint32_t {
  kNormal = 0x00,       // Standard boot
  kCharge = 0x0A,       // Boot into off-mode-charging UI.
  kRescue = 0xF9,       // Reboot into Rescue mode.
  kFastboot = 0xFA,     // Reboot into userspace fastbootd.
  kBootloader = 0xFC,   // Reboot into ABL fastboot mode.
  kFactory = 0xFD,      // Reboot into Factory testing mode.
  kRomRecovery = 0xFE,  // Reboot into BootROM recovery mode.
  kRecovery = 0xFF,     // Reboot into Recovery.
};

const char* ToString(RebootMode cookie) {
  switch (cookie) {
    case RebootMode::kNormal:
      return "Normal";
    case RebootMode::kCharge:
      return "Charge";
    case RebootMode::kRescue:
      return "Rescue";
    case RebootMode::kFastboot:
      return "Fastboot";
    case RebootMode::kBootloader:
      return "Bootloader";
    case RebootMode::kFactory:
      return "Factory";
    case RebootMode::kRomRecovery:
      return "RomRecovery";
    case RebootMode::kRecovery:
      return "Recovery";
  }
  return "Unknown";
}

zx_status_t iris_reboot(power_reboot_flags flags) {
  RebootMode cookie;
  switch (flags) {
    case power_reboot_flags::REBOOT_BOOTLOADER:
      cookie = RebootMode::kBootloader;
      break;
    case power_reboot_flags::REBOOT_RECOVERY:
      cookie = RebootMode::kRecovery;
      break;
    case power_reboot_flags::REBOOT_NORMAL:
      cookie = RebootMode::kNormal;
      break;
  }

  dprintf(INFO, "Iris reboot: flags %u, cookie %#x (%s)\n", static_cast<uint32_t>(flags),
          static_cast<uint32_t>(cookie), ToString(cookie));
  return psci_system_reset2_raw(kVendorSpecificWarmResetType, static_cast<uint32_t>(cookie));
}

const struct pdev_power_ops iris_power_ops = {
    .reboot = iris_reboot,
    .shutdown = psci_system_off,
    .cpu_off = psci_cpu_off,
    .cpu_on = psci_cpu_on,
    .get_cpu_state = psci_get_cpu_state,
};

}  // namespace

void iris_power_init_early() {
  dprintf(INFO, "POWER: registering iris power hooks\n");
  pdev_register_power(&iris_power_ops);
}
