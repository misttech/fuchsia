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
};

zx::result<BatteryFunc> ParseArgs(int argc, char** argv);
void PrintBatteryInfo(const fuchsia_power_battery::wire::BatteryInfo& info);

#endif  // SRC_DEVICES_POWER_BIN_BATTERYUTIL_BATTERYUTIL_H_
