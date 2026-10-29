// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/tracepoint_capture.h"

#include <gtest/gtest.h>

#include "src/developer/debug/shared/message_loop.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/mock_stack_delegate.h"
#include "src/developer/debug/zxdb/client/pretty_stack_manager.h"
#include "src/developer/debug/zxdb/client/remote_api_test.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/stack.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/expr/expr_value.h"
#include "src/developer/debug/zxdb/expr/mock_eval_context.h"
#include "src/developer/debug/zxdb/symbols/base_type.h"
#include "src/developer/debug/zxdb/symbols/code_block.h"
#include "src/developer/debug/zxdb/symbols/collection.h"
#include "src/developer/debug/zxdb/symbols/function.h"
#include "src/developer/debug/zxdb/symbols/location.h"
#include "src/developer/debug/zxdb/symbols/symbol_test_parent_setter.h"
#include "src/developer/debug/zxdb/symbols/type_test_support.h"
#include "src/developer/debug/zxdb/symbols/variable.h"

namespace zxdb {

namespace {

class EvalMockFrame : public MockFrame {
 public:
  using MockFrame::MockFrame;

  void set_custom_eval_context(fxl::RefPtr<EvalContext> context) {
    eval_context_ = std::move(context);
  }

  fxl::RefPtr<EvalContext> GetEvalContext() const override {
    if (eval_context_)
      return eval_context_;
    return MockFrame::GetEvalContext();
  }

 private:
  fxl::RefPtr<EvalContext> eval_context_;
};

class TracepointCaptureTest : public RemoteAPITest {};

TEST_F(TracepointCaptureTest, MatchTracepointFrames) {
  MockStackDelegate delegate(&session());
  Stack stack(&delegate);
  delegate.set_stack(&stack);

  TracepointSpec spec;
  spec.function = "ProcessData";

  // Empty stack returns empty vector.
  auto matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kLiveStopTopFrame);
  ASSERT_TRUE(matched.ok());
  EXPECT_TRUE(matched.value().frames.empty());

  auto phys_frame = std::make_unique<MockFrame>(&session(), nullptr, 0x1000, 0x2000,
                                                "my::ns::ProcessData", FileLine("bar.cc", 20));
  MockFrame* phys_frame_ptr = phys_frame.get();

  auto inline_frame = std::make_unique<MockFrame>(&session(), nullptr, 0x1000, 0x2000,
                                                  "my::ns::Helper", FileLine("foo.cc", 10));
  MockFrame* inline_frame_ptr = inline_frame.get();

  auto caller_frame = std::make_unique<MockFrame>(&session(), nullptr, 0x2000, 0x2020,
                                                  "my::ns::ProcessData", FileLine("baz.cc", 30));
  MockFrame* caller_frame_ptr = caller_frame.get();

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::move(inline_frame));
  frames.push_back(std::move(phys_frame));
  frames.push_back(std::move(caller_frame));
  stack.SetFramesForTest(std::move(frames), true);

  // Exact function suffix match with inline callee traversal at the same PC.
  matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kLiveStopTopFrame);
  ASSERT_TRUE(matched.ok());
  ASSERT_EQ(matched.value().frames.size(), 1u);
  EXPECT_EQ(matched.value().frames[0], phys_frame_ptr);

  // kAllMatchingFrames returns all matching frames across the stack.
  matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kAllMatchingFrames);
  ASSERT_TRUE(matched.ok());
  ASSERT_EQ(matched.value().frames.size(), 2u);
  EXPECT_EQ(matched.value().frames[0], phys_frame_ptr);
  EXPECT_EQ(matched.value().frames[1], caller_frame_ptr);

  // Watch specification matches top frame in kLiveStopTopFrame and rejects kAllMatchingFrames.
  TracepointSpec watch_spec;
  TracepointWatchSpec watch;
  watch.address = 0x2000;
  watch_spec.watch = watch;
  matched = MatchTracepointFrames(&stack, watch_spec, FrameMatchMode::kLiveStopTopFrame);
  ASSERT_TRUE(matched.ok());
  ASSERT_EQ(matched.value().frames.size(), 1u);
  EXPECT_EQ(matched.value().frames[0], inline_frame_ptr);

  matched = MatchTracepointFrames(&stack, watch_spec, FrameMatchMode::kAllMatchingFrames);
  EXPECT_TRUE(matched.has_error());

  // A special identifier such as $plt(...) does not match a normal function with the same name.
  TracepointSpec plt_spec;
  plt_spec.function = "$plt(ProcessData)";
  matched = MatchTracepointFrames(&stack, plt_spec, FrameMatchMode::kAllMatchingFrames);
  ASSERT_TRUE(matched.ok()) << matched.err().msg();
  EXPECT_TRUE(matched.value().frames.empty());
}

