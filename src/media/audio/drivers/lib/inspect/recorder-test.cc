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

  zx::time started_at = zx::time(100);
  rb_recorder->RecordStartTime(started_at);
  ASSERT_THAT(GetHierarchy(),
              ChildrenMatch(Contains(AllOf(
                  NodeMatches(NameMatches(std::string(kRingBuffers))),
                  ChildrenMatch(Contains(AllOf(
                      NodeMatches(NameMatches(std::string("test_ring_buffer"))),
                      ChildrenMatch(Contains(AllOf(
                          NodeMatches(NameMatches(std::string("instance_0"))),
                          ChildrenMatch(Contains(AllOf(
                              NodeMatches(NameMatches(std::string(kRunningIntervals))),
                              ChildrenMatch(Contains(NodeMatches(AllOf(
                                  NameMatches(std::string("0")),
                                  PropertyList(IsSupersetOf(
                                      {IntIs(std::string(kStartedAt), 100)})))))))))))))))))));

  zx::time stopped_at = zx::time(200);
  rb_recorder->RecordStopTime(stopped_at);
  ASSERT_THAT(GetHierarchy(),
              ChildrenMatch(Contains(AllOf(
                  NodeMatches(NameMatches(std::string(kRingBuffers))),
                  ChildrenMatch(Contains(AllOf(
                      NodeMatches(NameMatches(std::string("test_ring_buffer"))),
                      ChildrenMatch(Contains(AllOf(
                          NodeMatches(NameMatches(std::string("instance_0"))),
                          ChildrenMatch(Contains(AllOf(
                              NodeMatches(NameMatches(std::string(kRunningIntervals))),
                              ChildrenMatch(Contains(NodeMatches(AllOf(
                                  NameMatches(std::string("0")),
                                  PropertyList(IsSupersetOf(
                                      {IntIs(std::string(kStartedAt), 100),
                                       IntIs(std::string(kStoppedAt), 200)})))))))))))))))))));
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
          UintIs(std::string("avg_processing_time_us"), testing::Ge(2000)),
          UintIs(std::string("max_processing_time_us"), testing::Ge(3000)),
          UintIs(std::string("total_empty_buffer_duration_us"), testing::Ge(200 * 1000)),
          UintIs(std::string("empty_buffer_episode_count"), 1),
          UintIs(std::string("max_empty_buffer_duration_us"), testing::Ge(200 * 1000)),
          UintIs(std::string("total_full_buffer_duration_us"), 0),
          UintIs(std::string("full_buffer_episode_count"), 0),
          UintIs(std::string("max_full_buffer_duration_us"), 0),
          UintIs(std::string("avg_outstanding_buffer_count"), 1),
          UintIs(std::string("total_buffers_processed_duration_us"), 2 * 10 * 1000)})));

  auto hierarchy = GetHierarchy();
  std::vector<std::string> rb_buffer_tracker_path = {std::string(kRingBuffers), "test_ring_buffer",
                                                     "test_buffer_tracker"};
  const auto rb_buffer_tracker_hierarchy = hierarchy.GetByPath(rb_buffer_tracker_path);
  ASSERT_TRUE(rb_buffer_tracker_hierarchy);
  EXPECT_THAT(*rb_buffer_tracker_hierarchy, NodeMatches(expected_buffer_tracker));

  std::vector<std::string> running_instance_buffer_tracker_path = {
      std::string(kRingBuffers), "test_ring_buffer", "instance_0", "running_intervals", "0",
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
    sleep(1);
    task.Done();
    rb_recorder->RecordTaskMetrics(task.FinalMetrics());
  }
  rb_recorder->RecordStopTime(zx::time(200));

  auto expected_avg_metrics =
      AllOf(NameMatches(std::string("avg_metrics")),
            PropertyList(UnorderedElementsAre(
                AllOf(IntIs(std::string("wall_time_us"), testing::Ge(1 * 1000 * 1000)),
                      IntIs(std::string("wall_time_us"), testing::Le(2 * 1000 * 1000))),
                IntIs(std::string("cpu_time_us"), testing::Ge(0)),
                IntIs(std::string("queue_time_us"), testing::Ge(0)),
                IntIs(std::string("page_fault_time_us"), testing::Ge(0)),
                IntIs(std::string("kernel_lock_contention_time_us"), testing::Ge(0)),
                AllOf(IntIs(std::string("start_to_start_us"), testing::Ge(1 * 1000 * 1000)),
                      IntIs(std::string("start_to_start_us"), testing::Le(2 * 1000 * 1000))),
                AllOf(IntIs(std::string("end_to_end_us"), testing::Ge(1 * 1000 * 1000)),
                      IntIs(std::string("end_to_end_us"), testing::Le(2 * 1000 * 1000))))));

  auto hierarchy = GetHierarchy();
  std::vector<std::string> rb_avg_metrics_path = {std::string(kRingBuffers), "test_ring_buffer",
                                                  "avg_task_records", "avg_metrics"};
  const auto rb_avg_metrics_hierarchy = hierarchy.GetByPath(rb_avg_metrics_path);
  ASSERT_TRUE(rb_avg_metrics_hierarchy);
  EXPECT_THAT(*rb_avg_metrics_hierarchy, NodeMatches(expected_avg_metrics));

  std::vector<std::string> running_instance_avg_metrics_path = {
      std::string(kRingBuffers), "test_ring_buffer", "instance_0", "running_intervals", "0",
      "avg_task_records",        "avg_metrics"};
  const auto running_instance_avg_metrics_hierarchy =
      hierarchy.GetByPath(running_instance_avg_metrics_path);
  ASSERT_TRUE(running_instance_avg_metrics_hierarchy);
  EXPECT_THAT(*running_instance_avg_metrics_hierarchy, NodeMatches(expected_avg_metrics));
}

}  // namespace
}  // namespace audio
