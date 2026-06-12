// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.adc/cpp/wire.h>
#include <fidl/fuchsia.hardware.temperature/cpp/wire.h>
#include <fidl/fuchsia.hardware.trippoint/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <stdio.h>

#include <charconv>
#include <cstdlib>
#include <string>
#include <string_view>
#include <vector>

#include "device_resolver.h"

constexpr char kUsageMessage[] =
    R"""(Usage: temperature-cli [device_path_or_name] <command> [args...]
       temperature-cli list
       temperature-cli readall
       temperature-cli --help

    [device_path_or_name] can be:
    - An absolute path (e.g., /dev/class/temperature/000 or /svc/fuchsia.hardware.temperature.Service/default)
    - A friendly name (e.g., soc-thermal)
    - A service instance name/hash (e.g., a2471f28e36fbe951476bce7910aa396)

    If [device_path_or_name] is omitted, the tool will automatically resolve it:
    - If only one device matches the command type, it will be used.
    - If multiple devices match, you will be prompted to select one.

    Commands:
        list             - List all temperature device paths and their friendly names

        (For temperature class devices)
        name             - Get sensor name
        read             - Read temperature in Celsius
                           If no command is specified, "read" is assumed.
        readall          - Read all temperature devices found

        (For ADC class devices)
        resolution       - Get ADC resolution
        read             - Read ADC sample
        readnorm         - Read normalized ADC sample [0.0-1.0]

        (For trippoint class devices)
        trippoint        - Get/set trippoint. Follow with multiple sets of index:type,configuration.
                           No spaces allowed.
                           index must be an integer.
                           type is allowed to be "above", "below".
                           configuration can be a float (celsius) or "cleared".
        wait             - Wait for a trippoint to be triggered
        trip             - Use debug service to trip a trippoint (requires index arg)

    Examples:
        temperature-cli list
        temperature-cli readall
        temperature-cli LITTLE
        temperature-cli soc-thermal read
        temperature-cli soc-thermal name
        temperature-cli /dev/class/adc/000 read
        temperature-cli /svc/fuchsia.hardware.trippoint.Service/default/trippoint trippoint
        temperature-cli /svc/fuchsia.hardware.trippoint.Service/default/trippoint trippoint 0:below,4.2 1:above,cleared
)""";

namespace FidlTemperature = fuchsia_hardware_temperature;
namespace FidlAdc = fuchsia_hardware_adc;
namespace FidlTrippoint = fuchsia_hardware_trippoint;

// Trippoint Configs
constexpr char kTripTypeAbove[] = "above";
constexpr char kTripTypeBelow[] = "below";
constexpr char kTripConfigCleared[] = "cleared";

