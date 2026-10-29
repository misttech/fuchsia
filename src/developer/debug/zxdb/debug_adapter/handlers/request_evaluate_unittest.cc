// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_evaluate.h"

#include <gtest/gtest.h>
#include <llvm/BinaryFormat/Dwarf.h>

#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/console/console_context.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/mock_symbol_data_provider.h"
#include "src/developer/debug/zxdb/symbols/type_test_support.h"
#include "src/developer/debug/zxdb/symbols/variable_test_support.h"

namespace zxdb {

namespace {

class RequestEvaluateTest : public DebugAdapterContextTest {
 public:
  void SetUp() override {
    DebugAdapterContextTest::SetUp();
    InitializeDebugging();

    InjectProcessWithModule(kProcessKoid, 0x1000);
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

  dap::ResponseOrError<dap::EvaluateResponse> SendEvaluate(const dap::EvaluateRequest& req) {
    auto response = client().send(req);
    context().OnStreamReadable();
    loop().RunUntilNoTasks();
    RunClient();
    return response.get();
  }

  Thread* thread() const { return thread_; }

 private:
  Thread* thread_ = nullptr;
};

}  // namespace

TEST_F(RequestEvaluateTest, EvaluateVariableAndArithmetic) {
  fxl::RefPtr<Function> function1(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
  function1->set_assigned_name("test_func1");
  auto location1 = Location(0x10010, FileLine("test_file.cc", 23), 10,
                            SymbolContext::ForRelativeAddresses(), function1);

  auto test_var = MakeUint64VariableForTest(
      "test_var", 0x10030, 0x10050,
      DwarfExpr({llvm::dwarf::DW_OP_lit12, llvm::dwarf::DW_OP_stack_value}));

  fxl::RefPtr<Function> function2(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
  function2->set_assigned_name("test_func2");
  function2->set_code_ranges(AddressRanges(AddressRange(0x10030, 0x10050)));
  function2->set_variables({LazySymbol(std::move(test_var))});
  auto location2 = Location(0x10040, FileLine("test_file.cc", 50), 10,
                            SymbolContext::ForRelativeAddresses(), function2);

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread(), location1, 0x2000));
  auto frame2 = std::make_unique<MockFrame>(&session(), thread(), location2, 0x2020);
  frame2->GetMockSymbolDataProvider()->set_ip(0x10040);
  frames.push_back(std::move(frame2));

  auto stack_response = GetStackTrace(std::move(frames));
  ASSERT_FALSE(stack_response.error);
  ASSERT_EQ(stack_response.response.stackFrames.size(), 2u);

  dap::integer frame_id_1 = stack_response.response.stackFrames[1].id;
  ASSERT_NE(frame_id_1, 0);

  EXPECT_EQ(context().console()->context().GetActiveFrameIdForThread(thread()), 0);

  dap::EvaluateRequest req;
  req.context = "repl";
  req.frameId = frame_id_1;
  req.expression = "test_var + 30";
  auto got = SendEvaluate(req);

  ASSERT_FALSE(got.error);
  EXPECT_EQ(got.response.result, "42");
  ASSERT_TRUE(got.response.type.has_value());
  EXPECT_EQ(got.response.type.value(), "uint64_t");

  // Evaluating an expression via DAP must not mutate the console's active frame selection.
  EXPECT_EQ(context().console()->context().GetActiveFrameIdForThread(thread()), 0);
}

TEST_F(RequestEvaluateTest, EvaluateStructFormatting) {
  auto struct_type = MakeCollectionType(DwarfTag::kStructureType, "MyStruct",
                                        {{"a", MakeInt32Type()}, {"b", MakeInt32Type()}});
  auto struct_var = MakeVariableForTest(
      "my_struct", struct_type, 0x10000, 0x10020,
      DwarfExpr({llvm::dwarf::DW_OP_addr, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00}));

  fxl::RefPtr<Function> function(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
  function->set_assigned_name("test_func");
  function->set_code_ranges(AddressRanges(AddressRange(0x10000, 0x10020)));
  function->set_variables({LazySymbol(std::move(struct_var))});
  auto location = Location(0x10010, FileLine("test_file.cc", 23), 10,
                           SymbolContext::ForRelativeAddresses(), function);

  auto frame = std::make_unique<MockFrame>(&session(), thread(), location, 0x2000);
  frame->GetMockSymbolDataProvider()->set_ip(0x10010);
  frame->GetMockSymbolDataProvider()->AddMemory(0x3000, {10, 0, 0, 0, 20, 0, 0, 0});

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::move(frame));

  auto stack_response = GetStackTrace(std::move(frames));
  ASSERT_FALSE(stack_response.error);
  ASSERT_EQ(stack_response.response.stackFrames.size(), 1u);

  dap::EvaluateRequest req;
  req.context = "repl";
  req.frameId = stack_response.response.stackFrames[0].id;
  req.expression = "my_struct";
  auto got = SendEvaluate(req);

  ASSERT_FALSE(got.error);
  EXPECT_EQ(got.response.result, "{a = 10, b = 20}");
  ASSERT_TRUE(got.response.type.has_value());
  EXPECT_EQ(got.response.type.value(), "MyStruct");

  // Evaluating "&my_struct" expands the pointer asynchronously during FormatValue
  // (options.pointer_expand_depth = 1), exercising AsyncOutputBuffer::SetCompletionCallback.
  dap::EvaluateRequest ptr_req;
  ptr_req.context = "repl";
  ptr_req.frameId = stack_response.response.stackFrames[0].id;
  ptr_req.expression = "&my_struct";
  auto ptr_got = SendEvaluate(ptr_req);

  ASSERT_FALSE(ptr_got.error);
  EXPECT_NE(ptr_got.response.result.find("{a = 10, b = 20}"), std::string::npos);
  ASSERT_TRUE(ptr_got.response.type.has_value());
  EXPECT_EQ(ptr_got.response.type.value(), "MyStruct*");
}

TEST_F(RequestEvaluateTest, EvaluateContextsAndRejectConsoleCommand) {
  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread(), Location(), 0x2000));

