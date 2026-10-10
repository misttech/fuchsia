// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/incoming/cpp/protocol.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/debug/cpp/debug_server.h>
#include <lib/driver/outgoing/cpp/outgoing_directory.h>

#include <gtest/gtest.h>
#include <src/lib/testing/loop_fixture/real_loop_fixture.h>
#include <src/lib/testing/predicates/status.h>

namespace {

class DebugServerTest : public gtest::RealLoopFixture {
 protected:
  void SetUp() override {
    server_ = std::make_unique<fdf_debug::DebugServer>();

    // 1. Register StringCommandHandler (sync)
    server_->RegisterCommand(
        "ping", "Ping the driver.", "[-c <count>]",
        [](cpp20::span<const std::string> args) -> fit::result<std::string, std::string> {
          int count = 1;
          for (size_t i = 0; i < args.size(); ++i) {
            if (args[i] == "-c" && i + 1 < args.size()) {
              count = std::stoi(std::string(args[++i]));
            } else {
              return fit::error("invalid flag");
            }
          }
          std::string out;
          for (int i = 1; i <= count; ++i) {
            out += "ping " + std::to_string(i) + "\n";
          }
          return fit::ok(out);
        });

    // 2. Register FullCommandHandler
    server_->RegisterCommand("reset", "Reset the driver state.", "[--hard]",
                             [](cpp20::span<const std::string> args) -> fdf_debug::ExecutionResult {
                               bool hard = false;
                               for (size_t i = 0; i < args.size(); ++i) {
                                 if (args[i] == "--hard") {
                                   hard = true;
                                 } else {
                                   fdf_debug::ExecutionResult err;
                                   err.exit_code() = 2;
                                   err.stdout_() = "";
                                   err.stderr_() =
                                       "Unrecognized option: " + std::string(args[i]) + "\n";
                                   return err;
                                 }
                               }
                               fdf_debug::ExecutionResult res;
                               res.exit_code() = 0;
                               res.stdout_() = hard ? "hard reset\n" : "soft reset\n";
                               res.stderr_() = "";
                               return res;
                             });

    // 3. Register AsyncCommandHandler
    server_->RegisterCommand("async_cmd", "An asynchronous command.",
                             [](cpp20::span<const std::string> args,
                                fit::callback<void(fdf_debug::ExecutionResult)> completer) {
                               fdf_debug::ExecutionResult res;
                               res.exit_code() = 0;
                               res.stdout_() = "async success\n";
                               res.stderr_() = "";
                               completer(std::move(res));
                             });

    // 4. Register fault command that fails with handler error
    server_->RegisterCommand(
        "fault", "Induce driver fault.",
        [](cpp20::span<const std::string> args) -> fit::result<std::string, std::string> {
          return fit::error("driver internal fault\n");
        });

    // 5. Register simple string command
    server_->RegisterCommand(
        "simple", "Simple string command.",
        [](cpp20::span<const std::string> args) -> std::string { return "simple output\n"; });

    // 6. Register status string command
    server_->RegisterCommand("status_fail", "Fails with zx_status_t.",
                             [](cpp20::span<const std::string> args) -> zx::result<std::string> {
                               return zx::error(ZX_ERR_NOT_FOUND);
                             });
  }

  fidl::Client<fuchsia_driver_debug::Debug> CreateClient() {
    auto endpoints = fidl::Endpoints<fuchsia_driver_debug::Debug>::Create();
    server_->Bind(dispatcher(), std::move(endpoints.server));
    return fidl::Client<fuchsia_driver_debug::Debug>(std::move(endpoints.client), dispatcher());
  }

  std::vector<fuchsia_driver_debug::CommandInfo> ListCommands(
      fidl::Client<fuchsia_driver_debug::Debug>& client) {
    std::optional<fidl::Result<fuchsia_driver_debug::Debug::ListCommands>> response;
    client->ListCommands().ThenExactlyOnce(
        [&](fidl::Result<fuchsia_driver_debug::Debug::ListCommands>& result) {
          response = std::move(result);
        });
    RunLoopUntil([&] { return response.has_value(); });
    EXPECT_TRUE(response->is_ok());
    if (!response->is_ok()) {
      return {};
    }
    return std::move((*response)->commands());
  }