namespace {

// Not commands, but additional flags that map to kCmdHelp
constexpr std::string_view kShortFlagHelp = "-h";
constexpr std::string_view kLongFlagHelp = "--help";

struct CmdArgs {
  std::string device_path_or_name;
  std::string command;
  std::vector<std::string> extra_args;
};

zx::result<CmdArgs> ParseArgs(int argc, char** argv) {
  CmdArgs args;
  if (argc < 2) {
    return zx::ok(args);
  }

  std::string_view argv1(argv[1]);
  if (argv1 == kCmdHelp || argv1 == kShortFlagHelp || argv1 == kLongFlagHelp) {
    args.command = kCmdHelp;
    return zx::ok(args);
  }

  if (IsKnownCommand(argv[1])) {
    args.command = argv[1];
    for (int i = 2; i < argc; ++i) {
      args.extra_args.push_back(argv[i]);
    }
  } else {
    args.device_path_or_name = argv[1];
    if (argc >= 3) {
      args.command = argv[2];
      for (int i = 3; i < argc; ++i) {
        args.extra_args.push_back(argv[i]);
      }
    } else {
      args.command = kCmdRead;
    }
  }
  return zx::ok(args);
}

std::string ExpectedTypeForCommand(std::string_view command) {
  if (command == kCmdRead) {
    return std::string(ToString(DeviceType::kTemperature)) + " or " +
           std::string(ToString(DeviceType::kAdc));
  }
  if (command == kCmdName) {
    return std::string(ToString(DeviceType::kTemperature));
  }
  if (command == kCmdResolution || command == kCmdReadNorm) {
    return std::string(ToString(DeviceType::kAdc));
  }
  if (command == kCmdTripPoint || command == kCmdWait || command == kCmdTrip) {
    return std::string(ToString(DeviceType::kTrippoint));
  }
  return "unknown";
}

std::string ToString(const FidlTrippoint::wire::TripPointType& type) {
  switch (type) {
    case FidlTrippoint::TripPointType::kOneshotTempAbove:
      return "OneshotTempAbove";
    case FidlTrippoint::TripPointType::kOneshotTempBelow:
      return "OneshotTempBelow";
    default:
      return "Unknown";
  }
}

std::string ToString(const FidlTrippoint::wire::TripPointValue& value) {
  switch (value.Which()) {
    case FidlTrippoint::wire::TripPointValue::Tag::kClearedTripPoint:
      return "ClearedTripPoint";
    case FidlTrippoint::wire::TripPointValue::Tag::kOneshotTempAboveTripPoint:
      return "OneshotTempAboveTripPoint(" +
             std::to_string(value.oneshot_temp_above_trip_point().critical_temperature_celsius) +
             ")";
    case FidlTrippoint::wire::TripPointValue::Tag::kOneshotTempBelowTripPoint:
      return "OneshotTempBelowTripPoint(" +
             std::to_string(value.oneshot_temp_below_trip_point().critical_temperature_celsius) +
             ")";
    default:
      return "Unknown";
  }
}

void print_trippoint(
    const fidl::VectorView<FidlTrippoint::wire::TripPointDescriptor>& descriptors) {
  for (const auto& trippoint : descriptors) {
    printf("{\n");
    printf("   .index = %d,\n", trippoint.index);
    printf("   .type = %s,\n", ToString(trippoint.type).c_str());
    printf("   .configuration = %s,\n", ToString(trippoint.configuration).c_str());
    printf("},\n");
  }
}

std::vector<FidlTrippoint::wire::TripPointDescriptor> parse_set_trippoints_args(
    const std::vector<std::string>& extra_args) {
  std::vector<FidlTrippoint::wire::TripPointDescriptor> descriptors;
  for (const auto& arg : extra_args) {
    std::string_view trippoint(arg);
    auto delim = trippoint.find(':');
    if (delim == std::string_view::npos) {
      return {};
    }
    std::string_view index = trippoint.substr(0, delim);
    trippoint.remove_prefix(delim + 1);
    delim = trippoint.find(',');
    if (delim == std::string_view::npos) {
      return {};
    }
    std::string_view type = trippoint.substr(0, delim);
    trippoint.remove_prefix(delim + 1);
    std::string_view configuration = trippoint;

    if (!IsInteger(index)) {
      return {};
    }

    uint32_t index_val;
    auto [p1, ec1] = std::from_chars(index.data(), index.data() + index.size(), index_val);
    if (ec1 != std::errc()) {
      return {};
    }

    FidlTrippoint::wire::TripPointDescriptor desc;
    if (type == kTripTypeAbove) {
      if (configuration == kTripConfigCleared) {
        desc = {
            .type = FidlTrippoint::wire::TripPointType::kOneshotTempAbove,
            .index = index_val,
            .configuration = FidlTrippoint::wire::TripPointValue::WithClearedTripPoint(
                FidlTrippoint::wire::ClearedTripPoint()),
        };
      } else {
        float config_val;
        auto [p2, ec2] = std::from_chars(configuration.data(),
                                         configuration.data() + configuration.size(), config_val);
        if (ec2 != std::errc() || p2 != configuration.data() + configuration.size()) {
          return {};
        }
        desc = {
            .type = FidlTrippoint::wire::TripPointType::kOneshotTempAbove,
            .index = index_val,
            .configuration = FidlTrippoint::wire::TripPointValue::WithOneshotTempAboveTripPoint(
                FidlTrippoint::wire::OneshotTempAboveTripPoint(config_val)),
        };
      }
    } else if (type == kTripTypeBelow) {
      if (configuration == kTripConfigCleared) {
        desc = {
            .type = FidlTrippoint::wire::TripPointType::kOneshotTempBelow,
            .index = index_val,
            .configuration = FidlTrippoint::wire::TripPointValue::WithClearedTripPoint(
                FidlTrippoint::wire::ClearedTripPoint()),
        };
      } else {
        float config_val;
        auto [p2, ec2] = std::from_chars(configuration.data(),
                                         configuration.data() + configuration.size(), config_val);
        if (ec2 != std::errc() || p2 != configuration.data() + configuration.size()) {
          return {};
        }
        desc = {
            .type = FidlTrippoint::wire::TripPointType::kOneshotTempBelow,
            .index = index_val,
            .configuration = FidlTrippoint::wire::TripPointValue::WithOneshotTempBelowTripPoint(
                FidlTrippoint::wire::OneshotTempBelowTripPoint(config_val)),
        };
      }
    } else {
      return {};
    }

    descriptors.emplace_back(desc);
  }
  return descriptors;
}

int HandleTemperatureRead(std::string_view device_path) {
  auto client_res =
      ConnectToDevice<FidlTemperature::Device>(device_path, ToString(DeviceType::kTemperature));
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  auto response = client->GetTemperatureCelsius();
  if (response.ok()) {
    if (!response->status) {
      printf("temperature = %f\n", response->temp);
    } else {
      printf("GetTemperatureCelsius failed: status = %d\n", response->status);
      return -1;
    }
  } else {
    printf("GetTemperatureCelsius FIDL call failed: %s\n", response.status_string());
    return -1;
  }
  return 0;
}

int HandleTemperatureName(std::string_view device_path) {
  auto client_res =
      ConnectToDevice<FidlTemperature::Device>(device_path, ToString(DeviceType::kTemperature));
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  auto response = client->GetSensorName();
  if (response.ok()) {
    printf("Sensor Name = %.*s\n", static_cast<int>(response->name.size()), response->name.data());
  } else {
    printf("GetSensorName FIDL call failed: %s\n", response.status_string());
    return -1;
  }
  return 0;
}

int HandleAdcResolution(std::string_view device_path) {
  auto client_res = ConnectToDevice<FidlAdc::Device>(device_path, ToString(DeviceType::kAdc));
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  auto response = client->GetResolution();
  if (response.ok()) {
    if (response->is_error()) {
      printf("GetResolution failed: status = %d\n", response->error_value());
      return -1;
    }
    printf("adc resolution = %u\n", response->value()->resolution);
  } else {
    printf("GetResolution FIDL call failed: %s\n", response.status_string());
    return -1;
  }
  return 0;
}

int HandleAdcRead(std::string_view device_path) {
  auto client_res = ConnectToDevice<FidlAdc::Device>(device_path, ToString(DeviceType::kAdc));
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  auto response = client->GetSample();
  if (response.ok()) {
    if (response->is_error()) {
      printf("GetSample failed: status = %d\n", response->error_value());
      return -1;
    }
    printf("Value = %u\n", response->value()->value);
  } else {
    printf("GetSample FIDL call failed: %s\n", response.status_string());
    return -1;
  }
  return 0;
}

int HandleAdcReadNorm(std::string_view device_path) {
  auto client_res = ConnectToDevice<FidlAdc::Device>(device_path, ToString(DeviceType::kAdc));
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  auto response = client->GetNormalizedSample();
  if (response.ok()) {
    if (response->is_error()) {
      printf("GetNormalizedSample failed: status = %d\n", response->error_value());
      return -1;
    }
    printf("Value = %f\n", response->value()->value);
  } else {
    printf("GetNormalizedSample FIDL call failed: %s\n", response.status_string());
    return -1;
  }
  return 0;
}

int HandleTripPoint(std::string_view device_path, const std::vector<std::string>& extra_args) {
  auto client_res =
      ConnectToDevice<FidlTrippoint::TripPoint>(device_path, ToString(DeviceType::kTrippoint));
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  if (extra_args.empty()) {
    auto response = client->GetTripPointDescriptors();
    if (response.ok()) {
      if (response->is_error()) {
        printf("GetTripPointDescriptors failed: status = %d\n", response->error_value());
        return -1;
      }
      print_trippoint(response->value()->descriptors);
    } else {
      printf("GetTripPointDescriptors FIDL call failed: %s\n", response.status_string());
      return -1;
    }
  } else {
    std::vector descriptors = parse_set_trippoints_args(extra_args);
    if (descriptors.empty()) {
      printf("Invalid trippoints list\n");
      return -1;
    }
    auto fidl_descriptors =
        fidl::VectorView<FidlTrippoint::wire::TripPointDescriptor>::FromExternal(descriptors);
    printf("Setting trippoints:\n");
    print_trippoint(fidl_descriptors);
    auto response = client->SetTripPoints(fidl_descriptors);
    if (response.ok()) {
      if (response->is_error()) {
        printf("SetTripPoints failed: status = %d\n", response->error_value());
        return -1;
      }
    } else {
      printf("SetTripPoints FIDL call failed: %s\n", response.status_string());
      return -1;
    }
  }
  return 0;
}

int HandleWait(std::string_view device_path) {
  auto client_res =
      ConnectToDevice<FidlTrippoint::TripPoint>(device_path, ToString(DeviceType::kTrippoint));
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  auto response = client->WaitForAnyTripPoint();
  if (response.ok()) {
    if (response->is_error()) {
      printf("WaitForAnyTripPoint failed: status = %d\n", response->error_value());
      return -1;
    }
    printf("TripPoint indexed %u was tripped. Measured temperature was %f C\n",
           response->value()->result.index, response->value()->result.measured_temperature_celsius);
  } else {
    printf("WaitForAnyTripPoint FIDL call failed: %s\n", response.status_string());
    return -1;
  }
  return 0;
}

int HandleTrip(std::string_view device_path, const std::vector<std::string>& extra_args) {
  auto client_res = ConnectToDevice<FidlTrippoint::Debug>(device_path, "trippoint debug");
  if (client_res.is_error()) {
    return -1;
  }
  auto client = std::move(client_res.value());
  if (extra_args.empty()) {
    printf("trip command requires an index argument\n");
    return -1;
  }
  uint32_t index_val;
  std::string_view index_str(extra_args[0]);
  auto [p, ec] = std::from_chars(index_str.data(), index_str.data() + index_str.size(), index_val);
  if (ec != std::errc() || p != index_str.data() + index_str.size()) {
    printf("Invalid index '%s'. Index must be a non-negative integer.\n", extra_args[0].c_str());
    return -1;
  }
  auto response = client->Trip(index_val);
  if (!response.ok()) {
    printf("Trip FIDL call failed: %s\n", response.status_string());
    return -1;
  }
  return 0;
}

}  // namespace

