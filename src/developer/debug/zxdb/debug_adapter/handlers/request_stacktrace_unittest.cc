// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/debug_adapter/handlers/request_stacktrace.h"

#include <gtest/gtest.h>

#include "src/developer/debug/ipc/records.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/mock_remote_api.h"
#include "src/developer/debug/zxdb/client/process.h"
#include "src/developer/debug/zxdb/common/scoped_temp_file.h"
#include "src/developer/debug/zxdb/debug_adapter/context_test.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/loaded_module_symbols.h"
#include "src/developer/debug/zxdb/symbols/mock_module_symbols.h"
#include "src/developer/debug/zxdb/symbols/process_symbols.h"

namespace zxdb {

namespace {

using RequestStackTraceTest = DebugAdapterContextTest;

}  // namespace

TEST_F(RequestStackTraceTest, FullFrameAvailable) {
  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid, 0x1000);
  // Run client to receive process started event.
  RunClient();
  auto thread = InjectThread(kProcessKoid, kThreadKoid);
  // Run client to receive threads started event.
  RunClient();

  // Insert mock frames
  // Top frame has a valid source location
  ScopedTempFile temp_file;
  fxl::RefPtr<Function> function1(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
  function1->set_assigned_name("test_func1");
  function1->set_code_ranges(AddressRanges(AddressRange(0x10000, 0x10020)));
  auto location1 = Location(0x10010, FileLine(temp_file.name(), 23), 10,
                            SymbolContext::ForRelativeAddresses(), function1);

  // The source of this frame cannot be found and will not be reported in response.
  fxl::RefPtr<Function> function2(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
  function2->set_assigned_name("test_func2");
  function2->set_code_ranges(AddressRanges(AddressRange(0x10024, 0x10060)));
  auto location2 =
      Location(0x10040, FileLine("", 0), 0, SymbolContext::ForRelativeAddresses(), function2);

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, location1, 0x2000));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, location2, 0x2020));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);

  // Receive thread stopped event in client.
  RunClient();

  // Send request from the client.
  dap::StackTraceRequest request = {};
  request.threadId = kThreadKoid;
  auto response = client().send(request);

  // Read request and process it in server.
  context().OnStreamReadable();
  loop().RunUntilNoTasks();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_EQ(got.response.totalFrames.value(), 2);
  EXPECT_EQ(got.response.stackFrames[0].column, location1.column());
  EXPECT_EQ(got.response.stackFrames[0].line, location1.file_line().line());
  EXPECT_EQ(got.response.stackFrames[0].name, function1->GetAssignedName());
  EXPECT_EQ(got.response.stackFrames[0].source.value().path.value(), temp_file.name());
  EXPECT_EQ(got.response.stackFrames[1].column, location2.column());
  EXPECT_EQ(got.response.stackFrames[1].line, location2.file_line().line());
  EXPECT_EQ(got.response.stackFrames[1].name, function2->GetAssignedName());
  EXPECT_FALSE(got.response.stackFrames[1].source);
}