TEST_F(TracepointCaptureTest, CaptureRegistersAndSummary) {
  std::vector<debug::RegisterValue> regs;
  debug::RegisterValue reg1;
  reg1.id = debug::RegisterID::kX64_rax;
  reg1.data = {0x34, 0x12, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00};
  regs.push_back(reg1);

  Location loc(0x1000, FileLine("test.cc", 42), 0, SymbolContext::ForRelativeAddresses());
  auto frame = std::make_unique<MockFrame>(&session(), nullptr, loc, 0x2000, 0x2010, regs);

  TracepointSpec spec;
  spec.function = "TestFunc";
  spec.registers = true;

  bool done = false;
  CaptureTracepointFrame(fxl::WeakPtr<Thread>(), frame->GetWeakPtr(), spec,
                         [&done](ErrOr<TracepointFrameCapture> result) {
                           done = true;
                           ASSERT_TRUE(result.ok());
                           const auto& cap = result.value();
                           EXPECT_TRUE(cap.matched);
                           ASSERT_TRUE(cap.frame.has_value());
                           EXPECT_EQ(cap.frame->ip, 0x1000u);
                           EXPECT_EQ(cap.frame->sp, 0x2000u);
                           ASSERT_TRUE(cap.registers.has_value());
                           EXPECT_FALSE(cap.registers->empty());
                           EXPECT_NE(cap.registers->find("rax"), cap.registers->end());
                         });

  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);

  // Empty cached general registers still complete synchronously without an async round-trip.
  auto empty_reg_frame = std::make_unique<MockFrame>(&session(), nullptr, loc, 0x2000, 0x2010,
                                                     std::vector<debug::RegisterValue>{});
  bool empty_done = false;
  CaptureTracepointFrame(fxl::WeakPtr<Thread>(), empty_reg_frame->GetWeakPtr(), spec,
                         [&empty_done](ErrOr<TracepointFrameCapture> result) {
                           empty_done = true;
                           ASSERT_TRUE(result.ok());
                           ASSERT_TRUE(result.value().registers.has_value());
                           EXPECT_TRUE(result.value().registers->empty());
                         });
  EXPECT_TRUE(empty_done);
}

