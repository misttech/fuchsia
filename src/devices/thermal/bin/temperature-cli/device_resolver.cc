// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "device_resolver.h"

#include <fidl/fuchsia.hardware.adc/cpp/wire.h>
#include <fidl/fuchsia.hardware.temperature/cpp/wire.h>
#include <fidl/fuchsia.hardware.trippoint/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <stdio.h>

#include <algorithm>
#include <array>
#include <cctype>
#include <charconv>
#include <cstdlib>
#include <filesystem>
#include <optional>
#include <string>
#include <vector>

namespace FidlTemperature = fuchsia_hardware_temperature;

namespace {

// Device Paths
constexpr char kTemperatureServiceDir[] = "/svc/fuchsia.hardware.temperature.Service";
constexpr char kTemperatureDevfsDir[] = "/dev/class/temperature";
constexpr char kAdcServiceDir[] = "/svc/fuchsia.hardware.adc.Service";
constexpr char kAdcDevfsDir[] = "/dev/class/adc";
constexpr char kTrippointServiceDir[] = "/svc/fuchsia.hardware.trippoint.Service";
constexpr char kTrippointDebugServiceDir[] = "/svc/fuchsia.hardware.trippoint.DebugService";

// Service Members
constexpr char kServiceMemberDevice[] = "device";
constexpr char kServiceMemberTripPoint[] = "trippoint";
constexpr char kServiceMemberDebug[] = "debug";

struct DiscoveredDevice {
  std::string name;
  std::string path;
  bool is_service = false;
};

bool IsTrippointCommand(std::string_view arg) {
  return arg == kCmdTripPoint || arg == kCmdWait || arg == kCmdTrip;
}

std::string_view GetMemberForService(std::string_view service_dir, std::string_view command) {
  if (service_dir == kTemperatureServiceDir) {
    return IsTrippointCommand(command) ? kServiceMemberTripPoint : kServiceMemberDevice;
  }
  if (service_dir == kAdcServiceDir) {
    return kServiceMemberDevice;
  }
  if (service_dir == kTrippointServiceDir) {
    return kServiceMemberTripPoint;
  }
  if (service_dir == kTrippointDebugServiceDir) {
    return kServiceMemberDebug;
  }
  return "";
}

bool HasServiceMember(std::string_view path) {
  auto check = [](std::string_view path, std::string_view member) {
    return path.size() >= member.size() + 1 && path[path.size() - member.size() - 1] == '/' &&
           path.ends_with(member);
  };
  return check(path, kServiceMemberDevice) || check(path, kServiceMemberTripPoint) ||
         check(path, kServiceMemberDebug);
}

std::string GetPathForDevice(const DiscoveredDevice& dev, std::string_view command) {
  if (dev.is_service) {
    for (auto service_dir : {
             kTemperatureServiceDir,
             kAdcServiceDir,
             kTrippointServiceDir,
             kTrippointDebugServiceDir,
         }) {
      if (dev.path.starts_with(service_dir)) {
        std::string_view member = GetMemberForService(service_dir, command);
        if (!member.empty()) {
          return (std::filesystem::path(dev.path) / member).string();
        }
      }
    }
  }
  return dev.path;
}

std::optional<std::string> TryGetSensorName(const std::string& device_member_path) {
  auto client_res = ConnectToDevice<FidlTemperature::Device>(device_member_path);
  if (client_res.is_error()) {
    return std::nullopt;
  }
  auto client = std::move(client_res.value());
  auto response = client->GetSensorName();
  if (response.ok()) {
    return std::string(response->name.data(), response->name.size());
  }
  return std::nullopt;
}

zx::result<size_t> PromptUserSelection(const std::vector<std::string>& display_items,
                                       std::string_view device_type_name) {
  printf("Multiple %.*s devices found:\n", static_cast<int>(device_type_name.size()),
         device_type_name.data());
  for (size_t i = 0; i < display_items.size(); ++i) {
    printf("  %zu. %s\n", i + 1, display_items[i].c_str());
  }
  printf("Select a device (1-%zu): ", display_items.size());

  char input_buf[128];
  if (fgets(input_buf, sizeof(input_buf), stdin) == nullptr) {
    printf("Error reading input.\n");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  std::string_view input_sv(input_buf);
  while (!input_sv.empty() && std::isspace(static_cast<unsigned char>(input_sv.front()))) {
    input_sv.remove_prefix(1);
  }
  while (!input_sv.empty() && std::isspace(static_cast<unsigned char>(input_sv.back()))) {
    input_sv.remove_suffix(1);
  }

  if (input_sv.empty()) {
    printf("Invalid selection.\n");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  uint64_t selection;
  auto [p, ec] = std::from_chars(input_sv.data(), input_sv.data() + input_sv.size(), selection);
  if (ec != std::errc() || p != input_sv.data() + input_sv.size() || selection < 1 ||
      selection > display_items.size()) {
    printf("Invalid selection.\n");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(selection - 1);
}

std::vector<std::filesystem::directory_entry> GetEntriesInDirectory(std::string_view dir_path) {
  std::vector<std::filesystem::directory_entry> entries;
  std::error_code ec;
  std::filesystem::path path(dir_path);
  if (std::filesystem::exists(path, ec) && !ec) {
    for (auto it = std::filesystem::directory_iterator(path, ec);
         it != std::filesystem::directory_iterator() && !ec; it.increment(ec)) {
      entries.push_back(*it);
    }
    if (ec) {
      printf("Warning: directory iteration failed for '%.*s': %s\n",
             static_cast<int>(dir_path.size()), dir_path.data(), ec.message().c_str());
    }
  }
  return entries;
}

std::vector<DiscoveredDevice> ScanServiceDir(std::string_view service_dir,
                                             std::string_view member_to_check,
                                             std::string_view name_prefix = "",
                                             std::string_view name_member_to_connect = "") {
  std::vector<DiscoveredDevice> devices;
  for (const auto& entry : GetEntriesInDirectory(service_dir)) {
    std::filesystem::path path = entry.path();
    std::filesystem::path member_path = path / member_to_check;
    std::error_code ec;
    if (std::filesystem::exists(member_path, ec) && !ec) {
      std::string name;
      if (name_prefix.empty()) {
        name = path.filename().string();
      } else {
        name = std::string(name_prefix) + path.filename().string();
      }
      if (!name_member_to_connect.empty()) {
        std::filesystem::path name_member_path = path / name_member_to_connect;
        auto name_opt = TryGetSensorName(name_member_path.string());
        if (name_opt) {
          if (name_prefix.empty()) {
            name = *name_opt;
          } else {
            name = std::string(name_prefix) + *name_opt;
          }
        } else if (member_to_check == kServiceMemberDevice) {
          // Skip if we can't connect to temperature device member (matches original behavior)
          continue;
        }
      }
      devices.push_back({name, path.string(), /*is_service=*/true});
    }
  }
  return devices;
}

std::vector<DiscoveredDevice> ScanDevfsDir(std::string_view devfs_dir) {
  std::vector<DiscoveredDevice> devices;
  for (const auto& entry : GetEntriesInDirectory(devfs_dir)) {
    devices.push_back(
        {entry.path().filename().string(), entry.path().string(), /*is_service=*/false});
  }
  return devices;
}

std::vector<DiscoveredDevice> GetTemperatureDevices() {
  auto devices =
      ScanServiceDir(kTemperatureServiceDir, kServiceMemberDevice, "", kServiceMemberDevice);
  if (devices.empty()) {
    return ScanDevfsDir(kTemperatureDevfsDir);
  }
  return devices;
}

std::vector<DiscoveredDevice> GetTrippointDevices() {
  auto devices =
      ScanServiceDir(kTemperatureServiceDir, kServiceMemberTripPoint, "", kServiceMemberDevice);
  auto standalone = ScanServiceDir(kTrippointServiceDir, kServiceMemberTripPoint, "trippoint-");
  devices.insert(devices.end(), standalone.begin(), standalone.end());
  return devices;
}

std::vector<DiscoveredDevice> GetTrippointDebugDevices() {
  return ScanServiceDir(kTrippointDebugServiceDir, kServiceMemberDebug, "debug-");
}

std::vector<DiscoveredDevice> GetAdcDevices() {
  auto devices = ScanServiceDir(kAdcServiceDir, kServiceMemberDevice);
  if (devices.empty()) {
    return ScanDevfsDir(kAdcDevfsDir);
  }
  return devices;
}

zx::result<std::string> ResolveDeviceFromList(const std::vector<DiscoveredDevice>& devices,
                                              std::string_view provided_path_or_name,
                                              std::string_view command,
                                              std::string_view device_type_name,
                                              bool silent = false) {
  if (!provided_path_or_name.empty()) {
    for (const auto& dev : devices) {
      std::string instance_name = std::filesystem::path(dev.path).filename().string();
      if (dev.name == provided_path_or_name || instance_name == provided_path_or_name) {
        return zx::ok(GetPathForDevice(dev, command));
      }
    }
    if (!silent) {
      printf("Failed to resolve %.*s device name '%.*s'\n",
             static_cast<int>(device_type_name.size()), device_type_name.data(),
             static_cast<int>(provided_path_or_name.size()), provided_path_or_name.data());
    }
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (devices.empty()) {
    if (!silent) {
      printf("No %.*s devices found.\n", static_cast<int>(device_type_name.size()),
             device_type_name.data());
    }
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  if (devices.size() == 1) {
    std::string path = GetPathForDevice(devices[0], command);
    printf("Using %.*s device: %s (%s)\n", static_cast<int>(device_type_name.size()),
           device_type_name.data(), devices[0].name.c_str(), path.c_str());
    return zx::ok(path);
  }

  std::vector<std::string> display_items;
  display_items.reserve(devices.size());
  for (const auto& dev : devices) {
    display_items.push_back(dev.name + " (" + dev.path + ")");
  }

  auto selection_res = PromptUserSelection(display_items, device_type_name);
  if (selection_res.is_error()) {
    return selection_res.take_error();
  }
  size_t selection = selection_res.value();

  std::string path = GetPathForDevice(devices[selection], command);
  printf("Using %.*s device: %s (%s)\n", static_cast<int>(device_type_name.size()),
         device_type_name.data(), devices[selection].name.c_str(), path.c_str());
  return zx::ok(path);
}

zx::result<std::string> ResolveTemperatureDevice(std::string_view provided_path_or_name,
                                                 std::string_view command, bool silent = false) {
  return ResolveDeviceFromList(GetTemperatureDevices(), provided_path_or_name, command,
                               ToString(DeviceType::kTemperature), silent);
}

zx::result<std::string> ResolveTrippointDevice(std::string_view provided_path_or_name,
                                               std::string_view command) {
  bool is_trip = command == kCmdTrip;
  auto devices = is_trip ? GetTrippointDebugDevices() : GetTrippointDevices();
  std::string_view type_name = is_trip ? "trippoint debug" : ToString(DeviceType::kTrippoint);
  return ResolveDeviceFromList(devices, provided_path_or_name, command, type_name);
}

DeviceType DeduceDeviceTypeFromPathAndCommand(std::string_view path, std::string_view command) {
  if (path.starts_with(kAdcServiceDir) || path.starts_with(kAdcDevfsDir)) {
    return DeviceType::kAdc;
  }
  if (path.starts_with(kTrippointServiceDir) || path.starts_with(kTrippointDebugServiceDir)) {
    return DeviceType::kTrippoint;
  }
  if (path.starts_with(kTemperatureServiceDir)) {
    if (IsTrippointCommand(command)) {
      return DeviceType::kTrippoint;
    }
    return DeviceType::kTemperature;
  }
  if (path.starts_with(kTemperatureDevfsDir)) {
    return DeviceType::kTemperature;
  }

  // Fallback for arbitrary/unknown paths
  if (IsTrippointCommand(command)) {
    return DeviceType::kTrippoint;
  }
  if (command == kCmdResolution || command == kCmdReadNorm) {
    return DeviceType::kAdc;
  }
  if (command == kCmdRead) {
    if (path.find("adc") != std::string::npos) {
      return DeviceType::kAdc;
    }
    return DeviceType::kTemperature;
  }
  return DeviceType::kTemperature;
}

zx::result<std::string> ResolveAdcDevice(std::string_view provided_path_or_name,
                                         std::string_view command, bool silent = false) {
  return ResolveDeviceFromList(GetAdcDevices(), provided_path_or_name, command,
                               ToString(DeviceType::kAdc), silent);
}

}  // namespace

std::string_view ToString(DeviceType type) {
  switch (type) {
    case DeviceType::kTemperature:
      return "temperature";
    case DeviceType::kAdc:
      return "ADC";
    case DeviceType::kTrippoint:
      return "trippoint";
  }
}

bool IsInteger(std::string_view s) {
  return !s.empty() &&
         std::all_of(s.begin(), s.end(), [](unsigned char c) { return std::isdigit(c); });
}

bool IsKnownCommand(std::string_view arg) {
  // All commands except kCmdHelp
  static constexpr std::array kKnownCommands = {
      kCmdList, kCmdResolution, kCmdRead, kCmdReadNorm, kCmdTripPoint,
      kCmdWait, kCmdName,       kCmdTrip, kCmdReadAll,
  };
  return std::find(kKnownCommands.begin(), kKnownCommands.end(), arg) != kKnownCommands.end();
}

void do_list() {
  auto devices = GetTemperatureDevices();
  if (devices.empty()) {
    printf("No temperature devices found.\n");
    return;
  }
  printf("Found %zu temperature devices:\n", devices.size());
  for (const auto& dev : devices) {
    printf("  %-20s (%s)\n", dev.name.c_str(), dev.path.c_str());
  }
}

zx::result<ResolvedDevice> ResolveDevice(std::string_view provided_path_or_name,
                                         std::string_view command) {
  std::string path(provided_path_or_name);
  if (!path.empty() && path.starts_with('/')) {
    DeviceType dev_type = DeduceDeviceTypeFromPathAndCommand(path, command);
    // It's a path. Check if it needs a service member.
    for (auto service_dir : {
             kTemperatureServiceDir,
             kAdcServiceDir,
             kTrippointServiceDir,
             kTrippointDebugServiceDir,
         }) {
      if (path.starts_with(service_dir)) {
        if (!HasServiceMember(path)) {
          std::string_view member = GetMemberForService(service_dir, command);
          if (!member.empty()) {
            if (!path.ends_with('/')) {
              path += '/';
            }
            path += member;
          }
        }
        break;
      }
    }
    return zx::ok(ResolvedDevice{path, dev_type});
  }

  if (IsTrippointCommand(command)) {
    auto path_res = ResolveTrippointDevice(path, command);
    if (path_res.is_error())
      return path_res.take_error();
    return zx::ok(ResolvedDevice{path_res.value(), DeviceType::kTrippoint});
  }

  if (command == kCmdResolution || command == kCmdReadNorm) {
    auto path_res = ResolveAdcDevice(path, command);
    if (path_res.is_error())
      return path_res.take_error();
    return zx::ok(ResolvedDevice{path_res.value(), DeviceType::kAdc});
  }

  if (command == kCmdRead) {
    // Try Temperature first (silently to avoid noisy fallback prints)
    auto temp_res = ResolveTemperatureDevice(path, command, /*silent=*/true);
    if (temp_res.is_ok()) {
      return zx::ok(ResolvedDevice{temp_res.value(), DeviceType::kTemperature});
    }
    // Fallback to ADC
    auto adc_res = ResolveAdcDevice(path, command);
    if (adc_res.is_ok()) {
      return zx::ok(ResolvedDevice{adc_res.value(), DeviceType::kAdc});
    }
    return temp_res.take_error();
  }

  // Fallback for any other command (e.g., name or unknown commands)
  auto path_res = ResolveTemperatureDevice(path, command);
  if (path_res.is_error())
    return path_res.take_error();
  return zx::ok(ResolvedDevice{path_res.value(), DeviceType::kTemperature});
}

std::vector<DeviceInfo> GetTemperatureDevicesForReading() {
  auto devices = GetTemperatureDevices();
  std::vector<DeviceInfo> result;
  result.reserve(devices.size());
  for (const auto& dev : devices) {
    result.push_back({
        .name = dev.name,
        .path = GetPathForDevice(dev, kCmdRead),
    });
  }
  return result;
}