  auto stack_response = GetStackTrace(std::move(frames));
  ASSERT_FALSE(stack_response.error);
  ASSERT_EQ(stack_response.response.stackFrames.size(), 1u);
  dap::integer frame_id = stack_response.response.stackFrames[0].id;

  // Omitted context, "watch", and "variables" should all succeed.
  for (const auto& ctx :
       std::vector<std::optional<std::string>>{std::nullopt, "watch", "variables", "repl"}) {
    dap::EvaluateRequest req;
    if (ctx.has_value()) {
      req.context = *ctx;
    }
    req.frameId = frame_id;
    req.expression = "1 + 2";
    auto got = SendEvaluate(req);
    ASSERT_FALSE(got.error);
    EXPECT_EQ(got.response.result, "3");
  }

  // Hover evaluation is disabled in capabilities and should return an error.
  {
    dap::EvaluateRequest req;
    req.context = "hover";
    req.frameId = frame_id;
    req.expression = "1 + 2";
    auto got = SendEvaluate(req);
    ASSERT_TRUE(got.error);
    EXPECT_EQ(got.error.message, "Hover evaluation is not supported.");
  }

  // Raw console commands must not be executed by evaluate.
  {
    dap::EvaluateRequest req;
    req.context = "repl";
    req.frameId = frame_id;
    req.expression = "print 1";
    auto got = SendEvaluate(req);
    EXPECT_TRUE(got.error);
  }
}

TEST_F(RequestEvaluateTest, MissingAndInvalidFrameId) {
  {
    dap::EvaluateRequest req;
    req.context = "repl";
    req.expression = "1 + 2";
    auto got = SendEvaluate(req);
    ASSERT_TRUE(got.error);
    EXPECT_EQ(got.error.message, "Frame ID is required");
  }

  {
    dap::EvaluateRequest req;
    req.context = "repl";
    req.frameId = 9999;
    req.expression = "1 + 2";
    auto got = SendEvaluate(req);
    ASSERT_TRUE(got.error);
    EXPECT_EQ(got.error.message, "Invalid frame ID");
  }
}

TEST_F(RequestEvaluateTest, ThreadNotStopped) {
  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread(), Location(), 0x2000));
  thread()->GetStack().SetFramesForTest(std::move(frames), true);
  dap::integer frame_id = context().IdForFrame(kThreadKoid, 0);

  dap::EvaluateRequest req;
  req.context = "repl";
  req.frameId = frame_id;
  req.expression = "1 + 2";
  auto got = SendEvaluate(req);
  ASSERT_TRUE(got.error);
  EXPECT_NE(got.error.message.find("Thread should be suspended"), std::string::npos);
}

}  // namespace zxdb