  static std::string ReadSocket(const zx::socket& socket) {
    std::string out;
    char buf[512];
    while (true) {
      size_t actual = 0;
      zx_status_t status = socket.read(0, buf, sizeof(buf), &actual);
      if (status == ZX_OK) {
        out.append(buf, actual);
      } else {
        break;
      }
    }
    return out;
  }

  fdf_debug::ExecutionResult Execute(fidl::Client<fuchsia_driver_debug::Debug>& client,
                                     std::vector<std::string> args) {
    zx::socket stdout_client, stdout_server;
    EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &stdout_client, &stdout_server), ZX_OK);
    zx::socket stderr_client, stderr_server;
    EXPECT_EQ(zx::socket::create(ZX_SOCKET_STREAM, &stderr_client, &stderr_server), ZX_OK);

    std::optional<fidl::Result<fuchsia_driver_debug::Debug::Execute>> response;
    fuchsia_driver_debug::DebugExecuteRequest request;
    request.args() = std::move(args);
    request.stdout_() = std::move(stdout_server);
    request.stderr_() = std::move(stderr_server);

    client->Execute(std::move(request))
        .ThenExactlyOnce([&](fidl::Result<fuchsia_driver_debug::Debug::Execute>& result) {
          response = std::move(result);
        });
    RunLoopUntil([&] { return response.has_value(); });
    EXPECT_TRUE(response->is_ok());
    if (!response->is_ok()) {
      return {};
    }

    fdf_debug::ExecutionResult result;
    result.exit_code() = (*response)->exit_code();
    result.stdout_() = ReadSocket(stdout_client);
    result.stderr_() = ReadSocket(stderr_client);
    return result;
  }

  std::unique_ptr<fdf_debug::DebugServer> server_;
};

TEST_F(DebugServerTest, ListCommands) {
  auto client = CreateClient();
  auto commands = ListCommands(client);
  ASSERT_EQ(commands.size(), 6u);

  EXPECT_EQ(commands[0].name().value(), "ping");
  EXPECT_EQ(commands[0].description().value(), "Ping the driver.");

  EXPECT_EQ(commands[1].name().value(), "reset");
  EXPECT_EQ(commands[1].description().value(), "Reset the driver state.");

  EXPECT_EQ(commands[2].name().value(), "async_cmd");
  EXPECT_EQ(commands[2].description().value(), "An asynchronous command.");

  EXPECT_EQ(commands[3].name().value(), "fault");
  EXPECT_EQ(commands[3].description().value(), "Induce driver fault.");

  EXPECT_EQ(commands[4].name().value(), "simple");
  EXPECT_EQ(commands[4].description().value(), "Simple string command.");

  EXPECT_EQ(commands[5].name().value(), "status_fail");
  EXPECT_EQ(commands[5].description().value(), "Fails with zx_status_t.");
}

TEST_F(DebugServerTest, ExecuteStringCommand) {
  auto client = CreateClient();

  auto result = Execute(client, {"ping", "-c", "2"});
  EXPECT_EQ(result.exit_code(), 0);
  EXPECT_EQ(result.stdout_(), "ping 1\nping 2\n");
  EXPECT_EQ(result.stderr_(), "");
}

TEST_F(DebugServerTest, ExecuteFullCommand) {
  auto client = CreateClient();

  auto result = Execute(client, {"reset", "--hard"});
  EXPECT_EQ(result.exit_code(), 0);
  EXPECT_EQ(result.stdout_(), "hard reset\n");
  EXPECT_EQ(result.stderr_(), "");

  auto soft_res = Execute(client, {"reset"});
  EXPECT_EQ(soft_res.exit_code(), 0);
  EXPECT_EQ(soft_res.stdout_(), "soft reset\n");
  EXPECT_EQ(soft_res.stderr_(), "");
}

TEST_F(DebugServerTest, ExecuteAsyncCommand) {
  auto client = CreateClient();

  auto result = Execute(client, {"async_cmd"});
  EXPECT_EQ(result.exit_code(), 0);
  EXPECT_EQ(result.stdout_(), "async success\n");
  EXPECT_EQ(result.stderr_(), "");
}

TEST_F(DebugServerTest, ExecuteSimpleStringCommand) {
  auto client = CreateClient();

  auto result = Execute(client, {"simple"});
  EXPECT_EQ(result.exit_code(), 0);
  EXPECT_EQ(result.stdout_(), "simple output\n");
  EXPECT_EQ(result.stderr_(), "");
}

