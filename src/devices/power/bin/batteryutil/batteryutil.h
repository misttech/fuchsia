// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_POWER_BIN_BATTERYUTIL_BATTERYUTIL_H_
#define SRC_DEVICES_POWER_BIN_BATTERYUTIL_BATTERYUTIL_H_

#include <fidl/fuchsia.power.battery/cpp/fidl.h>
#include <lib/zx/result.h>

enum class BatteryFunc {
  kGet,
  kEnableCharger,
  kSetPowerSource,
};

struct CmdArgs {
  BatteryFunc func;
  std::string path;
  std::string value;
};

zx::result<CmdArgs> ParseArgs(int argc, char** argv);
zx::result<std::string> ResolveServicePath(const std::string& provided_path, BatteryFunc func);
void PrintBatteryInfo(const fuchsia_power_battery::wire::BatteryInfo& info);
zx::result<> SetPowerSource(const std::string& source);

#endif  // SRC_DEVICES_POWER_BIN_BATTERYUTIL_BATTERYUTIL_H_
