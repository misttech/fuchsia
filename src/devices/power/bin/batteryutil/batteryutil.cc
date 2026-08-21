// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "batteryutil.h"

#include <fidl/fuchsia.hardware.spmi/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <unistd.h>
#include <zircon/status.h>

#include <cctype>
#include <charconv>
#include <cmath>
#include <filesystem>
#include <iostream>
#include <string>
#include <vector>

namespace fbattery = fuchsia_hardware_power_battery;
namespace fcharger = fuchsia_hardware_power_charger;
namespace fsource = fuchsia_hardware_power_source;
namespace fpowerbattery = fuchsia_power_battery;

// Constants for SPMI
// kUsbInSuspendReg (0x2954) manipulates the PMIC directly via SPMI debug
// to explicitly suspend or resume USB charging, forcing the system to draw from
// the battery (suspending the USB power source).
constexpr uint16_t kUsbInSuspendReg = 0x2954;
constexpr uint8_t kSuspendUsbValue = 0x01;
constexpr uint8_t kResumeUsbValue = 0x00;

// SetPowerSource directly forces the PMIC into/out-of USB suspend for testing
// overrides by bypassing generic power FIDL frameworks and connecting directly to
// the first available fuchsia.hardware.spmi:Debug service, commanding target 0.
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

  std::error_code ec;
  std::string spmi_service_dir = std::string("/svc/") + fuchsia_hardware_spmi::DebugService::Name;
  if (!std::filesystem::exists(spmi_service_dir, ec) || ec) {
    fprintf(stderr, "SPMI debug service directory not found: %s\n", spmi_service_dir.c_str());
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  // Find the first available SPMI controller
  std::string controller_path;
  std::filesystem::directory_iterator it(spmi_service_dir, ec);
  if (!ec && it != std::filesystem::directory_iterator()) {
    controller_path = it->path().string() + "/device";
  }

  if (controller_path.empty()) {
    fprintf(stderr, "No SPMI controller found in %s\n", spmi_service_dir.c_str());
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

  fuchsia_hardware_spmi::DeviceRegisterWriteRequest request;
  request.address(kUsbInSuspendReg);
  request.data({disconnect ? kSuspendUsbValue : kResumeUsbValue});

  auto write_result = spmi_client->RegisterWrite(std::move(request));
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

  // Check for help flag/command.
  if (strcmp(argv[1], "help") == 0 || strcmp(argv[1], "h") == 0 || strcmp(argv[1], "-h") == 0 ||
      strcmp(argv[1], "--help") == 0) {
    CmdArgs args;
    args.func = BatteryFunc::kHelp;
    return zx::ok(args);
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
      std::string_view val = argv[value_idx];
      if (val == "1" || val == "true" || val == "enable") {
        args.value = "1";
      } else if (val == "0" || val == "false" || val == "disable") {
        args.value = "0";
      } else {
        fprintf(stderr,
                "Invalid value for enable command: '%s'. Expected '1', '0', 'true', 'false', "
                "'enable', or 'disable'.\n",
                argv[value_idx]);
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
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

zx::result<std::string> SelectInstance(const std::vector<std::string>& service_names) {
  std::vector<std::string> instances;
  for (const auto& service_name : service_names) {
    std::error_code ec;
    std::string path = "/svc/" + service_name;
    if (std::filesystem::exists(path, ec) && !ec) {
      std::filesystem::directory_iterator it(path, ec);
      std::filesystem::directory_iterator end;
      while (it != end && !ec) {
        instances.push_back(it->path().string());
        it.increment(ec);
      }
    }
  }

  if (instances.empty()) {
    fprintf(stderr, "No instances found for requested services\n");
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (instances.size() == 1) {
    printf("Using service: %s\n", instances[0].c_str());
    return zx::ok(instances[0]);
  }

  // If we are in an interactive terminal, prompt the user to choose.
  if (isatty(STDIN_FILENO)) {
    printf("Multiple services found:\n");
    for (size_t i = 0; i < instances.size(); ++i) {
      printf("  %zu. %s\n", i + 1, instances[i].c_str());
    }
    printf("Select a service (1-%zu): ", instances.size());
    fflush(stdout);

    std::string input;
    if (std::getline(std::cin, input)) {
      std::string_view trimmed(input);
      while (!trimmed.empty() && std::isspace(static_cast<unsigned char>(trimmed.front()))) {
        trimmed.remove_prefix(1);
      }
      while (!trimmed.empty() && std::isspace(static_cast<unsigned char>(trimmed.back()))) {
        trimmed.remove_suffix(1);
      }
      size_t selection = 0;
      auto [ptr, err] = std::from_chars(trimmed.data(), trimmed.data() + trimmed.size(), selection);
      if (err == std::errc() && ptr == trimmed.data() + trimmed.size() && selection >= 1 &&
          selection <= instances.size()) {
        return zx::ok(instances[selection - 1]);
      }
    }
    fprintf(stderr, "Invalid selection.\n");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  // Non-interactive fallback: pick the first one and warn.
  fprintf(stderr, "WARNING: Multiple service instances found in non-interactive environment.\n");
  fprintf(stderr, "Automatically using the first instance: %s\n", instances[0].c_str());
  return zx::ok(instances[0]);
}

// Helper to format values with micro-units (uA, uAh, uV)
std::string FormatUnit(int64_t value, const char* unit_suffix) {
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

namespace {

const char* BatteryStatusToString(fpowerbattery::BatteryStatus status) {
  switch (status) {
    case fpowerbattery::BatteryStatus::kOk:
      return "OK";
    case fpowerbattery::BatteryStatus::kNotAvailable:
      return "Not Available";
    case fpowerbattery::BatteryStatus::kNotPresent:
      return "Not Present";
    case fpowerbattery::BatteryStatus::kUnknown:
      return "Unknown";
    default:
      return "Unknown";
  }
}

const char* ChargeStatusToString(fpowerbattery::ChargeStatus status) {
  switch (status) {
    case fpowerbattery::ChargeStatus::kCharging:
      return "Charging";
    case fpowerbattery::ChargeStatus::kDischarging:
      return "Discharging";
    case fpowerbattery::ChargeStatus::kNotCharging:
      return "Not Charging";
    case fpowerbattery::ChargeStatus::kFull:
      return "Full";
    case fpowerbattery::ChargeStatus::kUnknown:
      return "Unknown";
    default:
      return "Unknown";
  }
}

const char* ChargeStatusToString(fbattery::ChargeStatus status) {
  switch (status) {
    case fbattery::ChargeStatus::kCharging:
      return "Charging";
    case fbattery::ChargeStatus::kDischarging:
      return "Discharging";
    case fbattery::ChargeStatus::kNotCharging:
      return "Not Charging";
    case fbattery::ChargeStatus::kFull:
      return "Full";
    default:
      return "Unknown";
  }
}

const char* HealthStatusToString(fpowerbattery::HealthStatus status) {
  switch (status) {
    case fpowerbattery::HealthStatus::kGood:
      return "Good";
    case fpowerbattery::HealthStatus::kCold:
      return "Cold";
    case fpowerbattery::HealthStatus::kHot:
      return "Hot";
    case fpowerbattery::HealthStatus::kDead:
      return "Dead";
    case fpowerbattery::HealthStatus::kOverVoltage:
      return "Over Voltage";
    case fpowerbattery::HealthStatus::kUnspecifiedFailure:
      return "Unspecified Failure";
    case fpowerbattery::HealthStatus::kCool:
      return "Cool";
    case fpowerbattery::HealthStatus::kWarm:
      return "Warm";
    case fpowerbattery::HealthStatus::kOverheat:
      return "Overheat";
    case fpowerbattery::HealthStatus::kUnknown:
      return "Unknown";
    default:
      return "Unknown";
  }
}

const char* HealthStatusToString(fbattery::HealthStatus status) {
  switch (status) {
    case fbattery::HealthStatus::kGood:
      return "Good";
    case fbattery::HealthStatus::kCold:
      return "Cold";
    case fbattery::HealthStatus::kCool:
      return "Cool";
    case fbattery::HealthStatus::kWarm:
      return "Warm";
    case fbattery::HealthStatus::kHot:
      return "Hot";
    case fbattery::HealthStatus::kDead:
      return "Dead";
    case fbattery::HealthStatus::kOverVoltage:
      return "Over Voltage";
    case fbattery::HealthStatus::kUnspecifiedFailure:
      return "Unspecified Failure";
    default:
      return "Unknown";
  }
}

const char* ChargeSourceToString(fpowerbattery::ChargeSource source) {
  switch (source) {
    case fpowerbattery::ChargeSource::kNone:
      return "None";
    case fpowerbattery::ChargeSource::kAcAdapter:
      return "AC Adapter";
    case fpowerbattery::ChargeSource::kUsb:
      return "USB";
    case fpowerbattery::ChargeSource::kWireless:
      return "Wireless";
    case fpowerbattery::ChargeSource::kUnknown:
      return "Unknown";
    default:
      return "Unknown";
  }
}

const char* LevelStatusToString(fpowerbattery::LevelStatus status) {
  switch (status) {
    case fpowerbattery::LevelStatus::kOk:
      return "OK";
    case fpowerbattery::LevelStatus::kWarning:
      return "Warning";
    case fpowerbattery::LevelStatus::kLow:
      return "Low";
    case fpowerbattery::LevelStatus::kCritical:
      return "Critical";
    case fpowerbattery::LevelStatus::kUnknown:
      return "Unknown";
    default:
      return "Unknown";
  }
}

void PrintBatteryInfo(const fpowerbattery::wire::BatteryInfo& info) {
  if (info.has_status()) {
    printf("Status: %s\n", BatteryStatusToString(info.status()));
  }
  if (info.has_charge_status()) {
    printf("Charge Status: %s\n", ChargeStatusToString(info.charge_status()));
  }
  if (info.has_charge_source()) {
    printf("Charge Source: %s\n", ChargeSourceToString(info.charge_source()));
  }
  if (info.has_level_percent()) {
    printf("Level: %.1f%%\n", info.level_percent());
  }
  if (info.has_level_status()) {
    printf("Level Status: %s\n", LevelStatusToString(info.level_status()));
  }
  if (info.has_health()) {
    printf("Health: %s\n", HealthStatusToString(info.health()));
  }
  if (info.has_present_voltage_mv()) {
    printf("Voltage: %s\n", FormatUnit(info.present_voltage_mv() * 1000, "V").c_str());
  }
  if (info.has_remaining_charge_uah()) {
    printf("Remaining Charge: %s\n", FormatUnit(info.remaining_charge_uah(), "Ah").c_str());
  }
  if (info.has_full_capacity_uah()) {
    printf("Full Capacity: %s\n", FormatUnit(info.full_capacity_uah(), "Ah").c_str());
  }
  if (info.has_temperature_mc()) {
    printf("Temperature: %.1f C\n", info.temperature_mc() / 1000.0);
  }
  if (info.has_present_charging_current_ua()) {
    printf("Current Draw: %s\n", FormatUnit(info.present_charging_current_ua(), "A").c_str());
  }
  if (info.has_average_charging_current_ua()) {
    printf("Average Current: %s\n", FormatUnit(info.average_charging_current_ua(), "A").c_str());
  }
}

void PrintBatteryInfo(const fbattery::wire::Status& info) {
  if (info.has_source_status()) {
    const auto& src = info.source_status();
    if (src.has_present()) {
      printf("Present: %s\n", src.present() ? "Yes" : "No");
    }
    if (src.has_voltage_uv()) {
      printf("Voltage: %s\n", FormatUnit(src.voltage_uv(), "V").c_str());
    }
    if (src.has_current_ua()) {
      printf("Current: %s\n", FormatUnit(src.current_ua(), "A").c_str());
    }
  }

  if (info.has_charge_status()) {
    printf("Charge Status: %s\n", ChargeStatusToString(info.charge_status()));
  }
  if (info.has_level_percent()) {
    printf("Level: %.1f%%\n", info.level_percent());
  }
  if (info.has_remaining_capacity_uah()) {
    printf("Remaining Capacity: %s\n", FormatUnit(info.remaining_capacity_uah(), "Ah").c_str());
  }
  if (info.has_full_charge_capacity_uah()) {
    printf("Full Charge Capacity: %s\n", FormatUnit(info.full_charge_capacity_uah(), "Ah").c_str());
  }
  if (info.has_health()) {
    printf("Health: %s\n", HealthStatusToString(info.health()));
  }
  if (info.has_temperature_mc()) {
    printf("Temperature: %.1f C\n", info.temperature_mc() / 1000.0);
  }
  if (info.has_cycle_count()) {
    printf("Cycle Count: %u\n", info.cycle_count());
  }
}

std::string AppendMemberSuffixIfNeeded(std::string_view path, std::string_view member_suffix) {
  std::filesystem::path p = std::filesystem::path(path).lexically_normal();
  std::filesystem::path suffix(member_suffix);
  if (p.filename() != suffix.filename()) {
    p /= suffix.filename();
  }
  return p.string();
}

template <typename TryNewFn, typename TryLegacyFn>
zx::result<> TryDualDispatch(const std::string& path, std::string_view new_service_name,
                             std::string_view legacy_service_name, TryNewFn try_new_fn,
                             TryLegacyFn try_legacy_fn, const char* error_label) {
  bool is_new = path.find(new_service_name) != std::string::npos;
  bool is_legacy = path.find(legacy_service_name) != std::string::npos;

  if (is_new) {
    return try_new_fn();
  }
  if (is_legacy) {
    return try_legacy_fn();
  }

  // Fallback for generic paths: try new first, then legacy.
  if (zx::result res = try_new_fn(); res.is_ok()) {
    return zx::ok();
  }
  if (zx::result res = try_legacy_fn(); res.is_ok()) {
    return zx::ok();
  }

  fprintf(stderr, "Failed to %s at %s\n", error_label, path.c_str());
  return zx::error(ZX_ERR_NOT_FOUND);
}

zx::result<> TryGetNewBatteryInfo(std::string path) {
  path = AppendMemberSuffixIfNeeded(path, "/battery");
  zx::result client = component::Connect<fbattery::Battery>(path);
  if (client.is_error()) {
    return client.take_error();
  }
  auto result = fidl::WireCall(client.value())->GetStatus();
  if (!result.ok()) {
    fprintf(stderr, "Call to GetStatus failed: %s\n", result.FormatDescription().c_str());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    fprintf(stderr, "GetStatus rejected with domain error: %d\n",
            static_cast<uint32_t>(result->error_value()));
    return zx::error(ZX_ERR_INTERNAL);
  }
  PrintBatteryInfo(result->value()->status);
  return zx::ok();
}

zx::result<> TryGetLegacyBatteryInfo(std::string path) {
  path = AppendMemberSuffixIfNeeded(path, "/device");
  zx::result client = component::Connect<fpowerbattery::BatteryInfoProvider>(path);
  if (client.is_error()) {
    return client.take_error();
  }
  auto result = fidl::WireCall(client.value())->GetBatteryInfo();
  if (!result.ok()) {
    fprintf(stderr, "Call to GetBatteryInfo failed: %s\n", result.FormatDescription().c_str());
    return zx::error(result.status());
  }
  PrintBatteryInfo(result.value().info);
  return zx::ok();
}

zx::result<> TrySetNewChargerEnable(std::string path, bool enable) {
  path = AppendMemberSuffixIfNeeded(path, "/charger");
  zx::result client = component::Connect<fcharger::Charger>(path);
  if (client.is_error()) {
    return client.take_error();
  }
  auto result = fidl::WireCall(client.value())->SetChargingEnabled(enable);
  if (!result.ok()) {
    fprintf(stderr, "Call to SetChargingEnabled failed: %s\n", result.FormatDescription().c_str());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    fprintf(stderr, "SetChargingEnabled rejected with domain error: %d\n",
            static_cast<uint32_t>(result->error_value()));
    return zx::error(ZX_ERR_INTERNAL);
  }
  return zx::ok();
}

zx::result<> TrySetLegacyChargerEnable(std::string path, bool enable) {
  path = AppendMemberSuffixIfNeeded(path, "/device");
  zx::result client = component::Connect<fpowerbattery::Charger>(path);
  if (client.is_error()) {
    return client.take_error();
  }
  auto result = fidl::WireCall(client.value())->Enable(enable);
  if (!result.ok()) {
    fprintf(stderr, "Call to Enable failed: %s\n", result.FormatDescription().c_str());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    fprintf(stderr, "Enable rejected with domain error: %s\n",
            zx_status_get_string(result->error_value()));
    return zx::error(ZX_ERR_INTERNAL);
  }
  return zx::ok();
}

}  // namespace

zx::result<> GetBatteryInfoCmd(const std::string& path) {
  return TryDualDispatch(
      path, fbattery::Service::Name, fpowerbattery::InfoService::Name,
      [&]() { return TryGetNewBatteryInfo(path); }, [&]() { return TryGetLegacyBatteryInfo(path); },
      "connect to battery service");
}

zx::result<> EnableChargerCmd(const std::string& path, bool enable) {
  return TryDualDispatch(
      path, fcharger::Service::Name, fpowerbattery::ChargerService::Name,
      [&]() { return TrySetNewChargerEnable(path, enable); },
      [&]() { return TrySetLegacyChargerEnable(path, enable); }, "enable/disable charger");
}
