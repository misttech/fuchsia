// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/debug/zxdb/client/tracepoint.h"

#include <gtest/gtest.h>

#include "src/developer/debug/shared/message_loop.h"
#include "src/developer/debug/zxdb/client/breakpoint.h"
#include "src/developer/debug/zxdb/client/breakpoint_impl.h"
#include "src/developer/debug/zxdb/client/mock_frame.h"
#include "src/developer/debug/zxdb/client/mock_remote_api.h"
#include "src/developer/debug/zxdb/client/process_impl.h"
#include "src/developer/debug/zxdb/client/remote_api_test.h"
#include "src/developer/debug/zxdb/client/session.h"
#include "src/developer/debug/zxdb/client/system.h"
#include "src/developer/debug/zxdb/client/system_observer.h"
#include "src/developer/debug/zxdb/client/target_impl.h"
#include "src/developer/debug/zxdb/client/thread.h"
#include "src/developer/debug/zxdb/client/thread_controller.h"
#include "src/developer/debug/zxdb/client/thread_impl.h"
#include "src/developer/debug/zxdb/client/tracepoint_capture.h"

namespace zxdb {

namespace {

class TestSystemObserver : public SystemObserver {
 public:
  void DidCreateTracepoint(Tracepoint* tracepoint) override { created.push_back(tracepoint->id()); }

  void WillDestroyTracepoint(Tracepoint* tracepoint) override {
    destroyed.push_back(tracepoint->id());
  }

  void OnTracepointHit(Tracepoint* tracepoint, const TracepointHitRecord& hit) override {
    hits.push_back({.tp_id = tracepoint->id(), .hit = hit});
  }

  struct HitInfo {
    uint32_t tp_id;
    TracepointHitRecord hit;
  };

  std::vector<uint32_t> created;
  std::vector<uint32_t> destroyed;
  std::vector<HitInfo> hits;
};

class TracepointTest : public RemoteAPITest {};

TEST_F(TracepointTest, LifecycleAndSettings) {
  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  ASSERT_NE(tp, nullptr);
  EXPECT_EQ(tp->id(), 1u);
  EXPECT_TRUE(tp->enabled());
  EXPECT_FALSE(tp->auto_disabled());
  EXPECT_EQ(tp->max_hits(), 20u);
  EXPECT_EQ(tp->hit_count(), 0u);
  EXPECT_EQ(observer.created.size(), 1u);
  EXPECT_EQ(observer.created[0], 1u);

  Breakpoint* bp = tp->breakpoint();
  ASSERT_NE(bp, nullptr);
  EXPECT_EQ(bp->GetSettings().stop_mode, BreakpointSettings::StopMode::kNone);
  EXPECT_TRUE(bp->GetSettings().enabled);

  TracepointSpec spec;
  spec.function = "MyFunction";
  spec.condition = "x > 10";
  spec.expressions = {"x", "y"};
  spec.locals = true;
  spec.registers = true;
  spec.raw_backtrace = true;

  tp->SetSpec(spec);
  EXPECT_EQ(tp->spec().function, "MyFunction");
  ASSERT_EQ(bp->GetSettings().locations.size(), 1u);
  EXPECT_EQ(bp->GetSettings().locations[0].name.GetFullName(), "MyFunction");

  tp->SetEnabled(false);
  EXPECT_FALSE(tp->enabled());
  EXPECT_FALSE(bp->GetSettings().enabled);

  tp->SetEnabled(true);
  EXPECT_TRUE(tp->enabled());
  EXPECT_TRUE(bp->GetSettings().enabled);

  auto found = session().system().GetTracepointForBreakpoint(bp);
  EXPECT_EQ(found, tp);

  session().system().DeleteTracepoint(tp);
  EXPECT_EQ(observer.destroyed.size(), 1u);
  EXPECT_EQ(observer.destroyed[0], 1u);
  EXPECT_TRUE(session().system().GetTracepoints().empty());

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, DeleteAllTracepoints) {
  Tracepoint* tp1 = session().system().CreateNewTracepoint();
  Tracepoint* tp2 = session().system().CreateNewTracepoint();
  ASSERT_NE(tp1, nullptr);
  ASSERT_NE(tp2, nullptr);
  EXPECT_EQ(session().system().GetTracepoints().size(), 2u);

  session().system().DeleteAllTracepoints();
  EXPECT_TRUE(session().system().GetTracepoints().empty());
}

TEST_F(TracepointTest, HitFiringAndAutoContinue) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  tp->SetSpec(spec);

  uint32_t bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();