TEST_F(DebugServerTest, TopLevelHelp) {
  auto client = CreateClient();

  auto res1 = Execute(client, {"--help"});
  EXPECT_EQ(res1.exit_code(), 0);
  EXPECT_NE(res1.stdout_().find("Usage: debug <command> [<args>]"), std::string::npos);
  EXPECT_NE(res1.stdout_().find("ping"), std::string::npos);
  EXPECT_NE(res1.stdout_().find("reset"), std::string::npos);
  EXPECT_EQ(res1.stderr_(), "");

  auto res2 = Execute(client, {});
  EXPECT_EQ(res2.exit_code(), 0);
  EXPECT_NE(res2.stdout_().find("Usage: debug <command> [<args>]"), std::string::npos);

  auto res3 = Execute(client, {"-h"});
  EXPECT_EQ(res3.exit_code(), 0);
  EXPECT_NE(res3.stdout_().find("Usage: debug <command> [<args>]"), std::string::npos);
}

TEST_F(DebugServerTest, CommandHelp) {
  auto client = CreateClient();

  auto res1 = Execute(client, {"ping", "--help"});
  EXPECT_EQ(res1.exit_code(), 0);
  EXPECT_NE(res1.stdout_().find("Usage: debug ping [-c <count>]"), std::string::npos);
  EXPECT_NE(res1.stdout_().find("Ping the driver."), std::string::npos);
  EXPECT_EQ(res1.stderr_(), "");

  auto res2 = Execute(client, {"reset", "-h"});
  EXPECT_EQ(res2.exit_code(), 0);
  EXPECT_NE(res2.stdout_().find("Usage: debug reset [--hard]"), std::string::npos);
  EXPECT_NE(res2.stdout_().find("Reset the driver state."), std::string::npos);
  EXPECT_EQ(res2.stderr_(), "");
}

TEST_F(DebugServerTest, UnknownCommandError) {
  auto client = CreateClient();

  auto result = Execute(client, {"unknown_cmd"});
  EXPECT_EQ(result.exit_code(), 2);
  EXPECT_EQ(result.stdout_(), "");
  EXPECT_NE(result.stderr_().find("Unrecognized command: unknown_cmd"), std::string::npos);
}

TEST_F(DebugServerTest, HandlerError) {
  auto client = CreateClient();

  auto result = Execute(client, {"fault"});
  EXPECT_EQ(result.exit_code(), 1);
  EXPECT_EQ(result.stdout_(), "");
  EXPECT_NE(result.stderr_().find("driver internal fault"), std::string::npos);

  auto status_result = Execute(client, {"status_fail"});
  EXPECT_EQ(status_result.exit_code(), 1);
  EXPECT_EQ(status_result.stdout_(), "");
  EXPECT_NE(status_result.stderr_().find("ZX_ERR_NOT_FOUND"), std::string::npos);
}

TEST_F(DebugServerTest, MultipleClients) {
  auto client1 = CreateClient();
  auto client2 = CreateClient();

  auto res1 = Execute(client1, {"ping"});
  EXPECT_EQ(res1.exit_code(), 0);
  EXPECT_EQ(res1.stdout_(), "ping 1\n");

  auto res2 = Execute(client2, {"reset"});
  EXPECT_EQ(res2.exit_code(), 0);
  EXPECT_EQ(res2.stdout_(), "soft reset\n");
}

TEST_F(DebugServerTest, ServeComponentOutgoingDirectory) {
  component::OutgoingDirectory outgoing(dispatcher());
  ASSERT_OK(server_->Serve(dispatcher(), outgoing));

  auto endpoints = fidl::Endpoints<fuchsia_io::Directory>::Create();
  ASSERT_OK(outgoing.Serve(std::move(endpoints.server)));

  auto client_end = component::ConnectAt<fuchsia_driver_debug::Debug>(
      endpoints.client, "svc/fuchsia.driver.debug.Debug");
  ASSERT_OK(client_end);

  fidl::Client client(std::move(*client_end), dispatcher());
  auto commands = ListCommands(client);
  EXPECT_EQ(commands.size(), 6u);
}

}  // namespace
