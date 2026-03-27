// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "resetctl.h"

#include <lib/stdformat/print.h>

namespace resetctl {

void PrintUsage(const char* binary_name) {
  cpp23::println(stderr, "Usage: {} <instance_name> <subcommand> [args]", binary_name);
  cpp23::println(stderr, "Subcommands:");
  cpp23::println(stderr, "  assert   - Assert the reset line");
  cpp23::println(stderr, "             Example: {} <instance_name> assert", binary_name);
  cpp23::println(stderr, "  deassert - Deassert the reset line");
  cpp23::println(stderr, "             Example: {} <instance_name> deassert", binary_name);
  cpp23::println(stderr, "  toggle   - Toggle the reset line (optional timeout in ns)");
  cpp23::println(stderr, "             Example: {} <instance_name> toggle", binary_name);
  cpp23::println(stderr, "             Example: {} <instance_name> toggle 1000", binary_name);
  cpp23::println(stderr, "  status   - Get the reset status");
  cpp23::println(stderr, "             Example: {} <instance_name> status", binary_name);
}

zx::result<> Run(int argc, const char** argv,
                 fidl::ClientEnd<fuchsia_hardware_reset::Reset> client_end) {
  if (argc < 2) {
    cpp23::println(stderr, "Subcommand missing.");
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  fidl::WireSyncClient client(std::move(client_end));
  const char* subcommand = argv[1];

  if (strcmp(subcommand, "assert") == 0) {
    auto result = client->Assert();
    if (!result.ok()) {
      return zx::error(result.status());
    }
    if (result->is_error()) {
      return zx::error(result->error_value());
    }
    return zx::ok();
  } else if (strcmp(subcommand, "deassert") == 0) {
    auto result = client->Deassert();
    if (!result.ok()) {
      return zx::error(result.status());
    }
    if (result->is_error()) {
      return zx::error(result->error_value());
    }
    return zx::ok();
  } else if (strcmp(subcommand, "toggle") == 0) {
    if (argc > 2) {
      uint64_t timeout_ns = strtoull(argv[2], nullptr, 10);
      auto result = client->ToggleWithTimeout(timeout_ns);
      if (!result.ok()) {
        return zx::error(result.status());
      }
      if (result->is_error()) {
        return zx::error(result->error_value());
      }
    } else {
      auto result = client->Toggle();
      if (!result.ok()) {
        return zx::error(result.status());
      }
      if (result->is_error()) {
        return zx::error(result->error_value());
      }
    }
    return zx::ok();
  } else if (strcmp(subcommand, "status") == 0) {
    auto result = client->Status();
    if (!result.ok()) {
      return zx::error(result.status());
    }
    if (result->is_error()) {
      return zx::error(result->error_value());
    }
    cpp23::println("Asserted: {}", result->value()->asserted ? "true" : "false");
    return zx::ok();
  } else {
    cpp23::println(stderr, "Unknown subcommand: {}", subcommand);
    return zx::error(ZX_ERR_NOT_SUPPORTED);
  }
}

}  // namespace resetctl