TEST_F(TracepointCaptureTest, ConditionEvaluation) {
  Location loc(0x1000, FileLine("test.cc", 42), 0, SymbolContext::ForRelativeAddresses());
  auto frame = std::make_unique<MockFrame>(&session(), nullptr, loc, 0x2000, 0x2010);

  // Condition true: 1 == 1
  {
    TracepointSpec spec;
    spec.condition = "1 == 1";
    bool done = false;
    CaptureTracepointFrame(fxl::WeakPtr<Thread>(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_TRUE(result.value().matched);
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }

  // Condition false: 1 == 2
  {
    TracepointSpec spec;
    spec.condition = "1 == 2";
    bool done = false;
    CaptureTracepointFrame(fxl::WeakPtr<Thread>(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_FALSE(result.value().matched);
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }

  // Condition error: malformed expression
  {
    TracepointSpec spec;
    spec.condition = "(+";
    bool done = false;
    CaptureTracepointFrame(fxl::WeakPtr<Thread>(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_FALSE(result.value().matched);
                             EXPECT_FALSE(result.value().errors.empty());
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }
}

TEST_F(TracepointCaptureTest, DuplicateExpressions) {
  Location loc(0x1000, FileLine("test.cc", 42), 0, SymbolContext::ForRelativeAddresses());
  auto frame = std::make_unique<MockFrame>(&session(), nullptr, loc, 0x2000, 0x2010);

  TracepointSpec spec;
  spec.expressions = {"10 + 20", "10 + 20", "30 + 40"};

  bool done = false;
  CaptureTracepointFrame(fxl::WeakPtr<Thread>(), frame->GetWeakPtr(), spec,
                         [&done](ErrOr<TracepointFrameCapture> result) {
                           done = true;
                           ASSERT_TRUE(result.ok());
                           const auto& cap = result.value();
                           EXPECT_TRUE(cap.matched);
                           EXPECT_EQ(cap.evaluations.size(), 2u);
                           EXPECT_TRUE(cap.evaluations.count("10 + 20"));
                           EXPECT_TRUE(cap.evaluations.count("30 + 40"));
                         });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);
}

TEST_F(TracepointCaptureTest, MatchTracepointFramesWithAddress) {
  MockStackDelegate delegate(&session());
  Stack stack(&delegate);
  delegate.set_stack(&stack);

  TracepointSpec spec;
  spec.address = 0x1000;

  // Empty stack returns empty vector.
  auto matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kLiveStopTopFrame);
  ASSERT_TRUE(matched.ok());
  EXPECT_TRUE(matched.value().frames.empty());

  auto frame1 = std::make_unique<MockFrame>(&session(), nullptr, 0x1000, 0x2000, "func1",
                                            FileLine("foo.cc", 10));
  MockFrame* frame1_ptr = frame1.get();

  auto frame2 = std::make_unique<MockFrame>(&session(), nullptr, 0x2000, 0x2020, "func2",
                                            FileLine("bar.cc", 20));

  auto frame3 = std::make_unique<MockFrame>(&session(), nullptr, 0x1000, 0x2040, "func3",
                                            FileLine("baz.cc", 30));
  MockFrame* frame3_ptr = frame3.get();

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::move(frame1));
  frames.push_back(std::move(frame2));
  frames.push_back(std::move(frame3));
  stack.SetFramesForTest(std::move(frames), true);

  // Top frame matches address 0x1000.
  matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kLiveStopTopFrame);
  ASSERT_TRUE(matched.ok());
  ASSERT_EQ(matched.value().frames.size(), 1u);
  EXPECT_EQ(matched.value().frames[0], frame1_ptr);

  // Top frame does not match address 0x9999.
  TracepointSpec non_matching_spec;
  non_matching_spec.address = 0x9999;
  matched = MatchTracepointFrames(&stack, non_matching_spec, FrameMatchMode::kLiveStopTopFrame);
  ASSERT_TRUE(matched.ok());
  EXPECT_TRUE(matched.value().frames.empty());

  // kAllMatchingFrames returns all frames matching address 0x1000.
  matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kAllMatchingFrames);
  ASSERT_TRUE(matched.ok());
  ASSERT_EQ(matched.value().frames.size(), 2u);
  EXPECT_EQ(matched.value().frames[0], frame1_ptr);
  EXPECT_EQ(matched.value().frames[1], frame3_ptr);
}

TEST_F(TracepointCaptureTest, MatchTracepointFramesWithFileLine) {
  MockStackDelegate delegate(&session());
  Stack stack(&delegate);
  delegate.set_stack(&stack);

  TracepointSpec spec;
  spec.file_line = FileLine("target.cc", 42);

  // Set up an inline frame and physical frame sharing the top PC (0x1000).
  auto frame_inline = std::make_unique<MockFrame>(&session(), nullptr, 0x1000, 0x2000,
                                                  "InlinedFunc", FileLine("target.cc", 42));
  MockFrame* inline_ptr = frame_inline.get();

  auto frame_phys = std::make_unique<MockFrame>(&session(), nullptr, 0x1000, 0x2000, "PhysFunc",
                                                FileLine("other.cc", 10));

  auto frame_other = std::make_unique<MockFrame>(&session(), nullptr, 0x2000, 0x2020, "CallerFunc",
                                                 FileLine("target.cc", 42));
  MockFrame* other_ptr = frame_other.get();

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::move(frame_inline));
  frames.push_back(std::move(frame_phys));
  frames.push_back(std::move(frame_other));
  stack.SetFramesForTest(std::move(frames), true);

  // In kLiveStopTopFrame, matches inline frame sharing top IP with matching file_line.
  auto matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kLiveStopTopFrame);
  ASSERT_TRUE(matched.ok());
  ASSERT_EQ(matched.value().frames.size(), 1u);
  EXPECT_EQ(matched.value().frames[0], inline_ptr);

  // In kAllMatchingFrames, matches all frames across stack matching file_line.
  matched = MatchTracepointFrames(&stack, spec, FrameMatchMode::kAllMatchingFrames);
  ASSERT_TRUE(matched.ok());
  ASSERT_EQ(matched.value().frames.size(), 2u);
  EXPECT_EQ(matched.value().frames[0], inline_ptr);
  EXPECT_EQ(matched.value().frames[1], other_ptr);
}

TEST_F(TracepointCaptureTest, CaptureLocals) {
  Location loc(0x1000, FileLine("test.cc", 42), 0, SymbolContext::ForRelativeAddresses());
  auto frame = std::make_unique<MockFrame>(&session(), nullptr, loc, 0x2000, 0x2010);

  TracepointSpec spec;
  spec.function = "TestFunc";
  spec.locals = true;

  bool done = false;
  CaptureTracepointFrame(fxl::WeakPtr<Thread>(), frame->GetWeakPtr(), spec,
                         [&done](ErrOr<TracepointFrameCapture> result) {
                           done = true;
                           ASSERT_TRUE(result.ok());
                           const auto& cap = result.value();
                           EXPECT_TRUE(cap.matched);
                           ASSERT_TRUE(cap.locals.has_value());
                           EXPECT_TRUE(cap.locals->empty());
                           // A frame without symbols reports why no locals were captured.
                           ASSERT_EQ(cap.errors.size(), 1u);
                           EXPECT_EQ(cap.errors[0],
                                     "There is no symbol information for the frame.");
                         });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);
}

TEST_F(TracepointCaptureTest, CaptureBacktrace) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x1000, 0x2000, "Func1",
                                               FileLine("foo.cc", 10)));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x2000, 0x2020, "Func2",
                                               FileLine("bar.cc", 20)));
  thread->GetStack().SetFramesForTest(std::move(frames), true);

  TracepointSpec spec;
  spec.function = "Func1";
  spec.backtrace = true;

  bool done = false;
  CaptureTracepointFrame(thread->GetWeakPtr(), thread->GetStack()[0]->GetWeakPtr(), spec,
                         [&done](ErrOr<TracepointFrameCapture> result) {
                           done = true;
                           ASSERT_TRUE(result.ok());
                           const auto& cap = result.value();
                           EXPECT_TRUE(cap.matched);
                           ASSERT_TRUE(cap.backtrace.has_value());
                           EXPECT_EQ(cap.backtrace->size(), 2u);
                           EXPECT_EQ((*cap.backtrace)[0].ip, 0x1000u);
                           EXPECT_EQ((*cap.backtrace)[1].ip, 0x2000u);
                         });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);
}