TEST_F(RequestStackTraceTest, SyncFramesRequired) {
  InitializeDebugging();

  auto process = InjectProcess(kProcessKoid);
  // Run client to receive process started event.
  RunClient();
  InjectThread(kProcessKoid, kThreadKoid);
  // Run client to receive threads started event.
  RunClient();

  constexpr uint64_t kAddress[] = {0x10010, 0x10040, 0x9000};
  constexpr uint64_t kStack[] = {0x3000, 0x3020, 0x3050};
  constexpr size_t kStackSize = 3;

  // Set up symbol resolution for stack frames.
  ScopedTempFile temp_file;
  auto mock_module = InjectMockModule(process, 0x10000);
  auto mock_module2 = InjectMockModule(process, 0x8000);
  std::vector<fxl::RefPtr<Function>> functions;
  std::vector<Location> locations;

  auto loaded_module1 = process->GetSymbols()->GetLoadedForModuleSymbols(mock_module.get());
  ASSERT_NE(loaded_module1, nullptr);

  for (size_t i = 0; i < kStackSize; i++) {
    // For stack frames that did not recover an exact PC value, the address lookup will actually be
    // the immediately preceding address to get the instruction from the caller rather than the
    // callee. For this test, we simulate the first frame being recovered from "context" which
    // always has an exact PC value, and then the following frames recovered via the return address
    // register.
    uint64_t lookup_address = kAddress[i];
    if (i > 0)
      lookup_address--;

    functions.push_back(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
    functions[i]->set_assigned_name(std::string("test_func_") + std::to_string(i));
    functions[i]->set_code_ranges(
        AddressRanges(AddressRange(kAddress[i] - 0x10, kAddress[i] + 0x10)));
    locations.push_back(Location(lookup_address, FileLine(temp_file.name(), 23 + i), 10 + i,
                                 SymbolContext::ForRelativeAddresses(), functions[i]));
    if (kAddress[i] >= loaded_module1->load_address()) {
      mock_module->AddSymbolLocations(lookup_address, {locations.back()});
    } else {
      mock_module2->AddSymbolLocations(lookup_address, {locations.back()});
    }
  }

  // Notify of thread stop and push expected stack frames.
  debug_ipc::NotifyException break_notification;
  break_notification.type = debug_ipc::ExceptionType::kSoftwareBreakpoint;
  break_notification.thread.id = {.process = kProcessKoid, .thread = kThreadKoid};
  break_notification.thread.state = debug_ipc::ThreadRecord::State::kBlocked;
  break_notification.thread.stack_amount = debug_ipc::ThreadRecord::StackAmount::kFull;
  for (size_t i = 0; i < kStackSize; i++) {
    debug_ipc::StackFrame frame(kAddress[i], kStack[i]);
    if (i > 0) {
      frame.pc_is_return_address = debug_ipc::StackFrame::AddressType::kReturn;
    }
    break_notification.thread.frames.push_back(frame);
  }
  InjectException(break_notification);

  // Receive exception event in client.
  RunClient();

  // Send request from the client.
  dap::StackTraceRequest request = {};
  request.threadId = kThreadKoid;
  auto response = client().send(request);

  // Read request and process it in server.
  context().OnStreamReadable();
  loop().RunUntilNoTasks();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_EQ(static_cast<size_t>(got.response.totalFrames.value()), kStackSize);
  for (size_t i = 0; i < kStackSize; i++) {
    EXPECT_EQ(got.response.stackFrames[i].source.value().path.value(), temp_file.name());
    EXPECT_EQ(got.response.stackFrames[i].column, locations[i].column());
    EXPECT_EQ(got.response.stackFrames[i].line, locations[i].file_line().line());
    EXPECT_EQ(got.response.stackFrames[i].name, functions[i]->GetAssignedName());
  }
}

TEST_F(RequestStackTraceTest, Pagination) {
  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid, 0x1000);
  // Run client to receive process started event.
  RunClient();
  auto thread = InjectThread(kProcessKoid, kThreadKoid);
  // Run client to receive threads started event.
  RunClient();

  // Insert mock frames
  const int kTotalFrames = 10;
  std::vector<std::unique_ptr<Frame>> frames;
  std::vector<Location> locations;
  std::vector<fxl::RefPtr<Function>> functions;
  std::vector<std::unique_ptr<ScopedTempFile>> temp_files;

  for (int i = 0; i < kTotalFrames; i++) {
    temp_files.push_back(std::make_unique<ScopedTempFile>());
    functions.push_back(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
    functions[i]->set_assigned_name("test_func" + std::to_string(i));
    functions[i]->set_code_ranges(
        AddressRanges(AddressRange(0x10000 + i * 0x20, 0x10020 + i * 0x20)));
    locations.emplace_back(0x10010 + i * 0x20, FileLine(temp_files[i]->name(), 23 + i), 10 + i,
                           SymbolContext::ForRelativeAddresses(), functions[i]);
    frames.push_back(
        std::make_unique<MockFrame>(&session(), thread, locations[i], 0x2000 + i * 0x20));
  }

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);

  // Receive thread stopped event in client.
  RunClient();

  // Send request from the client for a subset of frames.
  dap::StackTraceRequest request = {};
  request.threadId = kThreadKoid;
  constexpr int kStartFrame = 2;
  constexpr int kLevels = 3;
  request.startFrame = kStartFrame;
  request.levels = kLevels;
  auto response = client().send(request);

  // Read request and process it in server.
  context().OnStreamReadable();
  loop().RunUntilNoTasks();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_EQ(got.response.totalFrames.value(), kTotalFrames);
  ASSERT_EQ(got.response.stackFrames.size(), (size_t)kLevels);

  // Check the returned frames.
  for (int i = 0; i < kLevels; i++) {
    int frame_index = i + kStartFrame;
    EXPECT_EQ(got.response.stackFrames[i].column, locations[frame_index].column());
    EXPECT_EQ(got.response.stackFrames[i].line, locations[frame_index].file_line().line());
    EXPECT_EQ(got.response.stackFrames[i].name, functions[frame_index]->GetAssignedName());
    EXPECT_EQ(got.response.stackFrames[i].source.value().path.value(),
              temp_files[frame_index]->name());
  }
}