int main(int argc, char** argv) {
  auto args_res = ParseArgs(argc, argv);
  if (args_res.is_error()) {
    return -1;
  }
  const auto& args = args_res.value();

  if (args.command.empty()) {
    printf("%s", kUsageMessage);
    return -1;
  }

  if (args.command == kCmdHelp) {
    printf("%s", kUsageMessage);
    return 0;
  }

  if (args.command == kCmdList) {
    if (!args.extra_args.empty()) {
      printf("%.*s: additional arguments will be ignored\n", static_cast<int>(kCmdList.size()),
             kCmdList.data());
    }
    do_list();
    return 0;
  }

  if (args.command == kCmdReadAll) {
    if (!args.extra_args.empty()) {
      printf("%.*s: additional arguments will be ignored\n", static_cast<int>(kCmdReadAll.size()),
             kCmdReadAll.data());
    }
    auto devices = GetTemperatureDevicesForReading();
    if (devices.empty()) {
      printf("No temperature devices found.\n");
      return 0;
    }
    printf("Found %zu temperature devices:\n", devices.size());
    for (const auto& dev : devices) {
      printf("  %-20s (%s)\n", dev.name.c_str(), dev.path.c_str());
    }
    printf("\n");
    int ret = 0;
    for (const auto& dev : devices) {
      printf("Reading %s ...\n", dev.name.c_str());
      if (HandleTemperatureRead(dev.path) != 0) {
        ret = -1;
      }
    }
    return ret;
  }

  auto resolved_res = ResolveDevice(args.device_path_or_name, args.command);
  if (resolved_res.is_error()) {
    return -1;
  }
  const auto& resolved = resolved_res.value();

  if (args.command == kCmdRead) {
    if (resolved.type == DeviceType::kTemperature) {
      return HandleTemperatureRead(resolved.path);
    }
    if (resolved.type == DeviceType::kAdc) {
      return HandleAdcRead(resolved.path);
    }
  } else if (args.command == kCmdName) {
    if (resolved.type == DeviceType::kTemperature) {
      return HandleTemperatureName(resolved.path);
    }
  } else if (args.command == kCmdResolution) {
    if (resolved.type == DeviceType::kAdc) {
      return HandleAdcResolution(resolved.path);
    }
  } else if (args.command == kCmdReadNorm) {
    if (resolved.type == DeviceType::kAdc) {
      return HandleAdcReadNorm(resolved.path);
    }
  } else if (args.command == kCmdTripPoint) {
    if (resolved.type == DeviceType::kTrippoint) {
      return HandleTripPoint(resolved.path, args.extra_args);
    }
  } else if (args.command == kCmdWait) {
    if (resolved.type == DeviceType::kTrippoint) {
      return HandleWait(resolved.path);
    }
  } else if (args.command == kCmdTrip) {
    if (resolved.type == DeviceType::kTrippoint) {
      return HandleTrip(resolved.path, args.extra_args);
    }
  } else {
    printf("Unknown command: %s\n", args.command.c_str());
    printf("%s", kUsageMessage);
    return -1;
  }

  // If we reached here, the command was known but the resolved device type is incompatible.
  printf("Incompatible device type for command '%s'. Expected %s, detected %.*s.\n",
         args.command.c_str(), ExpectedTypeForCommand(args.command).c_str(),
         static_cast<int>(ToString(resolved.type).size()), ToString(resolved.type).data());
  return -1;
}