  debug_ipc::BreakpointStats bp_stats;
  bp_stats.id = bp_id;
  bp_stats.hit_count = 1;

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));

  mock_remote_api()->GetAndResetResumeCount();

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), true, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  ASSERT_EQ(observer.hits.size(), 1u);
  EXPECT_EQ(observer.hits[0].tp_id, tp->id());
  EXPECT_GT(mock_remote_api()->GetAndResetResumeCount(), 0);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, AutoDisableAtMaxHits) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  tp->SetSpec(spec);
  tp->set_max_hits(2);

  uint32_t bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats bp_stats;
  bp_stats.id = bp_id;
  bp_stats.hit_count = 1;

  // Hit 1
  std::vector<std::unique_ptr<Frame>> frames1;
  frames1.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                                FileLine("foo.cc", 10)));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames1), true, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  EXPECT_TRUE(tp->enabled());
  EXPECT_FALSE(tp->auto_disabled());

  // Hit 2 (reaches max_hits)
  std::vector<std::unique_ptr<Frame>> frames2;
  frames2.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                                FileLine("foo.cc", 10)));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames2), true, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 2u);
  EXPECT_FALSE(tp->enabled());
  EXPECT_TRUE(tp->auto_disabled());

  // Re-enabling clears auto_disabled
  tp->SetEnabled(true);
  EXPECT_TRUE(tp->enabled());
  EXPECT_FALSE(tp->auto_disabled());
}

TEST_F(TracepointTest, CoLocatedStoppingBreakpoint) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  tp->SetSpec(spec);

  Breakpoint* stop_bp = session().system().CreateNewBreakpoint();
  BreakpointSettings settings;
  settings.enabled = true;
  settings.stop_mode = BreakpointSettings::StopMode::kProcess;
  stop_bp->SetSettings(settings);

  uint32_t tp_bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  uint32_t stop_bp_id = static_cast<BreakpointImpl*>(stop_bp)->backend_id();

  debug_ipc::BreakpointStats tp_stats;
  tp_stats.id = tp_bp_id;
  tp_stats.hit_count = 1;

  debug_ipc::BreakpointStats stop_stats;
  stop_stats.id = stop_bp_id;
  stop_stats.hit_count = 1;

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));

  mock_remote_api()->GetAndResetResumeCount();

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), true, {tp_stats, stop_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  ASSERT_EQ(observer.hits.size(), 1u);
  // Stopping breakpoint must prevent auto-resume.
  EXPECT_EQ(mock_remote_api()->GetAndResetResumeCount(), 0);
  EXPECT_EQ(thread->GetState(), debug_ipc::ThreadRecord::State::kBlocked);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, TracepointHitWhileStepping) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  tp->SetSpec(spec);

  uint32_t tp_bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats tp_stats;
  tp_stats.id = tp_bp_id;
  tp_stats.hit_count = 1;

  class MockStepController : public ThreadController {
   public:
    MockStepController() = default;
    void InitWithThread(Thread* thread, fit::callback<void(const Err&)> cb) override {
      SetThread(thread);
      cb(Err());
    }
    ContinueOp GetContinueOp() override { return ContinueOp::Continue(); }
    StopOp OnThreadStop(debug_ipc::ExceptionType,
                        const std::vector<fxl::WeakPtr<Breakpoint>>&) override {
      return kContinue;
    }
    const char* GetName() const override { return "MockStep"; }
  };

  thread->ContinueWith(std::make_unique<MockStepController>(), [](const Err&) {});

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));

  mock_remote_api()->GetAndResetResumeCount();

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), true, {tp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  ASSERT_EQ(observer.hits.size(), 1u);
  EXPECT_EQ(observer.hits[0].tp_id, tp->id());
  EXPECT_GT(mock_remote_api()->GetAndResetResumeCount(), 0);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, DestroyedMidAsyncCapture) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  spec.backtrace = true;
  tp->SetSpec(spec);

  uint32_t bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats bp_stats;
  bp_stats.id = bp_id;
  bp_stats.hit_count = 1;

  // Provide only 1 frame initially and set has_all_frames = false so EnsureFrames triggers async
  // SyncFrames.
  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), false, {bp_stats});

  // Delete the tracepoint before the async SyncFrames reply is delivered.
  session().system().DeleteTracepoint(tp);

  mock_remote_api()->GetAndResetResumeCount();
  loop().RunUntilNoTasks();

  // Verify no UAF, NotifyTracepointHit was not called, and thread continued.
  EXPECT_TRUE(observer.hits.empty());
  EXPECT_GT(mock_remote_api()->GetAndResetResumeCount(), 0);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, MultipleTracepointsSameStop) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp1 = session().system().CreateNewTracepoint();
  TracepointSpec spec1;
  spec1.function = "MyFunction";
  spec1.condition = "0";
  tp1->SetSpec(spec1);

  Tracepoint* tp2 = session().system().CreateNewTracepoint();
  TracepointSpec spec2;
  spec2.function = "MyFunction";
  spec2.condition = "1";
  tp2->SetSpec(spec2);

  uint32_t bp_id1 = static_cast<BreakpointImpl*>(tp1->breakpoint())->backend_id();
  uint32_t bp_id2 = static_cast<BreakpointImpl*>(tp2->breakpoint())->backend_id();

  debug_ipc::BreakpointStats bp_stats1;
  bp_stats1.id = bp_id1;
  bp_stats1.hit_count = 1;

  debug_ipc::BreakpointStats bp_stats2;
  bp_stats2.id = bp_id2;
  bp_stats2.hit_count = 1;

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));

  mock_remote_api()->GetAndResetResumeCount();

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), true, {bp_stats1, bp_stats2});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp1->hit_count(), 0u);
  EXPECT_EQ(tp2->hit_count(), 1u);
  ASSERT_EQ(observer.hits.size(), 1u);
  EXPECT_EQ(observer.hits[0].tp_id, tp2->id());
  EXPECT_EQ(mock_remote_api()->GetAndResetResumeCount(), 1);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, ReEnableAutoDisabledTracepoint) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  tp->SetSpec(spec);
  tp->set_max_hits(1);

  uint32_t bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats bp_stats;
  bp_stats.id = bp_id;
  bp_stats.hit_count = 1;

  // Hit 1: reaches max_hits=1, becomes auto_disabled.
  std::vector<std::unique_ptr<Frame>> frames1;
  frames1.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                                FileLine("foo.cc", 10)));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames1), true, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  EXPECT_FALSE(tp->enabled());
  EXPECT_TRUE(tp->auto_disabled());
  EXPECT_EQ(observer.hits.size(), 1u);

  // Re-enabling resets hit_count to 0 and auto_disabled to false.
  tp->SetEnabled(true);
  EXPECT_TRUE(tp->enabled());
  EXPECT_FALSE(tp->auto_disabled());
  EXPECT_EQ(tp->hit_count(), 0u);

  // Hit 2: fires normally again.
  std::vector<std::unique_ptr<Frame>> frames2;
  frames2.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                                FileLine("foo.cc", 10)));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames2), true, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  EXPECT_EQ(observer.hits.size(), 2u);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, SetSpecInvalid) {
  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec valid_spec;
  valid_spec.function = "MyFunction";
  tp->SetSpec(valid_spec);
  EXPECT_TRUE(tp->breakpoint()->GetSettings().enabled);

  TracepointSpec invalid_spec;
  bool callback_called = false;
  tp->SetSpec(invalid_spec, [&callback_called](const Err& err) {
    callback_called = true;
    EXPECT_TRUE(err.has_error());
  });
  EXPECT_TRUE(callback_called);
  EXPECT_FALSE(tp->breakpoint()->GetSettings().enabled);
  EXPECT_FALSE(tp->verified());
}

