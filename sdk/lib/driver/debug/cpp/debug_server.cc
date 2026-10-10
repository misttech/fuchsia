// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/driver/debug/cpp/debug_server.h>
#include <zircon/status.h>

#include <algorithm>
#include <format>
#include <iterator>
#include <string_view>

namespace fdf_debug {

namespace {

void WriteToSocket(const zx::socket& socket, std::string_view data) {
  if (data.empty() || !socket.is_valid()) {
    return;
  }
  const char* ptr = data.data();
  size_t remaining = data.size();
  while (remaining > 0) {
    size_t actual = 0;
    zx_status_t status = socket.write(0, ptr, remaining, &actual);
    if (status != ZX_OK) {
      break;
    }
    ptr += actual;
    remaining -= actual;
  }
}

void ReplyWithResult(zx::socket stdout_socket, zx::socket stderr_socket,
                     const ExecutionResult& result,
                     DebugServer::ExecuteCompleter::Async completer) {
  WriteToSocket(stdout_socket, result.stdout_());
  stdout_socket.reset();
  WriteToSocket(stderr_socket, result.stderr_());
  stderr_socket.reset();

  fuchsia_driver_debug::DebugExecuteResponse response;
  response.exit_code() = result.exit_code();
  completer.Reply(fit::ok(std::move(response)));
}

}  // namespace

void DebugServer::RegisterCommand(std::string name, std::string description,
                                  StringCommandHandler handler) {
  RegisterCommand(std::move(name), std::move(description), "", std::move(handler));
}

void DebugServer::RegisterCommand(std::string name, std::string description, std::string usage,
                                  StringCommandHandler handler) {
  RegisterCommand(
      std::move(name), std::move(description), std::move(usage),
      AsyncCommandHandler([h = std::move(handler)](cpp20::span<const std::string> args,
                                                   fit::callback<void(ExecutionResult)> completer) {
        auto res = h(args);
        ExecutionResult result;
        if (res.is_ok()) {
          result.stdout_() = std::move(res.value());
          result.stderr_() = "";
          result.exit_code() = 0;
        } else {
          result.stdout_() = "";
          result.stderr_() = std::move(res.error_value());
          result.exit_code() = 1;
        }
        completer(std::move(result));
      }));
}

void DebugServer::RegisterCommand(std::string name, std::string description,
                                  StatusStringCommandHandler handler) {
  RegisterCommand(std::move(name), std::move(description), "", std::move(handler));
}

void DebugServer::RegisterCommand(std::string name, std::string description, std::string usage,
                                  StatusStringCommandHandler handler) {
  RegisterCommand(
      std::move(name), std::move(description), std::move(usage),
      AsyncCommandHandler([h = std::move(handler)](cpp20::span<const std::string> args,
                                                   fit::callback<void(ExecutionResult)> completer) {
        auto res = h(args);
        ExecutionResult result;
        if (res.is_ok()) {
          result.stdout_() = std::move(res.value());
          result.stderr_() = "";
          result.exit_code() = 0;
        } else {
          result.stdout_() = "";
          result.stderr_() = std::string(zx_status_get_string(res.error_value())) + "\n";
          result.exit_code() = 1;
        }
        completer(std::move(result));
      }));
}

void DebugServer::RegisterCommand(std::string name, std::string description,
                                  SimpleStringCommandHandler handler) {
  RegisterCommand(std::move(name), std::move(description), "", std::move(handler));
}

void DebugServer::RegisterCommand(std::string name, std::string description, std::string usage,
                                  SimpleStringCommandHandler handler) {
  RegisterCommand(
      std::move(name), std::move(description), std::move(usage),
      AsyncCommandHandler([h = std::move(handler)](cpp20::span<const std::string> args,
                                                   fit::callback<void(ExecutionResult)> completer) {
        ExecutionResult result;
        result.stdout_() = h(args);
        result.stderr_() = "";
        result.exit_code() = 0;
        completer(std::move(result));
      }));
}

void DebugServer::RegisterCommand(std::string name, std::string description,
                                  FullCommandHandler handler) {
  RegisterCommand(std::move(name), std::move(description), "", std::move(handler));
}

void DebugServer::RegisterCommand(std::string name, std::string description, std::string usage,
                                  FullCommandHandler handler) {
  RegisterCommand(
      std::move(name), std::move(description), std::move(usage),
      AsyncCommandHandler([h = std::move(handler)](cpp20::span<const std::string> args,
                                                   fit::callback<void(ExecutionResult)> completer) {
        completer(h(args));
      }));
}

void DebugServer::RegisterCommand(std::string name, std::string description,
                                  AsyncCommandHandler handler) {
  RegisterCommand(std::move(name), std::move(description), "", std::move(handler));
}

void DebugServer::RegisterCommand(std::string name, std::string description, std::string usage,
                                  AsyncCommandHandler handler) {
  commands_.push_back(CommandEntry{
      .name = std::move(name),
      .description = std::move(description),
      .usage = std::move(usage),
      .handler = std::move(handler),
  });
}

zx::result<> DebugServer::Serve(async_dispatcher_t* dispatcher,
                                component::OutgoingDirectory& outgoing) {
  return outgoing.AddUnmanagedProtocol<fuchsia_driver_debug::Debug>(CreateHandler(dispatcher));
}

zx::result<> DebugServer::Serve(async_dispatcher_t* dispatcher, fdf::OutgoingDirectory& outgoing) {
  return outgoing.component().AddUnmanagedProtocol<fuchsia_driver_debug::Debug>(
      CreateHandler(dispatcher));
}

zx::result<> DebugServer::Serve(async_dispatcher_t* dispatcher,
                                const std::shared_ptr<fdf::OutgoingDirectory>& outgoing) {
  if (!outgoing) {
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  return Serve(dispatcher, *outgoing);
}

void DebugServer::Bind(async_dispatcher_t* dispatcher,
                       fidl::ServerEnd<fuchsia_driver_debug::Debug> server_end) {
  bindings_.AddBinding(dispatcher, std::move(server_end), this, fidl::kIgnoreBindingClosure);
}

fidl::ProtocolHandler<fuchsia_driver_debug::Debug> DebugServer::CreateHandler(
    async_dispatcher_t* dispatcher) {
  return bindings_.CreateHandler(this, dispatcher, fidl::kIgnoreBindingClosure);
}

void DebugServer::Execute(ExecuteRequest& request, ExecuteCompleter::Sync& completer) {
  const std::vector<std::string>& args = request.args();
  zx::socket stdout_socket = std::move(request.stdout_());
  zx::socket stderr_socket = std::move(request.stderr_());

  if (args.empty() || args[0] == "--help" || args[0] == "-h" || args[0] == "help") {
    ExecutionResult result;
    result.stdout_() = GenerateHelp();
    result.stderr_() = "";
    result.exit_code() = 0;
    ReplyWithResult(std::move(stdout_socket), std::move(stderr_socket), result,
                    completer.ToAsync());
    return;
  }

  const std::string& cmd_name = args[0];
  auto it = std::find_if(commands_.begin(), commands_.end(),
                         [&cmd_name](const CommandEntry& entry) { return entry.name == cmd_name; });

  if (it == commands_.end()) {
    ExecutionResult result;
    result.stdout_() = "";
    result.stderr_() = "Unrecognized command: " + cmd_name + "\n";
    result.exit_code() = 2;
    ReplyWithResult(std::move(stdout_socket), std::move(stderr_socket), result,
                    completer.ToAsync());
    return;
  }

  if (args.size() > 1 && (args[1] == "--help" || args[1] == "-h")) {
    ExecutionResult result;
    result.stdout_() = GenerateCommandHelp(*it);
    result.stderr_() = "";
    result.exit_code() = 0;
    ReplyWithResult(std::move(stdout_socket), std::move(stderr_socket), result,
                    completer.ToAsync());
    return;
  }

  auto async_completer = completer.ToAsync();
  cpp20::span<const std::string> sub_args = cpp20::span(args).subspan(1);
  it->handler(sub_args, [async_completer = std::move(async_completer),
                         stdout_socket = std::move(stdout_socket),
                         stderr_socket = std::move(stderr_socket)](ExecutionResult result) mutable {
    ReplyWithResult(std::move(stdout_socket), std::move(stderr_socket), result,
                    std::move(async_completer));
  });
}

void DebugServer::ListCommands(ListCommandsCompleter::Sync& completer) {
  std::vector<fuchsia_driver_debug::CommandInfo> command_infos;
  command_infos.reserve(commands_.size());
  for (const auto& cmd : commands_) {
    fuchsia_driver_debug::CommandInfo info;
    info.name() = cmd.name;
    info.description() = cmd.description;
    command_infos.push_back(std::move(info));
  }
  fuchsia_driver_debug::DebugListCommandsResponse response;
  response.commands() = std::move(command_infos);
  completer.Reply(fit::ok(std::move(response)));
}

void DebugServer::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_debug::Debug> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  completer.Close(ZX_ERR_NOT_SUPPORTED);
}

std::string DebugServer::GenerateHelp() const {
  std::string help = "Usage: debug <command> [<args>]\n\n";
  help += "Driver debug commands.\n\n";
  help += "Options:\n";
  help += "  --help            display usage information\n\n";
  help += "Commands:\n";

  size_t max_name_len = 14;
  for (const auto& cmd : commands_) {
    max_name_len = std::max(max_name_len, cmd.name.length());
  }

  for (const auto& cmd : commands_) {
    std::format_to(std::back_inserter(help), "  {:<{}}  {}\n", cmd.name, max_name_len,
                   cmd.description);
  }

  return help;
}

std::string DebugServer::GenerateCommandHelp(const CommandEntry& cmd) const {
  std::string usage_suffix = cmd.usage.empty() ? "[<args>]" : cmd.usage;
  std::string help = "Usage: debug " + cmd.name + " " + usage_suffix + "\n\n";
  help += cmd.description + "\n";
  return help;
}

}  // namespace fdf_debug
