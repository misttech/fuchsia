// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/media/audio/drivers/lib/inspect/recorder.h"

#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/testing/cpp/inspect.h>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"

namespace audio {
namespace {

using ::inspect::testing::BoolIs;
using ::inspect::testing::ChildrenMatch;
using ::inspect::testing::IntIs;
using ::inspect::testing::NameMatches;
using ::inspect::testing::NodeMatches;
using ::inspect::testing::PropertyList;
using ::inspect::testing::UintArrayIs;
using ::inspect::testing::UintIs;
using ::testing::AllOf;
using ::testing::ElementsAre;
using ::testing::IsSupersetOf;
using ::testing::UnorderedElementsAre;

class RecorderTest : public gtest::RealLoopFixture {
 protected:
  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    inspector_ =
        std::make_unique<inspect::ComponentInspector>(dispatcher(), inspect::PublishOptions{});
    recorder_ = std::make_unique<Recorder>(inspector_->root());
  }

  inspect::Hierarchy GetHierarchy() {
    fpromise::result<inspect::Hierarchy> result =
        RunPromise(inspect::ReadFromInspector(inspector_->inspector()));
    EXPECT_TRUE(result.is_ok());
    return std::move(result.value());
  }

  std::unique_ptr<inspect::ComponentInspector> inspector_;
  std::unique_ptr<Recorder> recorder_;
};

TEST_F(RecorderTest, PowerTransitions) {
  zx::time called_at = zx::time(100);
  zx::time completed_at = zx::time(200);

  recorder_->RecordSocPowerUp(called_at, completed_at);
  ASSERT_THAT(
      GetHierarchy(),
      AllOf(NodeMatches(PropertyList(ElementsAre(BoolIs(std::string(kCurrentPowerState), true)))),
            ChildrenMatch(UnorderedElementsAre(
                AllOf(NodeMatches(NameMatches(std::string(kPowerTransitions))),
                      ChildrenMatch(UnorderedElementsAre(
                          NodeMatches(AllOf(NameMatches(std::string("0")),
                                            PropertyList(UnorderedElementsAre(
                                                BoolIs(std::string(kPowerState), true),
                                                IntIs(std::string(kCalledAt), 100),
                                                IntIs(std::string(kEffectiveAt), 200)))))))),
                NodeMatches(NameMatches(std::string(kDAIs))),
                NodeMatches(NameMatches(std::string(kRingBuffers)))))));

  called_at = zx::time(300);
  completed_at = zx::time(400);
  recorder_->RecordSocPowerDown(called_at, completed_at);
  ASSERT_THAT(
      GetHierarchy(),
      AllOf(NodeMatches(PropertyList(ElementsAre(BoolIs(std::string(kCurrentPowerState), false)))),
            ChildrenMatch(UnorderedElementsAre(
                AllOf(NodeMatches(NameMatches(std::string(kPowerTransitions))),
                      ChildrenMatch(UnorderedElementsAre(
                          NodeMatches(AllOf(NameMatches(std::string("0")),
                                            PropertyList(UnorderedElementsAre(
                                                BoolIs(std::string(kPowerState), true),
                                                IntIs(std::string(kCalledAt), 100),
                                                IntIs(std::string(kEffectiveAt), 200))))),
                          NodeMatches(AllOf(NameMatches(std::string("1")),
                                            PropertyList(UnorderedElementsAre(
                                                BoolIs(std::string(kPowerState), false),
                                                IntIs(std::string(kCalledAt), 300),
                                                IntIs(std::string(kEffectiveAt), 400)))))))),
                NodeMatches(NameMatches(std::string(kDAIs))),
                NodeMatches(NameMatches(std::string(kRingBuffers)))))));
}

TEST_F(RecorderTest, RingBufferAndDaiPopulation) {
  recorder_->PopulateRingBuffer("test_ring_buffer", 1, true, true);
  recorder_->PopulateDai("output_dai", 2);

  ASSERT_THAT(
      GetHierarchy(),
      ChildrenMatch(UnorderedElementsAre(
          AllOf(NodeMatches(NameMatches(std::string(kRingBuffers))),
                ChildrenMatch(UnorderedElementsAre(NodeMatches(AllOf(
                    NameMatches(std::string("test_ring_buffer")),
                    PropertyList(IsSupersetOf({UintIs(std::string(kElementId), 1),
                                               BoolIs(std::string(kSupportsActiveChannels), true),
                                               BoolIs(std::string(kIsOutgoingStream), true)}))))))),
          AllOf(NodeMatches(NameMatches(std::string(kDAIs))),
                ChildrenMatch(UnorderedElementsAre(NodeMatches(AllOf(
                    NameMatches(std::string("output_dai")),
                    PropertyList(UnorderedElementsAre(UintIs(std::string(kElementId), 2)))))))))));
}

TEST_F(RecorderTest, RingBufferInstance) {
  recorder_->PopulateRingBuffer("test_ring_buffer", 1, true, true);

  zx::time created_at = zx::time(100);
  auto* rb_recorder = &recorder_->CreateRingBufferInstance(1, created_at);
  ASSERT_THAT(
      GetHierarchy(),
      ChildrenMatch(Contains(AllOf(
          NodeMatches(NameMatches(std::string(kRingBuffers))),
          ChildrenMatch(Contains(AllOf(
              NodeMatches(NameMatches(std::string("test_ring_buffer"))),
              ChildrenMatch(Contains(NodeMatches(AllOf(
                  NameMatches(std::string("instance_0")),
                  PropertyList(UnorderedElementsAre(IntIs(std::string(kCtorTime), 100))))))))))))));

  zx::time destroyed_at = zx::time(200);
  rb_recorder->RecordDestructionTime(destroyed_at);
  ASSERT_THAT(
      GetHierarchy(),
      ChildrenMatch(Contains(AllOf(
          NodeMatches(NameMatches(std::string(kRingBuffers))),
          ChildrenMatch(Contains(AllOf(
              NodeMatches(NameMatches(std::string("test_ring_buffer"))),
              ChildrenMatch(Contains(NodeMatches(AllOf(
                  NameMatches(std::string("instance_0")),
                  PropertyList(UnorderedElementsAre(IntIs(std::string(kCtorTime), 100),
                                                    IntIs(std::string(kDtorTime), 200))))))))))))));
}

TEST_F(RecorderTest, StartAndStop) {
  recorder_->PopulateRingBuffer("test_ring_buffer", 1, true, true);
  auto* rb_recorder = &recorder_->CreateRingBufferInstance(1, zx::time(0));

  zx::time started_at = zx::time(zx::usec(100).get());
  rb_recorder->RecordStartTime(started_at);

  auto hierarchy = GetHierarchy();
  std::vector<std::string> running_interval_path = {std::string(kRingBuffers), "test_ring_buffer",
                                                    "instance_0", std::string(kRunningIntervals),
                                                    "0"};
  auto running_interval_hierarchy = hierarchy.GetByPath(running_interval_path);
  ASSERT_TRUE(running_interval_hierarchy);
  EXPECT_THAT(*running_interval_hierarchy,
              NodeMatches(PropertyList(IsSupersetOf({IntIs(std::string(kStartedAtUs), 100)}))));

  zx::time stopped_at = zx::time(zx::usec(200).get());
  rb_recorder->RecordStopTime(stopped_at);
  hierarchy = GetHierarchy();
  running_interval_hierarchy = hierarchy.GetByPath(running_interval_path);
  ASSERT_TRUE(running_interval_hierarchy);
  EXPECT_THAT(*running_interval_hierarchy,
              NodeMatches(PropertyList(IsSupersetOf({IntIs(std::string(kStartedAtUs), 100),
                                                     IntIs(std::string(kStoppedAtUs), 200),
                                                     IntIs(std::string(kAudioDuration), 100)}))));
}

TEST_F(RecorderTest, ActiveChannels) {
  recorder_->PopulateRingBuffer("test_ring_buffer", 1, true, true);
  auto* rb_recorder = &recorder_->CreateRingBufferInstance(1, zx::time(0));

  zx::time called_at = zx::time(100);
  zx::time completed_at = zx::time(200);
  rb_recorder->RecordActiveChannelsCall(0xff, called_at, completed_at);
  ASSERT_THAT(GetHierarchy(),
              ChildrenMatch(Contains(AllOf(
                  NodeMatches(NameMatches(std::string(kRingBuffers))),
                  ChildrenMatch(Contains(AllOf(
                      NodeMatches(NameMatches(std::string("test_ring_buffer"))),
                      ChildrenMatch(Contains(AllOf(
                          NodeMatches(NameMatches(std::string("instance_0"))),
                          ChildrenMatch(Contains(AllOf(
                              NodeMatches(NameMatches(std::string(kSetActiveChannelsCalls))),
                              ChildrenMatch(Contains(NodeMatches(AllOf(
                                  NameMatches(std::string("0")),
                                  PropertyList(IsSupersetOf(
                                      {UintIs(std::string(kChannelBitmask), 0xff),
                                       IntIs(std::string(kCalledAt), 100),
                                       IntIs(std::string(kEffectiveAt), 200)})))))))))))))))))));
}

TEST_F(RecorderTest, BufferTracker) {
  recorder_->PopulateRingBuffer("test_ring_buffer", 1, true, true);
  auto* rb_recorder = &recorder_->CreateRingBufferInstance(1, zx::time(0));

  rb_recorder->SetupBufferTracker("test_buffer_tracker", 5, zx::msec(10));

  rb_recorder->RecordStartTime(zx::time(100));
  for (uint32_t i = 0; i < 2; i++) {
    auto task = audio::Subtask("task_" + std::to_string(i), /*collect_thread_metrics*/ true);
    task.Start();
    sleep(1);
    task.Done();
    rb_recorder->RecordTaskMetrics(task.FinalMetrics());
  }
  rb_recorder->RecordStopTime(zx::time(200));

  // Simulate some buffer submissions and completions
  rb_recorder->RecordBufferSubmission();
  usleep(1000);
  rb_recorder->RecordBufferCompletion();

  // wait for empty buffer duration to be added.
  usleep(200 * 1000);

  rb_recorder->RecordBufferSubmission();
  usleep(3000);
  rb_recorder->RecordBufferCompletion();

  auto expected_buffer_tracker = AllOf(
      NameMatches(std::string("test_buffer_tracker")),
      PropertyList(IsSupersetOf(std::vector<::testing::Matcher<const ::inspect::PropertyValue&>>{
          UintIs(std::string(kProcessingTimeAvgUsec), testing::Ge(2000)),
          UintIs(std::string(kProcessingTimeMaxUsec), testing::Ge(3000)),
          UintIs(std::string(kEmptyBufferCumulativeDurationUsec), testing::Ge(200 * 1000)),
          UintIs(std::string(kEmptyBufferEpisodeCount), 1),
          UintIs(std::string(kEmptyBufferDurationMaxUsec), testing::Ge(200 * 1000)),
          UintIs(std::string(kFullBufferCumulativeDurationUsec), 0),
          UintIs(std::string(kFullBufferEpisodeCount), 0),
          UintIs(std::string(kFullBufferMaxDurationUsec), 0),
          UintIs(std::string(kCountOutstandingBuffersAvg), 1),
          UintIs(std::string(kProcessingTimeCumulativeUsec), 2 * 10 * 1000)})));

  auto hierarchy = GetHierarchy();
  std::vector<std::string> rb_buffer_tracker_path = {std::string(kRingBuffers), "test_ring_buffer",
                                                     std::string(kDiagnosticsSummary),
                                                     "test_buffer_tracker"};
  const auto rb_buffer_tracker_hierarchy = hierarchy.GetByPath(rb_buffer_tracker_path);
  ASSERT_TRUE(rb_buffer_tracker_hierarchy);
  EXPECT_THAT(*rb_buffer_tracker_hierarchy, NodeMatches(expected_buffer_tracker));

  std::vector<std::string> running_instance_buffer_tracker_path = {std::string(kRingBuffers),
                                                                   "test_ring_buffer",
                                                                   "instance_0",
                                                                   std::string(kRunningIntervals),
                                                                   "0",
                                                                   std::string(kDiagnostics),
                                                                   "test_buffer_tracker"};
  const auto running_instance_buffer_tracker_hierarchy =
      hierarchy.GetByPath(running_instance_buffer_tracker_path);
  ASSERT_TRUE(running_instance_buffer_tracker_hierarchy);
  EXPECT_THAT(*running_instance_buffer_tracker_hierarchy, NodeMatches(expected_buffer_tracker));
}

TEST_F(RecorderTest, AvgTaskMetrics) {
  recorder_->PopulateRingBuffer("test_ring_buffer", 1, true, true);
  auto* rb_recorder = &recorder_->CreateRingBufferInstance(1, zx::time(0));

  // Record first set of metrics.
  rb_recorder->RecordStartTime(zx::time(100));
  for (uint32_t i = 0; i < 2; i++) {
    auto task = audio::Subtask("task_" + std::to_string(i), /*collect_thread_metrics*/ true);
    task.Start();
    usleep(1000);
    task.Done();
    rb_recorder->RecordTaskMetrics(task.FinalMetrics());
  }
  rb_recorder->RecordStopTime(zx::time(200));

  auto expected_avg_metrics =
      AllOf(NameMatches(std::string(kAvg)),
            PropertyList(UnorderedElementsAre(
                IntIs(std::string(kWallTimeUsec), testing::Ge(1000)),
                IntIs(std::string(kCpuTimeUsec), testing::Ge(0)),
                IntIs(std::string(kQueueTimeUsec), testing::Ge(0)),
                IntIs(std::string(kPageFaultTimeUsec), testing::Ge(0)),
                IntIs(std::string(kKernelLockContentionTimeUsec), testing::Ge(0)),
                IntIs(std::string(kStartToStartIntervalUsec), testing::Ge(1000)),
                IntIs(std::string(kEndToEndIntervalUsec), testing::Ge(1000)))));

  auto hierarchy = GetHierarchy();
  std::vector<std::string> rb_avg_metrics_path = {
      std::string(kRingBuffers), "test_ring_buffer", std::string(kDiagnosticsSummary),
      std::string(kTaskRecords), std::string(kAvg),  std::string(kAvg)};
  const auto rb_avg_metrics_hierarchy = hierarchy.GetByPath(rb_avg_metrics_path);
  ASSERT_TRUE(rb_avg_metrics_hierarchy);
  EXPECT_THAT(*rb_avg_metrics_hierarchy, NodeMatches(expected_avg_metrics));

  std::vector<std::string> running_instance_avg_metrics_path = {std::string(kRingBuffers),
                                                                "test_ring_buffer",
                                                                "instance_0",
                                                                std::string(kRunningIntervals),
                                                                "0",
                                                                std::string(kDiagnostics),
                                                                std::string(kTaskRecords),
                                                                std::string(kAvg),
                                                                std::string(kAvg)};
  const auto running_instance_avg_metrics_hierarchy =
      hierarchy.GetByPath(running_instance_avg_metrics_path);
  ASSERT_TRUE(running_instance_avg_metrics_hierarchy);
  EXPECT_THAT(*running_instance_avg_metrics_hierarchy, NodeMatches(expected_avg_metrics));
}

TEST_F(RecorderTest, SchedulingDelayMetrics) {
  recorder_->PopulateRingBuffer("test_ring_buffer", 1, true, true);
  auto* rb_recorder = &recorder_->CreateRingBufferInstance(1, zx::time(0));

  // Set the task schedule interval.
  rb_recorder->SetTaskScheduleInterval(zx::msec(1));

  // Record first set of metrics.
  rb_recorder->RecordStartTime(zx::time(100));
  for (uint32_t i = 0; i < 2; i++) {
    auto task = audio::Subtask("task_" + std::to_string(i), /*collect_thread_metrics*/ true);
    task.Start();
    usleep(1500);  // Sleep for 1.5ms to ensure a scheduling delay.
    task.Done();
    rb_recorder->RecordTaskMetrics(task.FinalMetrics());
  }
  rb_recorder->RecordStopTime(zx::time(200));

  auto expected_min_metrics =
      AllOf(NameMatches(std::string(kMin)),
            PropertyList(Contains(IntIs(std::string("scheduling_delay_us"), testing::Ge(500)))));

  auto hierarchy = GetHierarchy();
  std::vector<std::string> rb_min_metrics_path = {
      std::string(kRingBuffers), "test_ring_buffer", std::string(kDiagnosticsSummary),
      std::string(kTaskRecords), std::string(kMin),  std::string(kMin)};
  const auto rb_min_metrics_hierarchy = hierarchy.GetByPath(rb_min_metrics_path);
  ASSERT_TRUE(rb_min_metrics_hierarchy);
  EXPECT_THAT(*rb_min_metrics_hierarchy, NodeMatches(expected_min_metrics));

  std::vector<std::string> running_instance_min_metrics_path = {std::string(kRingBuffers),
                                                                "test_ring_buffer",
                                                                "instance_0",
                                                                std::string(kRunningIntervals),
                                                                "0",
                                                                std::string(kDiagnostics),
                                                                std::string(kTaskRecords),
                                                                std::string(kMin),
                                                                std::string(kMin)};
  const auto running_instance_min_metrics_hierarchy =
      hierarchy.GetByPath(running_instance_min_metrics_path);
  ASSERT_TRUE(running_instance_min_metrics_hierarchy);
  EXPECT_THAT(*running_instance_min_metrics_hierarchy, NodeMatches(expected_min_metrics));
}

}  // namespace
}  // namespace audio
