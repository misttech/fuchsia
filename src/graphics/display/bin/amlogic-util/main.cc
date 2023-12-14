// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.amlogicdisplay/cpp/wire.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/incoming/cpp/service.h>
#include <zircon/fidl.h>
#include <zircon/status.h>
#include <zircon/types.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <optional>
#include <string>
#include <vector>

#include "src/lib/fxl/command_line.h"

struct Args {
  std::string hw;
  bool state;
};

std::optional<Args> ParseArgs(int argc, char* const argv[]) {
  fxl::CommandLine command_line = fxl::CommandLineFromArgcArgv(argc, argv);
  std::vector<std::string> positional_args = command_line.positional_args();
  if (positional_args.size() != 2) {
    return std::nullopt;
  }
  std::string hw = positional_args[0];
  std::string state = positional_args[1];

  static const std::vector<std::string> kValidHws = {"vsync", "vout", "all"};
  if (std::find(kValidHws.begin(), kValidHws.end(), hw) == kValidHws.end()) {
    return std::nullopt;
  }

  if (state != "on" && state != "off") {
    return std::nullopt;
  }

  return Args{
      .hw = hw,
      .state = state == "on",
  };
}

int main(int argc, char* argv[]) {
  std::optional<Args> args = ParseArgs(argc, argv);
  if (!args.has_value()) {
    fprintf(stderr, "invalid arguments. usage: amlogic-util <vsync|vout|all> <on|off>\n");
    return -1;
  }

  const std::string dev_path = "/dev/sys/platform/00:00:1e/dw-dsi/display/amlogic-display";
  zx::result result = component::Connect<fuchsia_hardware_amlogicdisplay::Device>(dev_path);
  if (result.is_error()) {
    printf("Could not create channel (%d)\n", result.error_value());
    return -1;
  }

  fidl::WireSyncClient client(std::move(result.value()));

  if (args->hw == "vsync") {
    const fidl::WireResult result = client->SetVsync(args->state);
    if (!result.ok()) {
      printf("SetVsync FIDL error (%s)\n", result.status_string());
      return -1;
    }
    if (result.value().is_error()) {
      printf("SetVsync error (%s)\n", zx_status_get_string(result->error_value()));
      return -1;
    }
  }

  if (args->hw == "vout") {
    const fidl::WireResult result = client->SetVoutPower(args->state);
    if (!result.ok()) {
      printf("SetVoutPower FIDL error (%s)\n", result.status_string());
      return -1;
    }
    if (result.value().is_error()) {
      printf("SetVoutPower error (%s)\n", zx_status_get_string(result->error_value()));
      return -1;
    }
  }

  if (args->hw == "all") {
    const fidl::WireResult result = client->SetDisplayEnginePower(args->state);
    if (!result.ok()) {
      printf("SetDisplayEnginePower FIDL error (%s)\n", result.status_string());
      return -1;
    }
    if (result.value().is_error()) {
      printf("SetDisplayEnginePower error (%s)\n", zx_status_get_string(result->error_value()));
      return -1;
    }
  }

  return 0;
}
