// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "batteryutil.h"

#include <fidl/fuchsia.hardware.spmi/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>

#include <filesystem>
#include <iostream>
#include <string>
#include <vector>

namespace fs = std::filesystem;

// Constants for SPMI
constexpr char kSpmiDebugServiceDir[] = "/svc/fuchsia.hardware.spmi.DebugService";
constexpr uint16_t kUsbInSuspendReg = 0x2954;
constexpr uint8_t kSuspendUsbValue = 0x01;
constexpr uint8_t kResumeUsbValue = 0x00;

zx::result<> SetPowerSource(const std::string& source) {
  bool disconnect = false;
  if (source == "battery") {
    disconnect = true;
  } else if (source == "usb") {
    disconnect = false;
  } else {
    // Also support 1/0 for backward compatibility/ease of use if desired,
    // but plan said battery/usb. Let's stick to battery/usb.
    fprintf(stderr, "Invalid power source: '%s'. Supported: battery, usb\n", source.c_str());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  if (!fs::exists(kSpmiDebugServiceDir)) {
    fprintf(stderr, "SPMI debug service directory not found: %s\n", kSpmiDebugServiceDir);
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  // Find the first available SPMI controller
  std::string controller_path;
  for (const auto& entry : fs::directory_iterator(kSpmiDebugServiceDir)) {
    controller_path = entry.path().string() + "/device";
    // Just take the first one found
    break;
  }

  if (controller_path.empty()) {
    fprintf(stderr, "No SPMI controller found in %s\n", kSpmiDebugServiceDir);
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  zx::result connector = component::Connect<fuchsia_hardware_spmi::Debug>(controller_path);
  if (connector.is_error()) {
    fprintf(stderr, "Failed to connect to SPMI controller at %s: %s\n", controller_path.c_str(),
            connector.status_string());
    return connector.take_error();
  }

  auto debug_client = fidl::SyncClient<fuchsia_hardware_spmi::Debug>(std::move(connector.value()));

  // Connect to target 0 (PMIC)
  auto [device_client, device_server] = fidl::Endpoints<fuchsia_hardware_spmi::Device>::Create();
  if (auto result = debug_client->ConnectTarget({0, std::move(device_server)}); result.is_error()) {
    fprintf(stderr, "Failed to connect to SPMI target 0 on controller %s: %s\n",
            controller_path.c_str(), result.error_value().FormatDescription().c_str());
    return zx::error(ZX_ERR_INTERNAL);
  }

  fidl::SyncClient<fuchsia_hardware_spmi::Device> spmi_client(std::move(device_client));

  fuchsia_hardware_spmi::DeviceExtendedRegisterWriteLongRequest request;
  request.address(kUsbInSuspendReg);
  request.data({disconnect ? kSuspendUsbValue : kResumeUsbValue});

  auto write_result = spmi_client->ExtendedRegisterWriteLong(std::move(request));
  if (write_result.is_error()) {
    fprintf(stderr, "Failed to write to SPMI register 0x%x: %s\n", kUsbInSuspendReg,
            write_result.error_value().FormatDescription().c_str());
    return zx::error(ZX_ERR_IO);
  }

  printf("Successfully set power source to %s (wrote 0x%02x to 0x%04x)\n", source.c_str(),
         disconnect ? kSuspendUsbValue : kResumeUsbValue, kUsbInSuspendReg);

  return zx::ok();
}

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
  } else if (strcmp(argv[1], "power") == 0) {
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
    } else if (strcmp(argv[2], "power") == 0) {
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
      return zx::error(ZX_ERR_INTERNAL);
    }
    return zx::ok(args);
  } else if (command_str == "power") {
    args.func = BatteryFunc::kSetPowerSource;
    if (value_idx != -1) {
      if (argc <= value_idx) {
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
      args.value = argv[value_idx];
    } else {
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

// Helper to format values with micro-units (uA, uAh, uV)
std::string FormatUnit(int32_t value, const char* unit_suffix) {
  double val = static_cast<double>(value);
  const char* prefix = "u";

  if (std::abs(val) >= 1000000) {
    val /= 1000000;
    prefix = "";
  } else if (std::abs(val) >= 1000) {
    val /= 1000;
    prefix = "m";
  }

  // Use sufficient precision
  char buffer[64];
  snprintf(buffer, sizeof(buffer), "%.3f %s%s", val, prefix, unit_suffix);
  return std::string(buffer);
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

  if (info.has_present_charging_current_ua()) {
    printf("Current Draw: %s\n", FormatUnit(info.present_charging_current_ua(), "A").c_str());
  }
}
