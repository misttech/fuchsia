// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_POWER_BIN_BATTERYUTIL_BATTERYUTIL_H_
#define SRC_DEVICES_POWER_BIN_BATTERYUTIL_BATTERYUTIL_H_

#include <fidl/fuchsia.hardware.power.battery/cpp/wire.h>
#include <fidl/fuchsia.hardware.power.charger/cpp/wire.h>
#include <fidl/fuchsia.hardware.power.source/cpp/wire.h>
#include <fidl/fuchsia.power.battery/cpp/wire.h>
#include <lib/zx/result.h>

#include <string>
#include <vector>

// Represents the operational commands supported by the battery utility.
enum class BatteryFunc {
  // Fetches and prints out the battery's real-time telemetry (SoC, voltage, status).
  kGet,
  // Sets the generic charger block enabled/disabled via the corresponding charger protocol.
  kEnableCharger,
  // Forces the system power source over to USB or battery explicitly via raw SPMI/registers.
  kSetPowerSource,
  // Prints usage help text.
  kHelp,
};

struct CmdArgs {
  BatteryFunc func;
  std::string path;
  std::string value;
};

zx::result<CmdArgs> ParseArgs(int argc, char** argv);
zx::result<std::string> SelectInstance(const std::vector<std::string>& service_names);
zx::result<> GetBatteryInfoCmd(const std::string& path);
zx::result<> EnableChargerCmd(const std::string& path, bool enable);
zx::result<> SetPowerSource(const std::string& source);

#endif  // SRC_DEVICES_POWER_BIN_BATTERYUTIL_BATTERYUTIL_H_