TEST_F(TracepointTest, TargetScopedWatchpointWillDestroyThreadIsolation) {
  constexpr uint64_t kProcess1Koid = 1111;
  constexpr uint64_t kThread1Koid = 1001;
  InjectProcessWithModule(kProcess1Koid, 0x100000);
  Thread* thread1 = InjectThread(kProcess1Koid, kThread1Koid);

  constexpr uint64_t kProcess2Koid = 2222;
  constexpr uint64_t kThread2Koid = 2001;
  InjectProcessWithModule(kProcess2Koid, 0x200000);
  InjectThread(kProcess2Koid, kThread2Koid);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  TracepointWatchSpec watch;
  watch.address = 0x5000;
  watch.access_type = debug_ipc::BreakpointType::kWrite;
  spec.watch = watch;
  tp->SetSpec(spec);

  // Scope the breakpoint to thread1.
  BreakpointSettings settings = tp->breakpoint()->GetSettings();
  settings.scope = ExecutionScope(thread1);
  tp->breakpoint()->SetSettings(settings);

  EXPECT_TRUE(tp->verified());

  // Destroying thread2 on target2 does NOT reset verified() on target1's watchpoint.
  debug_ipc::NotifyThreadExiting notify2;
  notify2.record.id.process = kProcess2Koid;
  notify2.record.id.thread = kThread2Koid;
  session().DispatchNotifyThreadExiting(notify2);
  loop().RunUntilNoTasks();

  EXPECT_TRUE(tp->verified());

  // Destroying the watched thread on target1 DOES reset verified() to false.
  debug_ipc::NotifyThreadExiting notify1;
  notify1.record.id.process = kProcess1Koid;
  notify1.record.id.thread = kThread1Koid;
  session().DispatchNotifyThreadExiting(notify1);
  loop().RunUntilNoTasks();

  EXPECT_FALSE(tp->verified());
}

