// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_console_command.h"

#include <gtest/gtest.h>

#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/console/console_context.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"
#include "src/developer/debug/zxdb/symbols/function.h"

namespace zxdb {

namespace {

class RequestConsoleCommandTest : public DebugAdapterContextTest {
 public:
  void SetUp() override {
    DebugAdapterContextTest::SetUp();
    InitializeDebugging();

    process_ = InjectProcessWithModule(kProcessKoid, 0x1000);
    RunClient();
    thread_ = InjectThread(kProcessKoid, kThreadKoid);
    RunClient();
  }

  dap::ResponseOrError<dap::StackTraceResponse> GetStackTrace(
      std::vector<std::unique_ptr<Frame>> frames) {
    InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                             std::move(frames), true);
    RunClient();

    dap::StackTraceRequest stack_request = {};
    stack_request.threadId = kThreadKoid;
    auto stack_response_fut = client().send(stack_request);

    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunClient();
    return stack_response_fut.get();
  }

  Thread* thread() const { return thread_; }
  Process* process() const { return process_; }

 private:
  Thread* thread_ = nullptr;
  Process* process_ = nullptr;
};

}  // namespace

TEST_F(RequestConsoleCommandTest, EmptyCommand) {
  dap::ZxdbConsoleCommandRequest req;
  req.command = "";
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunClient();

  auto got = response.get();
  ASSERT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "Command cannot be empty");
}

TEST_F(RequestConsoleCommandTest, GlobalCommandWithoutFrameId) {
  dap::ZxdbConsoleCommandRequest req;
  req.command = "help";
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunClient();

  auto got = response.get();
  ASSERT_FALSE(got.error);
  EXPECT_FALSE(got.response.output.empty());
  EXPECT_NE(got.response.output.find("Help!"), std::string::npos);
}

TEST_F(RequestConsoleCommandTest, CommandWithFrameId) {
  fxl::RefPtr<Function> function1(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
  function1->set_assigned_name("test_func1");
  auto location1 = Location(0x10010, FileLine("test_file.cc", 23), 10,
                            SymbolContext::ForRelativeAddresses(), function1);

  fxl::RefPtr<Function> function2(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
  function2->set_assigned_name("test_func2");
  auto location2 = Location(0x10040, FileLine("test_file.cc", 50), 10,
                            SymbolContext::ForRelativeAddresses(), function2);

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread(), location1, 0x2000));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread(), location2, 0x2020));

  auto stack_response = GetStackTrace(std::move(frames));
  ASSERT_FALSE(stack_response.error);
  ASSERT_EQ(stack_response.response.stackFrames.size(), 2u);

  dap::integer frame_id_1 = stack_response.response.stackFrames[1].id;
  ASSERT_NE(frame_id_1, 0);

  EXPECT_EQ(context().console()->context().GetActiveFrameIdForThread(thread()), 0);

  dap::ZxdbConsoleCommandRequest req;
  req.frameId = frame_id_1;
  req.command = "frame";
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunClient();

  auto got = response.get();
  ASSERT_FALSE(got.error);
  EXPECT_NE(got.response.output.find("test_func2"), std::string::npos);
  EXPECT_EQ(context().console()->context().GetActiveFrameIdForThread(thread()), 1);
  EXPECT_EQ(context().console()->context().GetActiveTarget(), process()->GetTarget());
}

TEST_F(RequestConsoleCommandTest, InvalidFrameId) {
  dap::ZxdbConsoleCommandRequest req;
  req.frameId = 9999;
  req.command = "frame";
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunClient();

  auto got = response.get();
  EXPECT_TRUE(got.error);
  EXPECT_EQ(got.error.message, "Invalid frame ID");
}

TEST_F(RequestConsoleCommandTest, CommandErrorReturnsDapError) {
  dap::ZxdbConsoleCommandRequest req;
  req.command = "nonexistent_command_xyz";
  auto response = client().send(req);

  context().OnStreamReadable();
  loop().RunUntilNoTasks();
  RunClient();

  auto got = response.get();
  EXPECT_TRUE(got.error);
  EXPECT_NE(got.error.message.find("nonexistent_command_xyz"), std::string::npos);
}

}  // namespace zxdb
