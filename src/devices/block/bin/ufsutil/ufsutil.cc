// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ufsutil.h"

#include <getopt.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <zircon/errors.h>

#include <cstdlib>

#include "query.h"

namespace ufsutil {
namespace {
#define QUERY_REQUEST_COMMAND_USAGE_MESSAGE(command)                    \
  "Usage: ufsutil <device> " command                                    \
  " [options]\n"                                                        \
  "options:\n"                                                          \
  "  [--type=<NUM>, -t <NUM>]        Descriptor type idn. (required)\n" \
  "  [--index=<NUM>, -i <NUM>]       (default = 0)\n"                   \
  "  [--selector=<NUM>, -s <NUM>]    (default = 0)\n"

constexpr char kReadDescUsageMessage[] = QUERY_REQUEST_COMMAND_USAGE_MESSAGE("read-desc");

constexpr char kWriteDescUsageMessage[] = R"""(
Usage: ufsutil <device> write-desc [options]
options:
  [--type=<NUM>, -t <NUM>]        Descriptor type idn.
  [--index=<NUM>, -i <NUM>]       (default = 0)
  [--selector=<NUM>, -s <NUM>]    (default = 0)
  [--value=<NUM>, -v <NUM>]
  [--file=<FILE_PATH>, -f <FILE_PATH>]  (required by the configuration descriptor)
)""";

using CommandHandler = std::function<int(const fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>&,
                                         const std::unordered_map<uint32_t, OptionValue>&)>;

const char* kQueryRequestShortOptions = "ht:i:s:";
const struct option kQueryRequestOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "type", .has_arg = required_argument, .flag = nullptr, .val = 't'},
    {.name = "index", .has_arg = required_argument, .flag = nullptr, .val = 'i'},
    {.name = "selector", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

const char* kWriteDescriptorShortOptions = "ht:i:s:v:f:";
const struct option kWriteDescriptorOptions[] = {
    {.name = "help", .has_arg = no_argument, .flag = nullptr, .val = 'h'},
    {.name = "type", .has_arg = required_argument, .flag = nullptr, .val = 't'},
    {.name = "index", .has_arg = required_argument, .flag = nullptr, .val = 'i'},
    {.name = "selector", .has_arg = required_argument, .flag = nullptr, .val = 's'},
    {.name = "value", .has_arg = required_argument, .flag = nullptr, .val = 'v'},
    {.name = "file", .has_arg = required_argument, .flag = nullptr, .val = 'f'},
    {.name = nullptr, .has_arg = 0, .flag = nullptr, .val = 0},
};

enum class UfsCommand : uint8_t {
  READ_DESC,
  WRITE_DESC,
  READ_FLAG,
  SET_FLAG,
  CLEAR_FLAG,
  TOGGLE_FLAG,
  READ_ATTR,
  WRITE_ATTR,
};

struct CommandDefinition {
  UfsCommand command_type;
  const char* description;
  const struct option* long_opts;
  const char* short_opts;
  CommandHandler handler;
  const char* helpMessage;
};

struct ParsedCommand {
  std::string_view name;
  std::unordered_map<uint32_t, OptionValue> options;
  CommandHandler handler;
};

std::map<std::string, CommandDefinition> commandRegistry;

void registerCommand(const std::string& cmd_name, const CommandDefinition& cmd) {
  commandRegistry[cmd_name] = cmd;
}

std::optional<uint32_t> ParseStrToUint32(const char* str) {
  char* endptr;
  uint64_t ret = std::strtoul(str, &endptr, 0);
  if (endptr == str || *endptr != '\0') {
    return std::nullopt;
  }
  if (ret > UINT32_MAX) {
    return std::nullopt;
  }
  return static_cast<uint32_t>(ret);
}

std::unique_ptr<ParsedCommand> ParseCommand(int argc, char** argv) {
  std::string cmd_name = argv[2];
  auto cmd = commandRegistry.find(cmd_name);
  if (cmd == commandRegistry.end()) {
    fprintf(stderr, "error: Invalid command '%s'\n", cmd_name.c_str());
    PrintUsage();
    return nullptr;
  }

  std::unique_ptr<ParsedCommand> result = std::make_unique<ParsedCommand>();
  result->name = cmd_name;
  result->handler = cmd->second.handler;

  optind = 3;
  int opt;
  while ((opt = getopt_long(argc, argv, cmd->second.short_opts, cmd->second.long_opts, nullptr)) !=
         -1) {
    switch (opt) {
      case '?':
      case 'h':
        printf("%s\n", cmd->second.description);
        printf("%s", cmd->second.helpMessage);
        return nullptr;
      case 'f':
        result->options[opt] = optarg;
        break;
      case 't':
      case 'i':
      case 's': {
        auto val = ParseStrToUint32(optarg);
        if (!val) {
          fprintf(stderr, "error: invalid argument for -%c\n", opt);
          return nullptr;
        }
        result->options[opt] = *val;
      } break;
      case 'v': {
        if (cmd->second.command_type == UfsCommand::WRITE_DESC) {
          result->options[opt] = optarg;
        }
      } break;
      default:
        break;
    }
  }

  return result;
}

int ExecuteCommand(fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>& client,
                   const ParsedCommand& cmd) {
  return cmd.handler(client, cmd.options);
}

}  // namespace

void Initialize() {
  registerCommand("read-desc",
                  {.command_type = UfsCommand::READ_DESC,
                   .description = "Retrieve the characteristics and functions of the device.\n",
                   .long_opts = kQueryRequestOptions,
                   .short_opts = kQueryRequestShortOptions,
                   .handler = HandleReadDescriptor,
                   .helpMessage = kReadDescUsageMessage});
  registerCommand("write-desc",
                  {.command_type = UfsCommand::WRITE_DESC,
                   .description = "Configure the characteristics and functions of the device.\n",
                   .long_opts = kWriteDescriptorOptions,
                   .short_opts = kWriteDescriptorShortOptions,
                   .handler = HandleWriteDescriptor,
                   .helpMessage = kWriteDescUsageMessage});
}

void PrintUsage() {
  printf(
      "Usage: ufsutil <device> <command> [options]\n"
      "   <device>       Path to the UFS device service endpoint, e.g., "
      "/svc/fuchsia.hardware.ufs.Service/<instance_id>/device\n\n");

  printf("Available commands:\n");
  for (const auto& cmd : commandRegistry) {
    printf("  %-40s  %s", cmd.first.c_str(), cmd.second.description);
  }
  printf("\nType 'ufsutil <device> <command> -h' for help on a specific command.\n");
  printf("\nSupported Specifiation: JEDEC UFS 3.1 (JESD220E)\n");
}

int RunUfsUtils(fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs> client, int argc, char** argv) {
  std::unique_ptr<ParsedCommand> cmd = ParseCommand(argc, argv);
  if (!cmd) {
    return EXIT_FAILURE;
  }

  return ExecuteCommand(client, *cmd);
}

zx::result<fidl::WireSyncClient<fuchsia_hardware_ufs::Ufs>> OpenDevice(const char* dev) {
  zx::result result = component::Connect<fuchsia_hardware_ufs::Ufs>(dev);
  if (result.is_error()) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return zx::ok(fidl::WireSyncClient(std::move(result.value())));
}

}  // namespace ufsutil