TEST_F(TracepointCaptureTest, NonTopmostFrameRegisters) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  std::vector<debug::RegisterValue> regs;
  debug::RegisterValue reg1;
  reg1.id = debug::RegisterID::kX64_rax;
  reg1.data = {0x78, 0x56, 0x34, 0x12, 0x00, 0x00, 0x00, 0x00};
  regs.push_back(reg1);

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x1000, 0x2000, "TopFunc",
                                               FileLine("foo.cc", 10)));
  frames.push_back(std::make_unique<MockFrame>(
      &session(), thread,
      Location(0x2000, FileLine("bar.cc", 20), 0, SymbolContext::ForRelativeAddresses()), 0x3000,
      0x3010, regs));
  thread->GetStack().SetFramesForTest(std::move(frames), true);

  TracepointSpec spec;
  spec.registers = true;

  bool done = false;
  CaptureTracepointFrame(thread->GetWeakPtr(), thread->GetStack()[1]->GetWeakPtr(), spec,
                         [&done](ErrOr<TracepointFrameCapture> result) {
                           done = true;
                           ASSERT_TRUE(result.ok());
                           const auto& cap = result.value();
                           EXPECT_TRUE(cap.matched);
                           ASSERT_TRUE(cap.registers.has_value());
                           EXPECT_EQ(cap.registers->at("rax"), "0x12345678");
                           EXPECT_TRUE(cap.errors.empty());
                         });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);
}

TEST_F(TracepointCaptureTest, RawBacktraceImpliesBacktrace) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x1000, 0x2000, "Func1",
                                               FileLine("foo.cc", 10)));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x2000, 0x2020, "Func2",
                                               FileLine("bar.cc", 20)));
  thread->GetStack().SetFramesForTest(std::move(frames), true);

  TracepointSpec spec;
  spec.function = "Func1";
  spec.raw_backtrace = true;
  spec.backtrace = false;

  bool done = false;
  CaptureTracepointFrame(thread->GetWeakPtr(), thread->GetStack()[0]->GetWeakPtr(), spec,
                         [&done](ErrOr<TracepointFrameCapture> result) {
                           done = true;
                           ASSERT_TRUE(result.ok());
                           const auto& cap = result.value();
                           EXPECT_TRUE(cap.matched);
                           ASSERT_TRUE(cap.backtrace.has_value());
                           EXPECT_EQ(cap.backtrace->size(), 2u);
                         });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);
}

TEST_F(TracepointCaptureTest, TracepointSpecValidate) {
  // Empty spec -> error.
  {
    TracepointSpec spec;
    EXPECT_TRUE(spec.Validate().has_error());
  }

  // Mutually exclusive target fields set simultaneously -> error.
  {
    TracepointSpec spec;
    spec.function = "MyFunc";
    spec.file_line = FileLine("foo.cc", 10);
    EXPECT_TRUE(spec.Validate().has_error());
  }
  {
    TracepointSpec spec;
    spec.function = "MyFunc";
    spec.address = 0x1000;
    EXPECT_TRUE(spec.Validate().has_error());
  }
  {
    TracepointSpec spec;
    spec.file_line = FileLine("foo.cc", 10);
    TracepointWatchSpec watch;
    watch.address = 0x2000;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().has_error());
  }
  {
    TracepointSpec spec;
    spec.address = 0x1000;
    TracepointWatchSpec watch;
    watch.address = 0x2000;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().has_error());
  }

  // file_line with empty file string or invalid FileLine -> error.
  {
    TracepointSpec spec;
    spec.file_line = FileLine();
    EXPECT_TRUE(spec.Validate().has_error());
  }
  {
    TracepointSpec spec;
    spec.file_line = FileLine("", 0);
    EXPECT_TRUE(spec.Validate().has_error());
  }

  // watch without a resolved address -> error even when expression is set.
  {
    TracepointSpec spec;
    TracepointWatchSpec watch;
    watch.expression = "";
    watch.address = std::nullopt;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().has_error());
  }
  {
    TracepointSpec spec;
    TracepointWatchSpec watch;
    watch.expression = "x";
    watch.address = std::nullopt;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().has_error());
  }

  // watch with unsupported access_type -> error.
  {
    TracepointSpec spec;
    TracepointWatchSpec watch;
    watch.address = 0x1000;
    watch.access_type = debug_ipc::BreakpointType::kSoftware;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().has_error());
  }
  {
    TracepointSpec spec;
    TracepointWatchSpec watch;
    watch.address = 0x1000;
    watch.expression = "x";
    watch.access_type = debug_ipc::BreakpointType::kHardware;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().has_error());
  }

  // Valid specs -> ok.
  {
    TracepointSpec spec;
    spec.function = "MyFunc";
    EXPECT_TRUE(spec.Validate().ok());
  }
  {
    TracepointSpec spec;
    spec.file_line = FileLine("foo.cc", 42);
    EXPECT_TRUE(spec.Validate().ok());
  }
  {
    TracepointSpec spec;
    spec.address = 0x1000;
    EXPECT_TRUE(spec.Validate().ok());
  }
  {
    TracepointSpec spec;
    TracepointWatchSpec watch;
    watch.expression = "x";
    watch.address = 0x1000;
    watch.access_type = debug_ipc::BreakpointType::kWrite;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().ok());
  }
  {
    TracepointSpec spec;
    TracepointWatchSpec watch;
    watch.address = 0x2000;
    watch.access_type = debug_ipc::BreakpointType::kReadWrite;
    spec.watch = watch;
    EXPECT_TRUE(spec.Validate().ok());
  }
}

