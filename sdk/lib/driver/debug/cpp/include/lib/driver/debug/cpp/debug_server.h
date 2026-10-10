// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEBUG_CPP_DEBUG_SERVER_H_
#define LIB_DRIVER_DEBUG_CPP_DEBUG_SERVER_H_

#include <fidl/fuchsia.driver.debug/cpp/fidl.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>
#include <lib/fit/function.h>
#include <lib/fit/result.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/result.h>
#include <lib/zx/socket.h>

#include <memory>
#include <string>
#include <vector>

namespace fdf_debug {

// Execution result representing stdout, stderr, and exit code.
struct ExecutionResult {
  std::string stdout_str;
  std::string stderr_str;
  int32_t exit_code_val = 0;

  std::string& stdout_() { return stdout_str; }
  const std::string& stdout_() const { return stdout_str; }
  std::string& stderr_() { return stderr_str; }
  const std::string& stderr_() const { return stderr_str; }
  int32_t& exit_code() { return exit_code_val; }
  int32_t exit_code() const { return exit_code_val; }
};

// Synchronous handler returning stdout string or stderr error string.
// On success, exit code is 0 and returned string is written to stdout.
// On error, exit code is 1 and returned error string is written to stderr.
using StringCommandHandler =
    fit::function<fit::result<std::string, std::string>(cpp20::span<const std::string> args)>;

// Synchronous handler returning zx::result<std::string>.
// On error (status != ZX_OK), exit code is 1 and status string is written to stderr.
using StatusStringCommandHandler =
    fit::function<zx::result<std::string>(cpp20::span<const std::string> args)>;

// Synchronous handler returning a stdout string directly. Exit code is always 0.
using SimpleStringCommandHandler = fit::function<std::string(cpp20::span<const std::string> args)>;

// Full command handler returning complete ExecutionResult (stdout, stderr, exit_code).
using FullCommandHandler = fit::function<ExecutionResult(cpp20::span<const std::string> args)>;

// Asynchronous command handler invoking a completer with ExecutionResult when complete.
using AsyncCommandHandler = fit::function<void(cpp20::span<const std::string> args,
                                               fit::callback<void(ExecutionResult)> completer)>;

class DebugServer final : public fidl::Server<fuchsia_driver_debug::Debug> {
 public:
  DebugServer() = default;
  ~DebugServer() override = default;

  DebugServer(const DebugServer&) = delete;
  DebugServer& operator=(const DebugServer&) = delete;
  DebugServer(DebugServer&&) = delete;
  DebugServer& operator=(DebugServer&&) = delete;

  // Register command handlers.
  void RegisterCommand(std::string name, std::string description, StringCommandHandler handler);
  void RegisterCommand(std::string name, std::string description, std::string usage,
                       StringCommandHandler handler);

  void RegisterCommand(std::string name, std::string description,
                       StatusStringCommandHandler handler);
  void RegisterCommand(std::string name, std::string description, std::string usage,
                       StatusStringCommandHandler handler);

  void RegisterCommand(std::string name, std::string description,
                       SimpleStringCommandHandler handler);
  void RegisterCommand(std::string name, std::string description, std::string usage,
                       SimpleStringCommandHandler handler);

  void RegisterCommand(std::string name, std::string description, FullCommandHandler handler);
  void RegisterCommand(std::string name, std::string description, std::string usage,
                       FullCommandHandler handler);

  void RegisterCommand(std::string name, std::string description, AsyncCommandHandler handler);
  void RegisterCommand(std::string name, std::string description, std::string usage,
                       AsyncCommandHandler handler);

  // Serve Debug protocol on an OutgoingDirectory.
  zx::result<> Serve(async_dispatcher_t* dispatcher, component::OutgoingDirectory& outgoing);
  zx::result<> Serve(async_dispatcher_t* dispatcher, fdf::OutgoingDirectory& outgoing);
  zx::result<> Serve(async_dispatcher_t* dispatcher,
                     const std::shared_ptr<fdf::OutgoingDirectory>& outgoing);

  // Bind a server endpoint directly.
  void Bind(async_dispatcher_t* dispatcher,
            fidl::ServerEnd<fuchsia_driver_debug::Debug> server_end);
  void Bind(fidl::ServerEnd<fuchsia_driver_debug::Debug> server_end,
            async_dispatcher_t* dispatcher) {
    Bind(dispatcher, std::move(server_end));
  }

  // Create a protocol handler to be added to an outgoing directory or service.
  fidl::ProtocolHandler<fuchsia_driver_debug::Debug> CreateHandler(async_dispatcher_t* dispatcher);

  // fidl::Server<fuchsia_driver_debug::Debug> implementation
  void Execute(ExecuteRequest& request, ExecuteCompleter::Sync& completer) override;
  void ListCommands(ListCommandsCompleter::Sync& completer) override;
  void handle_unknown_method(fidl::UnknownMethodMetadata<fuchsia_driver_debug::Debug> metadata,
                             fidl::UnknownMethodCompleter::Sync& completer) override;

 private:
  struct CommandEntry {
    std::string name;
    std::string description;
    std::string usage;
    AsyncCommandHandler handler;
  };

  std::string GenerateHelp() const;
  std::string GenerateCommandHelp(const CommandEntry& cmd) const;

  std::vector<CommandEntry> commands_;
  fidl::ServerBindingGroup<fuchsia_driver_debug::Debug> bindings_;
};

}  // namespace fdf_debug

#endif  // LIB_DRIVER_DEBUG_CPP_DEBUG_SERVER_H_
