// Copyright 2016 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.hardware.power.statecontrol/cpp/fidl.h>
#include <fidl/fuchsia.kernel/cpp/fidl.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include <zircon/status.h>

static int print_dm_help() {
  printf(
      "poweroff             - power off the system\n"
      "shutdown             - power off the system\n"
      "reboot               - reboot the system\n"
      "reboot-bootloader/rb - reboot the system into bootloader\n"
      "reboot-recovery/rr   - reboot the system into recovery\n");
  return 0;
}

static int send_statecontrol_admin_command(
    fuchsia_hardware_power_statecontrol::ShutdownAction action) {
  auto client_end = component::Connect<fuchsia_hardware_power_statecontrol::Admin>();
  if (client_end.is_error()) {
    return client_end.status_value();
  }
  auto client = fidl::WireSyncClient(std::move(*client_end));

  fidl::Arena arena;
  auto builder = fuchsia_hardware_power_statecontrol::wire::ShutdownOptions::Builder(arena);
  fuchsia_hardware_power_statecontrol::ShutdownReason reasons[] = {
      fuchsia_hardware_power_statecontrol::ShutdownReason::kDeveloperRequest};
  auto vector_view =
      fidl::VectorView<fuchsia_hardware_power_statecontrol::ShutdownReason>::FromExternal(reasons);
  builder.reasons(vector_view);
  builder.action(action);
  auto response = client->Shutdown(builder.Build());

  if (response.status() != ZX_OK) {
    printf("Command failed: %s (%d)\n", response.status_string(), response.status());
    return -1;
  }

  if (response->is_error()) {
    printf("Command failed: %d\n", response->error_value());
  }

  return 0;
}

static bool command_cmp(const char* long_command, const char* short_command, const char* input,
                        int* command_length) {
  const size_t input_length = strlen(input);

  // Ensure that the first command_length chars of input match and that it is
  // either the whole input or there is a space after the command, we don't want
  // partial command matching.
  if (short_command) {
    const size_t short_length = strlen(short_command);
    if (input_length >= short_length && strncmp(short_command, input, short_length) == 0 &&
        ((input_length == short_length) || input[short_length] == ' ')) {
      *command_length = short_length;
      return true;
    }
  }

  const size_t long_length = strlen(long_command);
  if (input_length >= long_length && strncmp(long_command, input, long_length) == 0 &&
      ((input_length == long_length) || input[long_length] == ' ')) {
    *command_length = long_length;
    return true;
  }
  return false;
}

__BEGIN_CDECLS
int zxc_dm(int argc, char** argv) {
  if (argc != 2) {
    printf("usage: dm <command>\n");
    return -1;
  }

  // Handle service backed commands.
  int command_length = 0;
  if (command_cmp("help", NULL, argv[1], &command_length)) {
    return print_dm_help();

  } else if (command_cmp("reboot", NULL, argv[1], &command_length)) {
    return send_statecontrol_admin_command(
        fuchsia_hardware_power_statecontrol::ShutdownAction::kReboot);

  } else if (command_cmp("reboot-bootloader", "rb", argv[1], &command_length)) {
    return send_statecontrol_admin_command(
        fuchsia_hardware_power_statecontrol::ShutdownAction::kRebootToBootloader);

  } else if (command_cmp("reboot-recovery", "rr", argv[1], &command_length)) {
    return send_statecontrol_admin_command(
        fuchsia_hardware_power_statecontrol::ShutdownAction::kRebootToRecovery);

  } else if (command_cmp("poweroff", NULL, argv[1], &command_length) ||
             command_cmp("shutdown", NULL, argv[1], &command_length)) {
    return send_statecontrol_admin_command(
        fuchsia_hardware_power_statecontrol::ShutdownAction::kPoweroff);

  } else {
    printf("Unknown command '%s'\n\n", argv[1]);
    printf("Valid commands:\n");
    print_dm_help();
  }

  return -1;
}
__END_CDECLS