TEST_F(TracepointCaptureTest, MatchTracepointFramesAdversarial) {
  MockStackDelegate delegate(&session());
  Stack stack(&delegate);
  delegate.set_stack(&stack);

  TracepointSpec spec;
  spec.function = "TargetFunc";

  // Empty stack returns empty vector without crashing in both modes.
  {
    auto res1 = MatchTracepointFrames(&stack, spec, FrameMatchMode::kLiveStopTopFrame);
    ASSERT_TRUE(res1.ok());
    EXPECT_TRUE(res1.value().frames.empty());

    auto res2 = MatchTracepointFrames(&stack, spec, FrameMatchMode::kAllMatchingFrames);
    ASSERT_TRUE(res2.ok());
    EXPECT_TRUE(res2.value().frames.empty());
  }

  // FrameMatchMode::kLiveStopTopFrame falls back to frame 0 when no inline frame at the top IP
  // matches spec.function or spec.file_line, without walking into caller frames at lower IPs.
  {
    auto frame0 = std::make_unique<MockFrame>(&session(), nullptr, 0x1000, 0x2000, "UnrelatedFunc",
                                              FileLine("unrelated.cc", 10));
    MockFrame* frame0_ptr = frame0.get();
    auto frame1 = std::make_unique<MockFrame>(&session(), nullptr, 0x2000, 0x2020, "TargetFunc",
                                              FileLine("target.cc", 20));

    std::vector<std::unique_ptr<Frame>> frames;
    frames.push_back(std::move(frame0));
    frames.push_back(std::move(frame1));
    stack.SetFramesForTest(std::move(frames), true);

    auto res = MatchTracepointFrames(&stack, spec, FrameMatchMode::kLiveStopTopFrame);
    ASSERT_TRUE(res.ok());
    ASSERT_EQ(res.value().frames.size(), 1u);
    EXPECT_EQ(res.value().frames[0], frame0_ptr);

    TracepointSpec fl_spec;
    fl_spec.file_line = FileLine("target.cc", 20);
    auto fl_res = MatchTracepointFrames(&stack, fl_spec, FrameMatchMode::kLiveStopTopFrame);
    ASSERT_TRUE(fl_res.ok());
    ASSERT_EQ(fl_res.value().frames.size(), 1u);
    EXPECT_EQ(fl_res.value().frames[0], frame0_ptr);
  }

  // Invalid specs (empty, empty function, multiple targets, watch without address) fail validation
  // in both matching modes instead of falling through.
  {
    TracepointSpec empty_spec;
    EXPECT_TRUE(
        MatchTracepointFrames(&stack, empty_spec, FrameMatchMode::kLiveStopTopFrame).has_error());
    EXPECT_TRUE(
        MatchTracepointFrames(&stack, empty_spec, FrameMatchMode::kAllMatchingFrames).has_error());

    TracepointSpec empty_func_spec;
    empty_func_spec.function = "";
    EXPECT_TRUE(MatchTracepointFrames(&stack, empty_func_spec, FrameMatchMode::kLiveStopTopFrame)
                    .has_error());
    EXPECT_TRUE(MatchTracepointFrames(&stack, empty_func_spec, FrameMatchMode::kAllMatchingFrames)
                    .has_error());

    TracepointSpec multi_target_spec;
    multi_target_spec.function = "TargetFunc";
    multi_target_spec.address = 0x1000;
    EXPECT_TRUE(MatchTracepointFrames(&stack, multi_target_spec, FrameMatchMode::kLiveStopTopFrame)
                    .has_error());

    TracepointSpec unresolved_watch_spec;
    TracepointWatchSpec watch;
    watch.expression = "x";
    unresolved_watch_spec.watch = watch;
    EXPECT_TRUE(
        MatchTracepointFrames(&stack, unresolved_watch_spec, FrameMatchMode::kLiveStopTopFrame)
            .has_error());
  }

  // FrameMatchMode::kAllMatchingFrames recursion cap: stack with >10 matching frames caps at 10.
  {
    std::vector<std::unique_ptr<Frame>> frames;
    frames.reserve(15);
    for (size_t i = 0; i < 15; ++i) {
      frames.push_back(std::make_unique<MockFrame>(&session(), nullptr, 0x1000 + (i * 0x10),
                                                   0x2000 + (i * 0x20), "RecursiveFunc",
                                                   FileLine("recurse.cc", 10)));
    }
    stack.SetFramesForTest(std::move(frames), true);

    TracepointSpec recurse_spec;
    recurse_spec.function = "RecursiveFunc";
    // With max_matching_frames = std::nullopt (default), returns all 15 frames with truncated ==
    // false.
    auto res = MatchTracepointFrames(&stack, recurse_spec, FrameMatchMode::kAllMatchingFrames);
    ASSERT_TRUE(res.ok());
    EXPECT_EQ(res.value().frames.size(), 15u);
    EXPECT_FALSE(res.value().truncated);

    // With max_matching_frames = 10, returns 10 frames with truncated == true.
    recurse_spec.max_matching_frames = 10;
    auto capped_res =
        MatchTracepointFrames(&stack, recurse_spec, FrameMatchMode::kAllMatchingFrames);
    ASSERT_TRUE(capped_res.ok());
    EXPECT_EQ(capped_res.value().frames.size(), 10u);
    EXPECT_TRUE(capped_res.value().truncated);

    // Validate() and MatchTracepointFrames() both reject max_matching_frames = 0.
    recurse_spec.max_matching_frames = 0;
    EXPECT_TRUE(recurse_spec.Validate().has_error());
    EXPECT_TRUE(MatchTracepointFrames(&stack, recurse_spec, FrameMatchMode::kAllMatchingFrames)
                    .has_error());
    EXPECT_TRUE(
        MatchTracepointFrames(&stack, recurse_spec, FrameMatchMode::kLiveStopTopFrame).has_error());
  }

  // Malformed spec.function identifier string (e.g. "foo::(") returns Err cleanly.
  {
    TracepointSpec bad_spec;
    bad_spec.function = "foo::(";
    auto res = MatchTracepointFrames(&stack, bad_spec, FrameMatchMode::kLiveStopTopFrame);
    EXPECT_TRUE(res.has_error());
  }
}

