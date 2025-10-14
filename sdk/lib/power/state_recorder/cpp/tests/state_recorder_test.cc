// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/inspect/component/cpp/component.h>
#include <lib/inspect/testing/cpp/inspect.h>

#include <memory>

#include <gtest/gtest.h>

#include "gmock/gmock.h"
#include "lib/power/state_recorder/cpp/enum_state_recorder.h"
#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "zircon/errors.h"

using ::inspect::testing::ChildrenMatch;
using ::inspect::testing::IntIs;
using ::inspect::testing::NameMatches;
using ::inspect::testing::NodeMatches;
using ::inspect::testing::PropertyList;
using ::inspect::testing::StringIs;
using ::inspect::testing::UintIs;
using ::testing::AllOf;
using ::testing::UnorderedElementsAre;

namespace power_observability {

class StateRecorderTest : public gtest::RealLoopFixture {
 protected:
  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    inspector_ =
        std::make_unique<inspect::ComponentInspector>(dispatcher(), inspect::PublishOptions{});
    manager_ = std::make_unique<StateRecorderManager>(*inspector_);
  }

  inspect::Hierarchy GetHierarchy() {
    fpromise::result<inspect::Hierarchy> result =
        RunPromise(inspect::ReadFromInspector(inspector_->inspector()));
    EXPECT_TRUE(result.is_ok());
    return std::move(result.value());
  }

  std::unique_ptr<inspect::ComponentInspector> inspector_;
  std::unique_ptr<StateRecorderManager> manager_;
};

enum class SwitchState : uint8_t {
  kOff = 0,
  kOn = 1,
};

static const std::map<SwitchState, std::string> kOffOn = {
    {SwitchState::kOff, "OFF"},
    {SwitchState::kOn, "ON"},
};

TEST_F(StateRecorderTest, OffOn) {
  EnumStateMetadata metadata = {
      .name = "my_switch",
      .states = kOffOn,
      .trace_category_literal = "power_test",
  };

  auto result = EnumStateRecorder<SwitchState>::Create(metadata, SwitchState::kOff, 10, *manager_);
  ASSERT_TRUE(result.is_ok());
  EnumStateRecorder recorder(std::move(result.value()));

  recorder.Record(SwitchState::kOn);
  recorder.Record(SwitchState::kOff);
  recorder.Record(SwitchState::kOn);

  auto hierarchy = GetHierarchy();
  EXPECT_THAT(hierarchy, AllOf(NodeMatches(NameMatches("root")),
                               ChildrenMatch(ElementsAre(NodeMatches(
                                   AllOf(NameMatches("power_observability_state_recorders")))))));

  auto recorders_root_hierarchy = hierarchy.GetByPath({"power_observability_state_recorders"});
  ASSERT_NE(recorders_root_hierarchy, nullptr);
  EXPECT_THAT(*recorders_root_hierarchy,
              ChildrenMatch(ElementsAre(NodeMatches(NameMatches("my_switch")))));

  auto recorder_hierarchy = recorders_root_hierarchy->GetByPath({"my_switch"});
  ASSERT_NE(recorder_hierarchy, nullptr);
  EXPECT_THAT(*recorder_hierarchy,
              AllOf(NodeMatches(NameMatches("my_switch")),
                    ChildrenMatch(UnorderedElementsAre(NodeMatches(NameMatches("metadata")),
                                                       NodeMatches(NameMatches("history"))))));

  auto metadata_hierarchy = recorder_hierarchy->GetByPath({"metadata"});
  ASSERT_NE(metadata_hierarchy, nullptr);
  EXPECT_THAT(*metadata_hierarchy,
              AllOf(NodeMatches(PropertyList(UnorderedElementsAre(StringIs("name", "my_switch"),
                                                                  StringIs("type", "enum")))),
                    ChildrenMatch(UnorderedElementsAre(NodeMatches(NameMatches("states"))))));
  auto states_hierarchy = metadata_hierarchy->GetByPath({"states"});
  ASSERT_NE(states_hierarchy, nullptr);
  EXPECT_THAT(*states_hierarchy,
              NodeMatches(PropertyList(UnorderedElementsAre(UintIs("OFF", 0), UintIs("ON", 1)))));

  auto history = recorder_hierarchy->GetByPath({"history"});
  ASSERT_NE(history, nullptr);

  EXPECT_THAT(*history, ChildrenMatch(UnorderedElementsAre(
                            NodeMatches(NameMatches("0")), NodeMatches(NameMatches("1")),
                            NodeMatches(NameMatches("2")), NodeMatches(NameMatches("3")))));

  EXPECT_THAT(*history->GetByPath({"0"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "OFF")))));
  EXPECT_THAT(*history->GetByPath({"1"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "ON")))));
  EXPECT_THAT(*history->GetByPath({"2"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "OFF")))));
  EXPECT_THAT(*history->GetByPath({"3"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "ON")))));
}

