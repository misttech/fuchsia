// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/developer/debug/ipc/protocol.h"
#include "src/developer/debug/zxdb/client/breakpoint.h"
#include "src/developer/debug/zxdb/client/mock_remote_api.h"
#include "src/developer/debug/zxdb/client/target_impl.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"

namespace zxdb {

namespace {

// Create our own fake RemoteAPI to get the id of created breakpoints, which is needed for
// `ContextTest.StoppedEventProcessLevelBreakpoint`.
class FakeRemoteAPI : public RemoteAPI {
 public:
  std::vector<uint32_t>& added_breakpoint_ids() { return added_breakpoint_ids_; }

  void AddOrChangeBreakpoint(
      const debug_ipc::AddOrChangeBreakpointRequest& request,
      fit::callback<void(const Err&, debug_ipc::AddOrChangeBreakpointReply)> cb) override {
    added_breakpoint_ids_.push_back(request.breakpoint.id);
    RemoteAPI::AddOrChangeBreakpoint(request, std::move(cb));
  }

 private:
  std::vector<uint32_t> added_breakpoint_ids_;
};

class ContextTest : public DebugAdapterContextTest {
 public:
  FakeRemoteAPI* remote_api() { return remote_api_; }

 protected:
  std::unique_ptr<RemoteAPI> GetRemoteAPIImpl() override {
    auto remote_api = std::make_unique<FakeRemoteAPI>();
    remote_api_ = remote_api.get();
    return remote_api;
  }

 private:
  FakeRemoteAPI* remote_api_;
};

}  // namespace

TEST_F(ContextTest, InitializeRequest) {
  SetUpConnectedContext();

  // Send initialize request from the client.
  auto response = client().send(dap::InitializeRequest{});

  // Read request and process it.
  context().OnStreamReadable();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_EQ(got.error, false);
  EXPECT_EQ(bool(got.response.supportsFunctionBreakpoints), true);
  EXPECT_EQ(bool(got.response.supportsConfigurationDoneRequest), true);
}

TEST_F(ContextTest, InitializeRequestNotConnected) {
  // Send initialize request from the client.
  auto response = client().send(dap::InitializeRequest{});

  // Read request and process it.
  context().OnStreamReadable();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_EQ(got.error, true);
}

TEST_F(ContextTest, InitializedEvent) {
  bool event_received = false;
  client().registerHandler([&](const dap::InitializedEvent& arg) { event_received = true; });

  // Send initialize request from the client.
  auto response = client().send(dap::InitializeRequest{});
  context().OnStreamReadable();
  // Run client twice to receive response and event.
  RunClient();
  RunClient();
  EXPECT_TRUE(event_received);
}

TEST_F(ContextTest, ProcessStartEvent) {
  bool start_received = false;

  client().registerHandler([&](const dap::ProcessEvent& arg) { start_received = true; });

  InitializeDebugging();
  InjectProcess(kProcessKoid);

  // Receive Process started event in client.
  RunClient();
  EXPECT_TRUE(start_received);
}

TEST_F(ContextTest, ThreadStartExitEvent) {
  bool start_received = false;
  bool exit_received = false;

  client().registerHandler([&](const dap::ThreadEvent& arg) {
    EXPECT_EQ(arg.threadId, static_cast<dap::integer>(kThreadKoid));
    if (arg.reason == "started") {
      start_received = true;
    }
    if (arg.reason == "exited") {
      exit_received = true;
    }
  });

  InitializeDebugging();

  InjectProcess(kProcessKoid);
  // Receive process started event in client.
  RunClient();

  InjectThread(kProcessKoid, kThreadKoid);
  // Receive thread started event in client.
  RunClient();
  EXPECT_TRUE(start_received);

  debug_ipc::NotifyThreadExiting notify;
  notify.record.id = {.process = kProcessKoid, .thread = kThreadKoid};
  notify.record.state = debug_ipc::ThreadRecord::State::kDying;
  session().DispatchNotifyThreadExiting(notify);

  // Receive thread exited event in client.
  RunClient();
  EXPECT_TRUE(exit_received);
}

TEST_F(ContextTest, StoppedEventException) {
  bool event_received = false;

  client().registerHandler([&](const dap::StoppedEvent& arg) {
    EXPECT_EQ(arg.reason, "unknown");
    EXPECT_TRUE(arg.threadId.has_value());
    EXPECT_FALSE(arg.allThreadsStopped.value(false));
    EXPECT_EQ(arg.threadId.value(), static_cast<dap::integer>(kThreadKoid));
    event_received = true;
  });

  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid);
  // Receive process started event in client.
  RunClient();

  InjectThread(kProcessKoid, kThreadKoid);
  // Receive thread started event in client.
  RunClient();

  constexpr uint64_t kAddress = 0x12345678;
  constexpr uint64_t kStack = 0x7890;