TEST_F(TracepointCaptureTest, CaptureTracepointFrameAdversarial) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  Location loc(0x1000, FileLine("test.cc", 42), 0, SymbolContext::ForRelativeAddresses());
  auto frame = std::make_unique<MockFrame>(&session(), thread, loc, 0x2000, 0x2010);

  // Null/invalid weak_frame passed to CaptureTracepointFrame -> returns Err via callback.
  {
    TracepointSpec spec;
    spec.function = "TestFunc";
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), fxl::WeakPtr<Frame>(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             EXPECT_TRUE(result.has_error());
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }

  // Condition evaluation failure (syntax error "1 + *" or undefined variable "nonexistent_var"):
  // verify callback succeeds with capture.matched == false and errors populated.
  {
    TracepointSpec spec;
    spec.condition = "1 + *";
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_FALSE(result.value().matched);
                             EXPECT_FALSE(result.value().errors.empty());
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }
  {
    TracepointSpec spec;
    spec.condition = "nonexistent_var";
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_FALSE(result.value().matched);
                             EXPECT_FALSE(result.value().errors.empty());
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }

  // Condition returning non-scalar/struct (e.g. (void)0) vs boolean/integer 0 (not matched)
  // vs non-zero integer/pointer (matched).
  {
    TracepointSpec spec;
    spec.condition = "(void)0";
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_FALSE(result.value().matched);
                             EXPECT_FALSE(result.value().errors.empty());
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }
  {
    TracepointSpec spec;
    spec.condition = "0";
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_FALSE(result.value().matched);
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }
  {
    TracepointSpec spec;
    spec.condition = "42";
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_TRUE(result.value().matched);
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }
  {
    TracepointSpec spec;
    spec.condition = "(void*)0x1234";
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             EXPECT_TRUE(result.value().matched);
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }

  // Multiple expressions where one succeeds ("1 + 2") and one fails ("bad_ident"):
  // verify both entries exist in evaluations, valid one has result/type, invalid one has error,
  // and capture.errors records the error.
  {
    TracepointSpec spec;
    spec.function = "TestFunc";
    spec.expressions = {"1 + 2", "bad_ident"};
    bool done = false;
    CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                           [&done](ErrOr<TracepointFrameCapture> result) {
                             done = true;
                             ASSERT_TRUE(result.ok());
                             const auto& cap = result.value();
                             EXPECT_TRUE(cap.matched);
                             EXPECT_EQ(cap.evaluations.size(), 2u);
                             ASSERT_TRUE(cap.evaluations.count("1 + 2"));
                             EXPECT_EQ(cap.evaluations.at("1 + 2").result, "3");
                             ASSERT_TRUE(cap.evaluations.at("1 + 2").type.has_value());
                             EXPECT_FALSE(cap.evaluations.at("1 + 2").type->empty());
                             ASSERT_TRUE(cap.evaluations.at("bad_ident").error.has_value());
                             EXPECT_FALSE(cap.evaluations.at("bad_ident").error->empty());
                             EXPECT_FALSE(cap.errors.empty());
                           });
    loop().RunUntilNoTasks();
    EXPECT_TRUE(done);
  }

  // raw_backtrace = true vs backtrace = true with PrettyStackManager matchers:
  // backtrace = true elides matching frames (elided == true, non-empty elide_description),
  // whereas raw_backtrace = true captures all frames with elided == false.
  {
    std::vector<std::unique_ptr<Frame>> frames;
    frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x1000, 0x2000, "TopFunc",
                                                 FileLine("top.cc", 10)));
    frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x2000, 0x2020, "Collapse1",
                                                 FileLine("c.cc", 20)));
    frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x3000, 0x2040, "Collapse2",
                                                 FileLine("c.cc", 30)));
    thread->GetStack().SetFramesForTest(std::move(frames), true);

    auto pretty_manager = session().system().pretty_stack_manager();
    ASSERT_TRUE(pretty_manager);
    PrettyStackManager::StackGlob glob(
        "CustomCollapse", {PrettyFrameGlob::Func("Collapse1"), PrettyFrameGlob::Func("Collapse2")});
    pretty_manager->SetMatchers({glob});

    // Case A: backtrace = true -> elision occurs.
    {
      TracepointSpec spec;
      spec.function = "TopFunc";
      spec.backtrace = true;
      bool done = false;
      CaptureTracepointFrame(thread->GetWeakPtr(), thread->GetStack()[0]->GetWeakPtr(), spec,
                             [&done](ErrOr<TracepointFrameCapture> result) {
                               done = true;
                               ASSERT_TRUE(result.ok());
                               const auto& cap = result.value();
                               ASSERT_TRUE(cap.backtrace.has_value());
                               ASSERT_EQ(cap.backtrace->size(), 3u);
                               EXPECT_FALSE((*cap.backtrace)[0].elided);
                               EXPECT_FALSE((*cap.backtrace)[1].elided);
                               EXPECT_EQ((*cap.backtrace)[1].elide_description, "CustomCollapse");
                               EXPECT_TRUE((*cap.backtrace)[2].elided);
                               EXPECT_EQ((*cap.backtrace)[2].elide_description, "CustomCollapse");
                             });
      loop().RunUntilNoTasks();
      EXPECT_TRUE(done);
    }

    // Case B: raw_backtrace = true (even with backtrace = false initially) -> no elision.
    {
      TracepointSpec spec;
      spec.function = "TopFunc";
      spec.raw_backtrace = true;
      spec.backtrace = false;
      bool done = false;
      CaptureTracepointFrame(thread->GetWeakPtr(), thread->GetStack()[0]->GetWeakPtr(), spec,
                             [&done](ErrOr<TracepointFrameCapture> result) {
                               done = true;
                               ASSERT_TRUE(result.ok());
                               const auto& cap = result.value();
                               ASSERT_TRUE(cap.backtrace.has_value());
                               ASSERT_EQ(cap.backtrace->size(), 3u);
                               for (const auto& rec : *cap.backtrace) {
                                 EXPECT_FALSE(rec.elided);
                                 EXPECT_TRUE(rec.elide_description.empty());
                               }
                             });
      loop().RunUntilNoTasks();
      EXPECT_TRUE(done);
    }

    // Case C: backtrace = true for non-top matched frame (stack[2]) matching glob -> not elided.
    {
      TracepointSpec spec;
      spec.function = "Collapse2";
      spec.backtrace = true;
      bool done = false;
      CaptureTracepointFrame(thread->GetWeakPtr(), thread->GetStack()[2]->GetWeakPtr(), spec,
                             [&done](ErrOr<TracepointFrameCapture> result) {
                               done = true;
                               ASSERT_TRUE(result.ok());
                               const auto& cap = result.value();
                               ASSERT_TRUE(cap.backtrace.has_value());
                               ASSERT_EQ(cap.backtrace->size(), 3u);
                               for (const auto& rec : *cap.backtrace) {
                                 EXPECT_FALSE(rec.elided);
                               }
                             });
      loop().RunUntilNoTasks();
      EXPECT_TRUE(done);
    }
  }
}

