// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/fit/defer.h>

#include "batteryutil.h"

static void usage() {
  printf(
      "Usage: batteryutil <device_path> <command>\n\n"
      "Get battery information.\n\n"
      "Commands:\n"
      "  get             Get the current battery info.\n"
      "  enable <1/0>    Enable or disable the charger.\n"
      "  help | h        Print this help text.\n\n"
      "Examples:\n"
      "  Get battery info:\n"
      "  $ batteryutil /svc/fuchsia.power.battery.InfoService/... get\n\n"
      "  Enable the charger:\n"
      "  $ batteryutil /svc/fuchsia.power.battery.ChargerService/... enable 1\n");
}

int main(int argc, char** argv) {
  auto print_usage = fit::defer([]() { usage(); });

  zx::result func = ParseArgs(argc, argv);
  if (func.is_error()) {
    fprintf(stderr, "Unable to parse arguments! %s\n\n", func.status_string());
    return 1;
  }

  switch (*func) {
    case BatteryFunc::kGet: {
      zx::result client_end = component::Connect<fuchsia_power_battery::BatteryInfoProvider>(
          std::string(argv[1]) + "/device");
      if (client_end.is_error()) {
        fprintf(stderr, "Could not connect to BatteryInfoProvider: %s\n",
                client_end.status_string());
        return 1;
      }

      auto result = fidl::WireCall(client_end.value())->GetBatteryInfo();
      if (!result.ok()) {
        fprintf(stderr, "Call to get battery info failed: %s\n",
                result.FormatDescription().c_str());
        return 1;
      }
      PrintBatteryInfo(result.value().info);
      break;
    }
    case BatteryFunc::kEnableCharger: {
      zx::result client_end =
          component::Connect<fuchsia_power_battery::Charger>(std::string(argv[1]) + "/device");
      if (client_end.is_error()) {
        fprintf(stderr, "Could not connect to Charger: %s\n", client_end.status_string());
        return 1;
      }

      std::string_view arg = argv[3];
      auto result = fidl::WireCall(client_end.value())->Enable(arg == "1");
      if (!result.ok()) {
        fprintf(stderr, "Call to enable charger failed: %s\n", result.FormatDescription().c_str());
        return 1;
      }
      if (result->is_error()) {
        fprintf(stderr, "Could not enable charger: %d\n", result->error_value());
        return 1;
      }
      break;
    }
    default:
      fprintf(stderr, "Invalid function\n");
      return 1;
  }
  print_usage.cancel();
  return 0;
}