  // Notify of thread stop due to a general exception (e.g., crash).
  debug_ipc::NotifyException exception_notification;
  exception_notification.type = debug_ipc::ExceptionType::kGeneral;
  exception_notification.thread.id = {.process = kProcessKoid, .thread = kThreadKoid};
  exception_notification.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  exception_notification.thread.frames.emplace_back(kAddress, kStack, kStack);
  InjectException(exception_notification);

  // Receive thread stopped event in client.
  RunClient();
  EXPECT_TRUE(event_received);
}

TEST_F(ContextTest, StoppedEventThreadLevelBreakpoint) {
  bool event_received = false;

  client().registerHandler([&](const dap::StoppedEvent& arg) {
    EXPECT_EQ(arg.reason, "breakpoint");
    EXPECT_TRUE(arg.threadId.has_value());
    EXPECT_FALSE(arg.allThreadsStopped.value(false));
    EXPECT_EQ(arg.threadId.value(), static_cast<dap::integer>(kThreadKoid));
    event_received = true;
  });

  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid);
  // Receive process started event in client.
  RunClient();

  InjectThread(kProcessKoid, kThreadKoid);
  // Receive thread started event in client.
  RunClient();

  constexpr uint64_t kAddress = 0x12345678;
  constexpr uint64_t kStack = 0x7890;

  // Create a thread-level breakpoint.
  Breakpoint* bp = session().system().CreateNewBreakpoint();
  BreakpointSettings settings;
  settings.enabled = true;
  settings.stop_mode = BreakpointSettings::StopMode::kThread;
  settings.locations.emplace_back(kAddress);
  bp->SetSettings(settings);
  // Receive breakpoint changed event in client.
  RunClient();

  // Get the breakpoint id to be added to the `break_notification` exception.
  ASSERT_EQ(1u, remote_api()->added_breakpoint_ids().size());
  auto bp_id = remote_api()->added_breakpoint_ids().back();

  // Notify of thread stop.
  debug_ipc::NotifyException break_notification;
  break_notification.type = debug_ipc::ExceptionType::kSoftwareBreakpoint;
  break_notification.thread.id = {.process = kProcessKoid, .thread = kThreadKoid};
  break_notification.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  break_notification.thread.frames.emplace_back(kAddress, kStack, kStack);
  // Add the breakpoint id associated with the thread-level breakpoint that was created above.
  // This allows `DebugAdapterContext::OnThreadStopped()` to examine `StopInfo::hit_breakpoints` and
  // set dap::StoppedEvent::allThreadsStopped accordingly.
  break_notification.hit_breakpoints.push_back({.id = bp_id});
  InjectException(break_notification);

  // Receive thread stopped event in client.
  RunClient();
  EXPECT_TRUE(event_received);
}

TEST_F(ContextTest, StoppedEventProcessLevelBreakpoint) {
  bool event_received = false;

  client().registerHandler([&](const dap::StoppedEvent& arg) {
    EXPECT_EQ(arg.reason, "breakpoint");
    EXPECT_TRUE(arg.threadId.has_value());
    EXPECT_TRUE(arg.allThreadsStopped.value(false));
    EXPECT_EQ(arg.threadId.value(), static_cast<dap::integer>(kThreadKoid));
    event_received = true;
  });

  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid);
  // Receive process started event in client.
  RunClient();

  InjectThread(kProcessKoid, kThreadKoid);
  // Receive thread started event in client.
  RunClient();

  constexpr uint64_t kAddress = 0x12345678;
  constexpr uint64_t kStack = 0x7890;

  // Create a process-level breakpoint.
  Breakpoint* bp = session().system().CreateNewBreakpoint();
  BreakpointSettings settings;
  settings.enabled = true;
  settings.stop_mode = BreakpointSettings::StopMode::kProcess;
  settings.locations.emplace_back(kAddress);
  bp->SetSettings(settings);
  // Receive breakpoint changed event in client.
  RunClient();

  // Get the breakpoint id to be added to the `break_notification` exception.
  ASSERT_EQ(1u, remote_api()->added_breakpoint_ids().size());
  auto bp_id = remote_api()->added_breakpoint_ids().back();

  // Notify of thread stop.
  debug_ipc::NotifyException break_notification;
  break_notification.type = debug_ipc::ExceptionType::kSoftwareBreakpoint;
  break_notification.thread.id = {.process = kProcessKoid, .thread = kThreadKoid};
  break_notification.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  break_notification.thread.frames.emplace_back(kAddress, kStack, kStack);
  // Add the breakpoint id associated with the process-level breakpoint that was created above.
  // This allows `DebugAdapterContext::OnThreadStopped()` to examine `StopInfo::hit_breakpoints` and
  // set dap::StoppedEvent::allThreadsStopped accordingly.
  break_notification.hit_breakpoints.push_back({.id = bp_id});
  InjectException(break_notification);

  // Receive thread stopped event in client.
  RunClient();
  EXPECT_TRUE(event_received);
}