TEST_F(TracepointCaptureTest, CompositeCollectionFormatting) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  auto function = fxl::MakeRefCounted<Function>(DwarfTag::kSubprogram);
  function->set_assigned_name("TestFunc");
  function->set_code_ranges(AddressRanges(AddressRange(0x1000, 0x2000)));

  auto block = fxl::MakeRefCounted<CodeBlock>(DwarfTag::kLexicalBlock);
  block->set_code_ranges(AddressRanges(AddressRange(0x1000, 0x2000)));
  SymbolTestParentSetter parent(block, function);

  auto int32_type = MakeInt32Type();
  auto struct_type =
      MakeCollectionType(DwarfTag::kStructureType, "Point", {{"a", int32_type}, {"b", int32_type}});

  auto local_var = fxl::MakeRefCounted<Variable>(DwarfTag::kVariable);
  local_var->set_assigned_name("pt");
  local_var->set_type(struct_type);
  block->set_variables({LazySymbol(local_var)});
  function->set_inner_blocks({LazySymbol(block)});

  Location location(0x1050, FileLine("file.cc", 10), 0, SymbolContext::ForRelativeAddresses(),
                    function);

  auto mock_eval = fxl::MakeRefCounted<MockEvalContext>();
  ExprValue struct_val(struct_type, {0x01, 0x00, 0x00, 0x00, 0x02, 0x00, 0x00, 0x00});
  mock_eval->AddVariable(local_var.get(), struct_val);
  mock_eval->AddVariable("pt_expr", struct_val);

  auto frame = std::make_unique<EvalMockFrame>(&session(), thread, location, 0x2000, 0x2010);
  frame->set_custom_eval_context(mock_eval);

  TracepointSpec spec;
  spec.locals = true;
  spec.expressions = {"pt_expr"};

  bool done = false;
  CaptureTracepointFrame(thread->GetWeakPtr(), frame->GetWeakPtr(), spec,
                         [&done](ErrOr<TracepointFrameCapture> result) {
                           done = true;
                           ASSERT_TRUE(result.ok());
                           const auto& cap = result.value();
                           EXPECT_TRUE(cap.matched);

                           ASSERT_TRUE(cap.locals.has_value());
                           ASSERT_EQ(cap.locals->count("pt"), 1u);
                           EXPECT_EQ(cap.locals->at("pt").value, "{a = 1, b = 2}");
                           EXPECT_EQ(cap.locals->at("pt").type, "Point");

                           ASSERT_EQ(cap.evaluations.count("pt_expr"), 1u);
                           EXPECT_EQ(cap.evaluations.at("pt_expr").result, "{a = 1, b = 2}");
                           EXPECT_EQ(cap.evaluations.at("pt_expr").type, "Point");
                         });
  loop().RunUntilNoTasks();
  EXPECT_TRUE(done);
}