TEST_F(StateRecorderTest, CantReuseName) {
  EnumStateMetadata metadata = {
      .name = "my_switch",
      .states = kOffOn,
      .trace_category_literal = "power_test",
  };

  {
    auto result_1 =
        EnumStateRecorder<SwitchState>::Create(metadata, SwitchState::kOff, 10, *manager_);
    ASSERT_TRUE(result_1.is_ok());

    // Reusing a name results in an error.
    auto result_2 =
        EnumStateRecorder<SwitchState>::Create(metadata, SwitchState::kOff, 10, *manager_);
    ASSERT_TRUE(result_2.is_error());
    ASSERT_EQ(result_2.error_value(), ZX_ERR_ALREADY_EXISTS);
  }

  // After result_1 is dropped, the name is available for use again.
  auto result_3 =
      EnumStateRecorder<SwitchState>::Create(metadata, SwitchState::kOff, 10, *manager_);
  ASSERT_TRUE(result_3.is_ok());
}

TEST_F(StateRecorderTest, MultipleRecorders) {
  enum class EnablementState : uint8_t { kDisabled = 0, kEnabled = 1 };

  const std::map<EnablementState, std::string> kDisabledEnabled = {
      {EnablementState::kDisabled, "DISABLED"},
      {EnablementState::kEnabled, "ENABLED"},
  };

  EnumStateMetadata<SwitchState> metadata_0 = {
      .name = "switch_0",
      .states = kOffOn,
      .trace_category_literal = "power_test",
  };
  EnumStateMetadata<EnablementState> metadata_1 = {
      .name = "switch_1",
      .states = kDisabledEnabled,
      .trace_category_literal = "power_test",
  };

  auto result_0 =
      EnumStateRecorder<SwitchState>::Create(metadata_0, SwitchState::kOff, 10, *manager_);
  ASSERT_TRUE(result_0.is_ok());
  EnumStateRecorder recorder_0 = std::move(result_0.value());

  auto result_1 = EnumStateRecorder<EnablementState>::Create(metadata_1, EnablementState::kEnabled,
                                                             10, *manager_);
  ASSERT_TRUE(result_1.is_ok());
  EnumStateRecorder recorder_1 = std::move(result_1.value());

  recorder_0.Record(SwitchState::kOn);
  recorder_1.Record(EnablementState::kDisabled);

  auto hierarchy = GetHierarchy();
  EXPECT_THAT(hierarchy, AllOf(NodeMatches(NameMatches("root")),
                               ChildrenMatch(ElementsAre(NodeMatches(
                                   AllOf(NameMatches("power_observability_state_recorders")))))));

  auto recorders_root_hierarchy = hierarchy.GetByPath({"power_observability_state_recorders"});
  ASSERT_NE(recorders_root_hierarchy, nullptr);
  EXPECT_THAT(*recorders_root_hierarchy,
              ChildrenMatch(ElementsAre(NodeMatches(NameMatches("switch_0")),
                                        NodeMatches(NameMatches("switch_1")))));

  auto history_0 = recorders_root_hierarchy->GetByPath({"switch_0", "history"});
  ASSERT_NE(history_0, nullptr);
  EXPECT_THAT(*history_0, ChildrenMatch(UnorderedElementsAre(NodeMatches(NameMatches("0")),
                                                             NodeMatches(NameMatches("1")))));
  EXPECT_THAT(*history_0->GetByPath({"0"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "OFF")))));
  EXPECT_THAT(*history_0->GetByPath({"1"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "ON")))));

  auto history_1 = recorders_root_hierarchy->GetByPath({"switch_1", "history"});
  ASSERT_NE(history_1, nullptr);
  EXPECT_THAT(*history_1, ChildrenMatch(UnorderedElementsAre(NodeMatches(NameMatches("0")),
                                                             NodeMatches(NameMatches("1")))));
  EXPECT_THAT(*history_1->GetByPath({"0"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "ENABLED")))));
  EXPECT_THAT(*history_1->GetByPath({"1"}),
              NodeMatches(PropertyList(UnorderedElementsAre(IntIs("@time", testing::_),
                                                            StringIs("value", "DISABLED")))));
}