TEST_F(TracepointTest, CoLocatedOneShotStoppingBreakpoint) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  tp->SetSpec(spec);

  Breakpoint* stop_bp = session().system().CreateNewBreakpoint();
  BreakpointSettings settings;
  settings.enabled = true;
  settings.stop_mode = BreakpointSettings::StopMode::kProcess;
  settings.one_shot = true;
  stop_bp->SetSettings(settings);

  uint32_t tp_bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  uint32_t stop_bp_id = static_cast<BreakpointImpl*>(stop_bp)->backend_id();

  debug_ipc::BreakpointStats tp_stats;
  tp_stats.id = tp_bp_id;
  tp_stats.hit_count = 1;

  debug_ipc::BreakpointStats stop_stats;
  stop_stats.id = stop_bp_id;
  stop_stats.hit_count = 1;
  stop_stats.should_delete = true;

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));

  mock_remote_api()->GetAndResetResumeCount();

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), true, {tp_stats, stop_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  ASSERT_EQ(observer.hits.size(), 1u);
  EXPECT_EQ(mock_remote_api()->GetAndResetResumeCount(), 0);
  EXPECT_EQ(thread->GetState(), debug_ipc::ThreadRecord::State::kBlocked);

  // The one-shot breakpoint should be deleted from system.
  EXPECT_EQ(session().system().GetBreakpoints().size(), 0u);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, MaxMatchingFramesTruncatedHit) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  spec.max_matching_frames = 1;
  tp->SetSpec(spec);

  uint32_t tp_bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats tp_stats;
  tp_stats.id = tp_bp_id;
  tp_stats.hit_count = 1;

  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));

  mock_remote_api()->GetAndResetResumeCount();

  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), true, {tp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  ASSERT_EQ(observer.hits.size(), 1u);
  EXPECT_TRUE(observer.hits[0].hit.truncated);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, ConditionEvaluationErrorReportsHitAndCountsTowardMaxHits) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  spec.condition = "nonexistent_var > 0";
  tp->SetSpec(spec);
  tp->set_max_hits(2);

  uint32_t bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats bp_stats;
  bp_stats.id = bp_id;
  bp_stats.hit_count = 1;

  // First hit fails condition evaluation, emits an error hit record, and auto-resumes.
  std::vector<std::unique_ptr<Frame>> frames1;
  frames1.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                                FileLine("foo.cc", 10)));
  mock_remote_api()->GetAndResetResumeCount();
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames1), true, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  EXPECT_TRUE(tp->enabled());
  EXPECT_FALSE(tp->auto_disabled());
  ASSERT_EQ(observer.hits.size(), 1u);
  EXPECT_FALSE(observer.hits[0].hit.capture.matched);
  EXPECT_FALSE(observer.hits[0].hit.capture.errors.empty());
  EXPECT_FALSE(observer.hits[0].hit.auto_disabled);
  EXPECT_GT(mock_remote_api()->GetAndResetResumeCount(), 0);

  // Second error hit reaches max_hits and auto-disables the tracepoint.
  std::vector<std::unique_ptr<Frame>> frames2;
  frames2.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                                FileLine("foo.cc", 10)));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames2), true, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 2u);
  EXPECT_FALSE(tp->enabled());
  EXPECT_TRUE(tp->auto_disabled());
  ASSERT_EQ(observer.hits.size(), 2u);
  EXPECT_FALSE(observer.hits[1].hit.capture.matched);
  EXPECT_FALSE(observer.hits[1].hit.capture.errors.empty());
  EXPECT_TRUE(observer.hits[1].hit.auto_disabled);

  session().system().RemoveObserver(&observer);
}