TEST_F(RequestStackTraceTest, PaginationAllFrames) {
  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid, 0x1000);
  // Run client to receive process started event.
  RunClient();
  auto thread = InjectThread(kProcessKoid, kThreadKoid);
  // Run client to receive threads started event.
  RunClient();

  // Insert mock frames
  const int kTotalFrames = 5;
  std::vector<std::unique_ptr<Frame>> frames;
  std::vector<Location> locations;
  std::vector<fxl::RefPtr<Function>> functions;
  std::vector<std::unique_ptr<ScopedTempFile>> temp_files;

  for (int i = 0; i < kTotalFrames; i++) {
    temp_files.push_back(std::make_unique<ScopedTempFile>());
    functions.push_back(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
    functions[i]->set_assigned_name("test_func" + std::to_string(i));
    functions[i]->set_code_ranges(
        AddressRanges(AddressRange(0x10000 + i * 0x20, 0x10020 + i * 0x20)));
    locations.emplace_back(0x10010 + i * 0x20, FileLine(temp_files[i]->name(), 23 + i), 10 + i,
                           SymbolContext::ForRelativeAddresses(), functions[i]);
    frames.push_back(
        std::make_unique<MockFrame>(&session(), thread, locations[i], 0x2000 + i * 0x20));
  }

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);

  // Receive thread stopped event in client.
  RunClient();

  // Send request from the client for all frames.
  dap::StackTraceRequest request = {};
  request.threadId = kThreadKoid;
  request.startFrame = 0;
  request.levels = 0;  // 0 means all frames
  auto response = client().send(request);

  // Read request and process it in server.
  context().OnStreamReadable();
  loop().RunUntilNoTasks();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_EQ(got.response.totalFrames.value(), kTotalFrames);
  ASSERT_EQ(got.response.stackFrames.size(), (size_t)kTotalFrames);

  // Check the returned frames.
  for (int i = 0; i < kTotalFrames; i++) {
    EXPECT_EQ(got.response.stackFrames[i].column, locations[i].column());
    EXPECT_EQ(got.response.stackFrames[i].line, locations[i].file_line().line());
    EXPECT_EQ(got.response.stackFrames[i].name, functions[i]->GetAssignedName());
    EXPECT_EQ(got.response.stackFrames[i].source.value().path.value(), temp_files[i]->name());
  }
}