TEST_F(StateRecorderTest, ThreeStates) {
  enum class FanSpeed : uint8_t { kOff = 0, kLow = 1, kHigh = 2 };

  const std::map<FanSpeed, std::string> kFanSpeeds = {
      {FanSpeed::kOff, "OFF"},
      {FanSpeed::kLow, "LOW_SPEED"},
      {FanSpeed::kHigh, "HIGH_SPEED"},
  };
  EnumStateMetadata metadata = {
      .name = "my_fan",
      .states = kFanSpeeds,
      .trace_category_literal = "power_test",
  };

  auto result = EnumStateRecorder<FanSpeed>::Create(metadata, FanSpeed::kOff, 10, *manager_);
  ASSERT_TRUE(result.is_ok());
  EnumStateRecorder recorder(std::move(result.value()));

  recorder.Record(FanSpeed::kHigh);
  recorder.Record(FanSpeed::kLow);

  auto hierarchy = GetHierarchy();
  EXPECT_THAT(hierarchy, AllOf(NodeMatches(NameMatches("root")),
                               ChildrenMatch(ElementsAre(NodeMatches(
                                   AllOf(NameMatches("power_observability_state_recorders")))))));

  auto recorders_root_hierarchy = hierarchy.GetByPath({"power_observability_state_recorders"});
  ASSERT_NE(recorders_root_hierarchy, nullptr);
  EXPECT_THAT(*recorders_root_hierarchy,
              ChildrenMatch(ElementsAre(NodeMatches(NameMatches("my_fan")))));

  auto recorder_hierarchy = recorders_root_hierarchy->GetByPath({"my_fan"});
  ASSERT_NE(recorder_hierarchy, nullptr);
  EXPECT_THAT(*recorder_hierarchy,
              AllOf(NodeMatches(NameMatches("my_fan")),
                    ChildrenMatch(UnorderedElementsAre(NodeMatches(NameMatches("metadata")),
                                                       NodeMatches(NameMatches("history"))))));

  auto history = recorder_hierarchy->GetByPath({"history"});
  ASSERT_NE(history, nullptr);

  EXPECT_THAT(*history, ChildrenMatch(UnorderedElementsAre(NodeMatches(NameMatches("0")),
                                                           NodeMatches(NameMatches("1")),
                                                           NodeMatches(NameMatches("2")))));

  EXPECT_THAT(*history->GetByPath({"0"}),
              NodeMatches(PropertyList(
                  UnorderedElementsAre(IntIs("@time", testing::_), StringIs("value", "OFF")))));
  EXPECT_THAT(*history->GetByPath({"1"}),
              NodeMatches(PropertyList(UnorderedElementsAre(IntIs("@time", testing::_),
                                                            StringIs("value", "HIGH_SPEED")))));
  EXPECT_THAT(*history->GetByPath({"2"}),
              NodeMatches(PropertyList(UnorderedElementsAre(IntIs("@time", testing::_),
                                                            StringIs("value", "LOW_SPEED")))));
}

}  // namespace power_observability