TEST_F(ContextTest, StoppedEventUnspecifiedAllLevelBreakpoint) {
  bool event_received = false;

  client().registerHandler([&](const dap::StoppedEvent& arg) {
    EXPECT_EQ(arg.reason, "breakpoint");
    EXPECT_TRUE(arg.threadId.has_value());
    EXPECT_TRUE(arg.allThreadsStopped.value(false));
    EXPECT_EQ(arg.threadId.value(), static_cast<dap::integer>(kThreadKoid));
    event_received = true;
  });

  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid);
  // Receive process started event in client.
  RunClient();

  InjectThread(kProcessKoid, kThreadKoid);
  // Receive thread started event in client.
  RunClient();

  constexpr uint64_t kAddress = 0x12345678;
  constexpr uint64_t kStack = 0x7890;

  // Create a breakpoint with an unspecified `stop_mode`, which should default to StopMode::kAll`.
  Breakpoint* bp = session().system().CreateNewBreakpoint();
  BreakpointSettings settings;
  settings.enabled = true;
  settings.locations.emplace_back(kAddress);
  bp->SetSettings(settings);
  // Receive breakpoint changed event in client.
  RunClient();

  // Get the breakpoint id to be added to the `break_notification` exception.
  ASSERT_EQ(1u, remote_api()->added_breakpoint_ids().size());
  auto bp_id = remote_api()->added_breakpoint_ids().back();

  // Notify of thread stop.
  debug_ipc::NotifyException break_notification;
  break_notification.type = debug_ipc::ExceptionType::kSoftwareBreakpoint;
  break_notification.thread.id = {.process = kProcessKoid, .thread = kThreadKoid};
  break_notification.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  break_notification.thread.frames.emplace_back(kAddress, kStack, kStack);
  // Add the breakpoint id associated with the process-level breakpoint that was created above.
  // This allows `DebugAdapterContext::OnThreadStopped()` to examine `StopInfo::hit_breakpoints` and
  // set dap::StoppedEvent::allThreadsStopped accordingly.
  break_notification.hit_breakpoints.push_back({.id = bp_id});
  InjectException(break_notification);

  // Receive thread stopped event in client.
  RunClient();
  EXPECT_TRUE(event_received);
}

TEST_F(ContextTest, DisconnectRequest) {
  bool request_received = false;

  context().set_destroy_connection_callback([&request_received]() { request_received = true; });

  InitializeDebugging();

  // Send disconnect request from client
  auto response = client().send(dap::DisconnectRequest());

  // Receive and process request in the server.
  context().OnStreamReadable();
  loop().RunUntilNoTasks();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_TRUE(request_received);
}

TEST_F(ContextTest, ExitedEvent) {
  bool event_received = false;

  client().registerHandler([&event_received](const dap::ExitedEvent& arg) {
    EXPECT_EQ(arg.exitCode, 20);
    event_received = true;
  });

  InitializeDebugging();

  InjectProcess(kProcessKoid);
  // Receive process started event in client.
  RunClient();

  InjectThread(kProcessKoid, kThreadKoid);
  // Receive thread started event in client.
  RunClient();

  // Detach from target
  auto targets = session().system().GetTargetImpls();
  ASSERT_EQ(targets.size(), 1u);
  targets[0]->OnProcessExiting(20, 0);

  // Run client to receive event.
  RunClient();
  EXPECT_TRUE(event_received);
}

namespace {

class ProcessDetachRemoteAPI : public MockRemoteAPI {
 public:
  void Detach(const debug_ipc::DetachRequest& request,
              fit::callback<void(const Err&, debug_ipc::DetachReply)> cb) override {
    debug_ipc::DetachReply reply;
    cb(Err(), reply);
  }
};

class ProcessDetachTest : public DebugAdapterContextTest {
 public:
  ProcessDetachRemoteAPI* remote_api() const { return remote_api_; }

 protected:
  std::unique_ptr<RemoteAPI> GetRemoteAPIImpl() override {
    auto remote_api = std::make_unique<ProcessDetachRemoteAPI>();
    remote_api_ = remote_api.get();
    return remote_api;
  }

 private:
  ProcessDetachRemoteAPI* remote_api_;
};

}  // namespace

TEST_F(ProcessDetachTest, TerminatedEvent) {
  bool event_received = false;

  client().registerHandler(
      [&event_received](const dap::TerminatedEvent& arg) { event_received = true; });

  InitializeDebugging();

  InjectProcess(kProcessKoid);
  // Receive process started event in client.
  RunClient();

  InjectThread(kProcessKoid, kThreadKoid);
  // Receive thread started event in client.
  RunClient();

  // Detach from target
  auto targets = session().system().GetTargetImpls();
  ASSERT_EQ(targets.size(), 1u);
  targets[0]->Detach([](const fxl::WeakPtr<Target>& target, const Err& err) {});

  // Run client to receive event.
  RunClient();
  EXPECT_TRUE(event_received);
}

}  // namespace zxdb
