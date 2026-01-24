// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "batteryutil.h"

#include <filesystem>
#include <iostream>
#include <string>
#include <vector>

namespace fs = std::filesystem;

zx::result<CmdArgs> ParseArgs(int argc, char** argv) {
  if (argc < 2) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  CmdArgs args;
  std::string_view command_str;
  int value_idx = -1;

  // Determine path and command argument index.
  if (strcmp(argv[1], "get") == 0) {
    args.path = "";
    command_str = argv[1];
  } else if (strcmp(argv[1], "enable") == 0) {
    args.path = "";
    command_str = argv[1];
    value_idx = 2;
  } else {
    // First argument is path.
    if (argc < 3) {
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    args.path = argv[1];
    command_str = argv[2];
    if (strcmp(argv[2], "enable") == 0) {
      value_idx = 3;
    }
  }

  // Parse the command.
  if (command_str == "get") {
    args.func = BatteryFunc::kGet;
    return zx::ok(args);
  } else if (command_str == "enable") {
    args.func = BatteryFunc::kEnableCharger;
    if (value_idx != -1) {
      if (argc <= value_idx) {
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
      args.value = argv[value_idx];
    } else {
      // This case should not be reached given the checks above.
      return zx::error(ZX_ERR_INTERNAL);
    }
    return zx::ok(args);
  } else {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
}

zx::result<std::string> ResolveServicePath(const std::string& provided_path, BatteryFunc func) {
  if (!provided_path.empty()) {
    return zx::ok(provided_path);
  }

  std::string service_dir;
  if (func == BatteryFunc::kGet) {
    service_dir = "/svc/fuchsia.power.battery.InfoService";
  } else {
    service_dir = "/svc/fuchsia.power.battery.ChargerService";
  }

  if (!fs::exists(service_dir)) {
    fprintf(stderr, "Service directory %s not found.\n", service_dir.c_str());
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  std::vector<std::string> instances;
  for (const auto& entry : fs::directory_iterator(service_dir)) {
    instances.push_back(entry.path().string());
  }

  if (instances.empty()) {
    fprintf(stderr, "No instances found in %s\n", service_dir.c_str());
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (instances.size() == 1) {
    printf("Using service: %s\n", instances[0].c_str());
    return zx::ok(instances[0]);
  }

  printf("Multiple services found:\n");
  for (size_t i = 0; i < instances.size(); ++i) {
    printf("  %zu. %s\n", i + 1, instances[i].c_str());
  }
  printf("Select a service (1-%zu): ", instances.size());

  size_t selection;
  std::cin >> selection;

  if (std::cin.fail() || selection < 1 || selection > instances.size()) {
    fprintf(stderr, "Invalid selection.\n");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  return zx::ok(instances[selection - 1]);
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
