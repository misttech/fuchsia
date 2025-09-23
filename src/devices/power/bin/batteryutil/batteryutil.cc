// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "batteryutil.h"

zx::result<BatteryFunc> ParseArgs(int argc, char** argv) {
  if (argc < 3) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  std::string_view func_arg = argv[2];

  if (func_arg == "get") {
    return zx::ok(BatteryFunc::kGet);
  }
  if (func_arg == "enable") {
    if (argc < 4) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    return zx::ok(BatteryFunc::kEnableCharger);
  }

  return zx::error(ZX_ERR_INVALID_ARGS);
}

void PrintBatteryInfo(const fuchsia_power_battery::wire::BatteryInfo& info) {
  if (info.has_status()) {
    switch (info.status()) {
      case fuchsia_power_battery::BatteryStatus::kOk:
        printf("Status: OK\n");
        break;
      case fuchsia_power_battery::BatteryStatus::kNotAvailable:
        printf("Status: Not Available\n");
        break;
      case fuchsia_power_battery::BatteryStatus::kNotPresent:
        printf("Status: Not Present\n");
        break;
      case fuchsia_power_battery::BatteryStatus::kUnknown:
      default:
        printf("Status: Unknown\n");
        break;
    }
  }

  if (info.has_charge_status()) {
    switch (info.charge_status()) {
      case fuchsia_power_battery::ChargeStatus::kCharging:
        printf("Charge Status: Charging\n");
        break;
      case fuchsia_power_battery::ChargeStatus::kDischarging:
        printf("Charge Status: Discharging\n");
        break;
      case fuchsia_power_battery::ChargeStatus::kFull:
        printf("Charge Status: Full\n");
        break;
      case fuchsia_power_battery::ChargeStatus::kNotCharging:
        printf("Charge Status: Not Charging\n");
        break;
      case fuchsia_power_battery::ChargeStatus::kUnknown:
      default:
        printf("Charge Status: Unknown\n");
        break;
    }
  }

  if (info.has_level_percent()) {
    printf("Level: %f%%\n", info.level_percent());
  }
}