TEST_F(TracepointCaptureTest, CaptureThreadBacktraceDirect) {
  EXPECT_TRUE(CaptureThreadBacktrace(nullptr).empty());

  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x1000, 0x2000, "GroupA1",
                                               FileLine("a.cc", 10)));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x2000, 0x2020, "GroupA2",
                                               FileLine("a.cc", 20)));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x3000, 0x2040, "GroupB1",
                                               FileLine("b.cc", 10)));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x4000, 0x2060, "GroupB2",
                                               FileLine("b.cc", 20)));
  thread->GetStack().SetFramesForTest(std::move(frames), true);

  auto pretty_manager = session().system().pretty_stack_manager();
  ASSERT_TRUE(pretty_manager);
  pretty_manager->SetMatchers({
      PrettyStackManager::StackGlob(
          "GroupA", {PrettyFrameGlob::Func("GroupA1"), PrettyFrameGlob::Func("GroupA2")}),
      PrettyStackManager::StackGlob(
          "GroupB", {PrettyFrameGlob::Func("GroupB1"), PrettyFrameGlob::Func("GroupB2")}),
  });

  // Without matched frames, both groups elide their trailing frames.
  auto default_bt = CaptureThreadBacktrace(thread);
  ASSERT_EQ(default_bt.size(), 4u);
  EXPECT_FALSE(default_bt[0].elided);
  EXPECT_TRUE(default_bt[1].elided);
  EXPECT_FALSE(default_bt[2].elided);
  EXPECT_TRUE(default_bt[3].elided);

  // Passing matched frames from MatchTracepointFrames un-elides only the group containing a match.
  TracepointSpec spec;
  spec.function = "GroupB2";
  auto match_res =
      MatchTracepointFrames(&thread->GetStack(), spec, FrameMatchMode::kAllMatchingFrames);
  ASSERT_TRUE(match_res.ok());
  ASSERT_EQ(match_res.value().frames.size(), 1u);

  auto selective_bt =
      CaptureThreadBacktrace(thread, /*raw_backtrace=*/false, match_res.value().frames);
  ASSERT_EQ(selective_bt.size(), 4u);
  EXPECT_FALSE(selective_bt[0].elided);
  EXPECT_TRUE(selective_bt[1].elided);
  EXPECT_FALSE(selective_bt[2].elided);
  EXPECT_FALSE(selective_bt[3].elided);

  // Raw backtrace leaves all frames un-elided with empty descriptions.
  auto raw_bt = CaptureThreadBacktrace(thread, /*raw_backtrace=*/true);
  ASSERT_EQ(raw_bt.size(), 4u);
  for (const auto& rec : raw_bt) {
    EXPECT_FALSE(rec.elided);
    EXPECT_TRUE(rec.elide_description.empty());
  }
}

}  // namespace

}  // namespace zxdb
