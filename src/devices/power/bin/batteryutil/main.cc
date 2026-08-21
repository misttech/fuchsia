// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>

#include "batteryutil.h"

namespace fbattery = fuchsia_hardware_power_battery;
namespace fcharger = fuchsia_hardware_power_charger;
namespace fpowerbattery = fuchsia_power_battery;

static void usage() {
  printf(
      "Usage: batteryutil [device_path] <command>\n\n"
      "Get battery information.\n\n"
      "Commands:\n"
      "  get             Get the current battery info.\n"
      "  enable <1/0>    Enable or disable the charger.\n"
      "  power <battery|usb>  Set power source (force discharge or allow charging).\n"
      "  help | h        Print this help text.\n\n"
      "Examples:\n"
      "  Get battery info:\n"
      "  $ batteryutil get\n"
      "  $ batteryutil /svc/fuchsia.power.battery.InfoService/... get\n\n"
      "  Enable the charger:\n"
      "  $ batteryutil enable 1\n"
      "  $ batteryutil /svc/fuchsia.power.battery.ChargerService/... enable 1\n\n"
      "  Force battery power (suspend USB input):\n"
      "  $ batteryutil power battery\n\n"
      "  Allow USB power (resume USB input):\n"
      "  $ batteryutil power usb\n");
}

int main(int argc, char** argv) {
  auto print_usage = fit::defer([]() { usage(); });

  zx::result<CmdArgs> args_result = ParseArgs(argc, argv);
  if (args_result.is_error()) {
    fprintf(stderr, "Unable to parse arguments! %s\n\n", args_result.status_string());
    return 1;
  }
  CmdArgs args = args_result.value();
  if (args.func == BatteryFunc::kHelp) {
    return 0;
  }

  // Cancel usage printing for runtime errors to avoid spamming usage when arguments were parsed
  // correctly.
  print_usage.cancel();

  zx::result<std::string> device_path = zx::ok("");
  if (args.func != BatteryFunc::kSetPowerSource) {
    device_path =
        !args.path.empty()
            ? zx::ok(args.path)
            : SelectInstance(args.func == BatteryFunc::kGet
                                 ? std::vector<std::string>{fbattery::Service::Name,
                                                            fpowerbattery::InfoService::Name}
                                 : std::vector<std::string>{fcharger::Service::Name,
                                                            fpowerbattery::ChargerService::Name});

    if (device_path.is_error()) {
      return 1;
    }
  }
  std::string path = device_path.value();

  switch (args.func) {
    case BatteryFunc::kHelp:
      return 0;
    case BatteryFunc::kGet:
      return GetBatteryInfoCmd(path).is_ok() ? 0 : 1;
    case BatteryFunc::kEnableCharger:
      return EnableChargerCmd(path, args.value == "1").is_ok() ? 0 : 1;
    case BatteryFunc::kSetPowerSource:
      return SetPowerSource(args.value).is_ok() ? 0 : 1;
  }
  return 0;
}