TEST_F(TracepointTest, ConcurrentCapturesRespectMaxHits) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThread1Koid = 5678;
  constexpr uint64_t kThread2Koid = 5679;
  Thread* thread1 = InjectThread(kProcessKoid, kThread1Koid);
  Thread* thread2 = InjectThread(kProcessKoid, kThread2Koid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  spec.registers = true;
  tp->SetSpec(spec);
  tp->set_max_hits(1);

  uint32_t bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats bp_stats;
  bp_stats.id = bp_id;
  bp_stats.hit_count = 1;

  // Construct frames with empty synchronous general registers so CaptureTracepointFrame issues
  // asynchronous ReadRegisters requests on both threads before either hit is delivered.
  Location loc(0x100000, FileLine("foo.cc", 10), 0, SymbolContext::ForRelativeAddresses());
  std::vector<std::unique_ptr<Frame>> frames1;
  frames1.push_back(std::make_unique<MockFrame>(&session(), thread1, loc, 0x2000, 0x2010,
                                                std::vector<debug::RegisterValue>{}));
  std::vector<std::unique_ptr<Frame>> frames2;
  frames2.push_back(std::make_unique<MockFrame>(&session(), thread2, loc, 0x3000, 0x3010,
                                                std::vector<debug::RegisterValue>{}));

  mock_remote_api()->GetAndResetResumeCount();
  InjectExceptionWithStack(kProcessKoid, kThread1Koid,
                           debug_ipc::ExceptionType::kSoftwareBreakpoint, std::move(frames1), true,
                           {bp_stats});
  InjectExceptionWithStack(kProcessKoid, kThread2Koid,
                           debug_ipc::ExceptionType::kSoftwareBreakpoint, std::move(frames2), true,
                           {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  EXPECT_FALSE(tp->enabled());
  EXPECT_TRUE(tp->auto_disabled());
  EXPECT_EQ(observer.hits.size(), 1u);
  EXPECT_EQ(mock_remote_api()->GetAndResetResumeCount(), 2);

  session().system().RemoveObserver(&observer);
}

class FailingThreadStatusRemoteAPI : public MockRemoteAPI {
 public:
  void ThreadStatus(const debug_ipc::ThreadStatusRequest& request,
                    fit::callback<void(const Err&, debug_ipc::ThreadStatusReply)> cb) override {
    debug::MessageLoop::Current()->PostTask(FROM_HERE, [cb = std::move(cb)]() mutable {
      cb(Err("Unwind RPC failed"), debug_ipc::ThreadStatusReply());
    });
  }
};

class TracepointUnwindErrorTest : public RemoteAPITest {
 protected:
  std::unique_ptr<RemoteAPI> GetRemoteAPIImpl() override {
    return std::make_unique<FailingThreadStatusRemoteAPI>();
  }
};

TEST_F(TracepointUnwindErrorTest, EnsureFramesFailureRecordedOnHit) {
  constexpr uint64_t kProcessKoid = 1234;
  InjectProcessWithModule(kProcessKoid, 0x100000);
  constexpr uint64_t kThreadKoid = 5678;
  Thread* thread = InjectThread(kProcessKoid, kThreadKoid);

  TestSystemObserver observer;
  session().system().AddObserver(&observer);

  Tracepoint* tp = session().system().CreateNewTracepoint();
  TracepointSpec spec;
  spec.function = "MyFunction";
  spec.backtrace = true;
  tp->SetSpec(spec);

  uint32_t bp_id = static_cast<BreakpointImpl*>(tp->breakpoint())->backend_id();
  debug_ipc::BreakpointStats bp_stats;
  bp_stats.id = bp_id;
  bp_stats.hit_count = 1;

  // Partial stack triggers EnsureFrames, which fails while preserving the initial top frame.
  std::vector<std::unique_ptr<Frame>> frames;
  frames.push_back(std::make_unique<MockFrame>(&session(), thread, 0x100000, 0x2000, "MyFunction",
                                               FileLine("foo.cc", 10)));
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           std::move(frames), false, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 1u);
  ASSERT_EQ(observer.hits.size(), 1u);
  EXPECT_TRUE(observer.hits[0].hit.capture.matched);
  ASSERT_FALSE(observer.hits[0].hit.capture.errors.empty());
  EXPECT_EQ(observer.hits[0].hit.capture.errors[0], "Unwind RPC failed");

  // Empty stack with EnsureFrames failure still delivers an error hit record.
  InjectExceptionWithStack(kProcessKoid, kThreadKoid, debug_ipc::ExceptionType::kSoftwareBreakpoint,
                           {}, false, {bp_stats});
  loop().RunUntilNoTasks();

  EXPECT_EQ(tp->hit_count(), 2u);
  ASSERT_EQ(observer.hits.size(), 2u);
  EXPECT_FALSE(observer.hits[1].hit.capture.matched);
  ASSERT_FALSE(observer.hits[1].hit.capture.errors.empty());
  EXPECT_EQ(observer.hits[1].hit.capture.errors[0], "Unwind RPC failed");

  session().system().RemoveObserver(&observer);
}

}  // namespace

}  // namespace zxdb