TEST_F(RequestStackTraceTest, PaginationTooManyFramesRequested) {
  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid, 0x1000);
  // Run client to receive process started event.
  RunClient();
  auto thread = InjectThread(kProcessKoid, kThreadKoid);
  // Run client to receive threads started event.
  RunClient();

  // Insert mock frames
  const int kTotalFrames = 5;
  std::vector<std::unique_ptr<Frame>> frames;
  std::vector<Location> locations;
  std::vector<fxl::RefPtr<Function>> functions;
  std::vector<std::unique_ptr<ScopedTempFile>> temp_files;

  for (int i = 0; i < kTotalFrames; i++) {
    temp_files.push_back(std::make_unique<ScopedTempFile>());
    functions.push_back(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
    functions[i]->set_assigned_name("test_func" + std::to_string(i));
    functions[i]->set_code_ranges(
        AddressRanges(AddressRange(0x10000 + i * 0x20, 0x10020 + i * 0x20)));
    locations.emplace_back(0x10010 + i * 0x20, FileLine(temp_files[i]->name(), 23 + i), 10 + i,
                           SymbolContext::ForRelativeAddresses(), functions[i]);
    frames.push_back(
        std::make_unique<MockFrame>(&session(), thread, locations[i], 0x2000 + i * 0x20));
  }

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);

  // Receive thread stopped event in client.
  RunClient();

  // Send request from the client for a subset of frames.
  constexpr int kStartFrame = 3;
  constexpr int kRequestedLevels = 5;  // Request more than the amount of frames after `startFrame`.

  dap::StackTraceRequest request = {};
  request.threadId = kThreadKoid;
  request.startFrame = kStartFrame;
  request.levels = kRequestedLevels;
  auto response = client().send(request);

  // Read request and process it in server.
  context().OnStreamReadable();
  loop().RunUntilNoTasks();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_EQ(got.response.totalFrames.value(), kTotalFrames);
  size_t expected_frames = kTotalFrames - kStartFrame;
  ASSERT_EQ(got.response.stackFrames.size(), expected_frames);

  // Check the returned frames.
  for (size_t i = 0; i < expected_frames; i++) {
    int frame_index = i + kStartFrame;
    EXPECT_EQ(got.response.stackFrames[i].column, locations[frame_index].column());
    EXPECT_EQ(got.response.stackFrames[i].line, locations[frame_index].file_line().line());
    EXPECT_EQ(got.response.stackFrames[i].name, functions[frame_index]->GetAssignedName());
    EXPECT_EQ(got.response.stackFrames[i].source.value().path.value(),
              temp_files[frame_index]->name());
  }
}

TEST_F(RequestStackTraceTest, PaginationOutOfBoundsStartIndex) {
  InitializeDebugging();

  InjectProcessWithModule(kProcessKoid, 0x1000);
  // Run client to receive process started event.
  RunClient();
  auto thread = InjectThread(kProcessKoid, kThreadKoid);
  // Run client to receive threads started event.
  RunClient();

  // Insert mock frames
  const int kTotalFrames = 5;
  std::vector<std::unique_ptr<Frame>> frames;
  std::vector<Location> locations;
  std::vector<fxl::RefPtr<Function>> functions;
  std::vector<std::unique_ptr<ScopedTempFile>> temp_files;

  for (int i = 0; i < kTotalFrames; i++) {
    temp_files.push_back(std::make_unique<ScopedTempFile>());
    functions.push_back(fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram));
    functions[i]->set_assigned_name("test_func" + std::to_string(i));
    functions[i]->set_code_ranges(
        AddressRanges(AddressRange(0x10000 + i * 0x20, 0x10020 + i * 0x20)));
    locations.emplace_back(0x10010 + i * 0x20, FileLine(temp_files[i]->name(), 23 + i), 10 + i,
                           SymbolContext::ForRelativeAddresses(), functions[i]);
    frames.push_back(
        std::make_unique<MockFrame>(&session(), thread, locations[i], 0x2000 + i * 0x20));
  }

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSingleStep,
                           std::move(frames), true);

  // Receive thread stopped event in client.
  RunClient();

  // Send request from the client for a subset of frames.
  dap::StackTraceRequest request = {};
  request.threadId = kThreadKoid;
  request.startFrame = 8;
  request.levels = 0;  // 0 means all frames
  auto response = client().send(request);

  // Read request and process it in server.
  context().OnStreamReadable();
  loop().RunUntilNoTasks();

  // Run client to receive response.
  RunClient();
  auto got = response.get();
  EXPECT_FALSE(got.error);
  EXPECT_EQ(got.response.totalFrames.value(), kTotalFrames);
  ASSERT_EQ(got.response.stackFrames.size(), 0u);
}

}  // namespace zxdb
