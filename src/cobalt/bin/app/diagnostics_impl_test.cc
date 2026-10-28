// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/cobalt/bin/app/diagnostics_impl.h"

#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/testing/cpp/inspect.h>
#include <lib/syslog/cpp/macros.h>

#include <gtest/gtest.h>

#include "src/cobalt/bin/app/key_fetcher.h"
#include "src/cobalt/bin/testing/fake_clock.h"
#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "third_party/cobalt/src/lib/util/dynamic_encrypted_message_maker.h"
#include "third_party/cobalt/src/lib/util/encrypted_message_util.h"

namespace cobalt {

using inspect::testing::ChildrenMatch;
using inspect::testing::IntIs;
using inspect::testing::NameMatches;
using inspect::testing::NodeMatches;
using inspect::testing::PropertyList;
using inspect::testing::StringIs;
using inspect::testing::UintIs;
using ::testing::IsEmpty;
using ::testing::IsSupersetOf;
using ::testing::UnorderedElementsAre;

namespace {

constexpr int64_t kDefaultNowSecs = 1700000000;

util::ParsedKey MakeTestKey(uint32_t key_index, int64_t expiration_timestamp_secs) {
  return util::ParsedKey(util::EncryptedMessageMaker::MakeUnencrypted(), key_index,
                         expiration_timestamp_secs);
}

}  // namespace

class DiagnosticsTest : public gtest::TestLoopFixture {
 public:
  void SetUp() override {
    listener_ = std::make_unique<DiagnosticsImpl>(inspector_.GetRoot().CreateChild("core"));
  }

  inspect::Hierarchy InspectHierarchy() {
    fpromise::result<inspect::Hierarchy> result = inspect::ReadFromVmo(inspector_.DuplicateVmo());
    return result.take_value();
  }

 protected:
  std::unique_ptr<DiagnosticsImpl> listener_;
  inspect::Inspector inspector_;
};

TEST_F(DiagnosticsTest, SentObservationResultSuccess) {
  listener_->SentObservationResult(Status::OkStatus());
  EXPECT_THAT(InspectHierarchy(),
              AllOf(NodeMatches(NameMatches("root")),
                    ChildrenMatch(UnorderedElementsAre(AllOf(
                        NodeMatches(NameMatches("core")),
                        ChildrenMatch(Contains(AllOf(NodeMatches(AllOf(
                            NameMatches("sending_observations"),
                            PropertyList(IsSupersetOf({IntIs("successes", 1), IntIs("errors", 0),
                                                       IntIs("last_success_time", testing::Gt(0)),
                                                       IntIs("last_error_time", 0)}))))))))))));
}

TEST_F(DiagnosticsTest, SentObservationResultError) {
  listener_->SentObservationResult(
      Status(StatusCode::DEADLINE_EXCEEDED, "error_message", "error_details"));
  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(
          NodeMatches(NameMatches("root")),
          ChildrenMatch(UnorderedElementsAre(AllOf(
              NodeMatches(NameMatches("core")),
              ChildrenMatch(Contains(AllOf(NodeMatches(AllOf(
                  NameMatches("sending_observations"),
                  PropertyList(UnorderedElementsAre(
                      IntIs("successes", 0), IntIs("errors", 1), IntIs("last_success_time", 0),
                      IntIs("last_error_time", testing::Gt(0)),
                      IntIs("last_error_code", static_cast<int64_t>(StatusCode::DEADLINE_EXCEEDED)),
                      StringIs("last_error_message", "error_message"),
                      StringIs("last_error_details", "error_details")))))))))))));
}

TEST_F(DiagnosticsTest, ObservationStoreUpdatedOnce) {
  std::map<lib::ReportSpec, uint64_t> num_obs_per_report;
  num_obs_per_report[{.customer_id = 1, .project_id = 2, .metric_id = 3, .report_id = 4}] = 5;
  num_obs_per_report[{.customer_id = 6, .project_id = 7, .metric_id = 8, .report_id = 9}] = 10;
  listener_->ObservationStoreUpdated(num_obs_per_report, 1024, 2048);
  EXPECT_THAT(InspectHierarchy(),
              AllOf(NodeMatches(NameMatches("root")),
                    ChildrenMatch(UnorderedElementsAre(
                        AllOf(NodeMatches(NameMatches("core")),
                              ChildrenMatch(Contains(AllOf(NodeMatches(AllOf(
                                  NameMatches("observations_stored"),
                                  PropertyList(UnorderedElementsAre(
                                      IntIs("byte_count", 1024), IntIs("byte_count_limit", 2048),
                                      IntIs("report_1-2-3-4", 5), IntIs("report_6-7-8-9", 10),
                                      IntIs("total", 15)))))))))))));
}

TEST_F(DiagnosticsTest, ObservationStoreUpdatedMultipleTimes) {
  std::map<lib::ReportSpec, uint64_t> num_obs_per_report;
  num_obs_per_report[{.customer_id = 1, .project_id = 2, .metric_id = 3, .report_id = 4}] = 5;
  listener_->ObservationStoreUpdated(num_obs_per_report, 1024, 2048);
  num_obs_per_report[{.customer_id = 1, .project_id = 2, .metric_id = 3, .report_id = 4}] = 12;
  listener_->ObservationStoreUpdated(num_obs_per_report, 4096, 8192);
  EXPECT_THAT(InspectHierarchy(),
              AllOf(NodeMatches(NameMatches("root")),
                    ChildrenMatch(UnorderedElementsAre(
                        AllOf(NodeMatches(NameMatches("core")),
                              ChildrenMatch(Contains(AllOf(NodeMatches(AllOf(
                                  NameMatches("observations_stored"),
                                  PropertyList(UnorderedElementsAre(
                                      IntIs("byte_count", 4096), IntIs("byte_count_limit", 8192),
                                      IntIs("report_1-2-3-4", 12), IntIs("total", 12)))))))))))));
}

TEST_F(DiagnosticsTest, LoggerCalled) {
  listener_->LoggerCalled(1, "fuchsia/cobalt");
  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(Contains(AllOf(
                    NodeMatches(NameMatches("internal_metrics")),
                    ChildrenMatch(Contains(AllOf(
                        NodeMatches(AllOf(NameMatches("logger_calls"),
                                          PropertyList(UnorderedElementsAre(
                                              IntIs("total", 1),
                                              IntIs("last_successful_time", testing::Gt(0)))))),
                        ChildrenMatch(UnorderedElementsAre(
                            AllOf(NodeMatches(NameMatches("per_project")),
                                  ChildrenMatch(UnorderedElementsAre(NodeMatches(AllOf(
                                      NameMatches("fuchsia/cobalt"),
                                      PropertyList(UnorderedElementsAre(
                                          IntIs("num_calls", 1),
                                          IntIs("last_successful_time", testing::Gt(0))))))))),
                            AllOf(NodeMatches(NameMatches("per_method")),
                                  ChildrenMatch(UnorderedElementsAre(NodeMatches(
                                      AllOf(NameMatches("method_1"),
                                            PropertyList(UnorderedElementsAre(
                                                IntIs("num_calls", 1),
                                                IntIs("last_successful_time",
                                                      testing::Gt(0))))))))))))))))))))));
  listener_->LoggerCalled(2, "fuchsia/memory");
  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(
          NodeMatches(NameMatches("root")),
          ChildrenMatch(UnorderedElementsAre(AllOf(
              NodeMatches(NameMatches("core")),
              ChildrenMatch(Contains(AllOf(
                  NodeMatches(NameMatches("internal_metrics")),
                  ChildrenMatch(Contains(AllOf(
                      NodeMatches(AllOf(
                          NameMatches("logger_calls"),
                          PropertyList(UnorderedElementsAre(
                              IntIs("total", 2), IntIs("last_successful_time", testing::Gt(0)))))),
                      ChildrenMatch(UnorderedElementsAre(
                          AllOf(NodeMatches(NameMatches("per_project")),
                                ChildrenMatch(UnorderedElementsAre(
                                    NodeMatches(
                                        AllOf(NameMatches("fuchsia/cobalt"),
                                              PropertyList(UnorderedElementsAre(
                                                  IntIs("num_calls", 1),
                                                  IntIs("last_successful_time", testing::Gt(0)))))),
                                    NodeMatches(AllOf(
                                        NameMatches("fuchsia/memory"),
                                        PropertyList(UnorderedElementsAre(
                                            IntIs("num_calls", 1),
                                            IntIs("last_successful_time", testing::Gt(0))))))))),
                          AllOf(NodeMatches(NameMatches("per_method")),
                                ChildrenMatch(UnorderedElementsAre(
                                    NodeMatches(
                                        AllOf(NameMatches("method_1"),
                                              PropertyList(UnorderedElementsAre(
                                                  IntIs("num_calls", 1),
                                                  IntIs("last_successful_time", testing::Gt(0)))))),
                                    NodeMatches(AllOf(NameMatches("method_2"),
                                                      PropertyList(UnorderedElementsAre(
                                                          IntIs("num_calls", 1),
                                                          IntIs("last_successful_time",
                                                                testing::Gt(0))))))))))))))))))))));
  listener_->LoggerCalled(2, "fuchsia/cobalt");
  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(
          NodeMatches(NameMatches("root")),
          ChildrenMatch(UnorderedElementsAre(AllOf(
              NodeMatches(NameMatches("core")),
              ChildrenMatch(Contains(AllOf(
                  NodeMatches(NameMatches("internal_metrics")),
                  ChildrenMatch(Contains(AllOf(
                      NodeMatches(AllOf(
                          NameMatches("logger_calls"),
                          PropertyList(UnorderedElementsAre(
                              IntIs("total", 3), IntIs("last_successful_time", testing::Gt(0)))))),
                      ChildrenMatch(UnorderedElementsAre(
                          AllOf(NodeMatches(NameMatches("per_project")),
                                ChildrenMatch(UnorderedElementsAre(
                                    NodeMatches(
                                        AllOf(NameMatches("fuchsia/cobalt"),
                                              PropertyList(UnorderedElementsAre(
                                                  IntIs("num_calls", 2),
                                                  IntIs("last_successful_time", testing::Gt(0)))))),
                                    NodeMatches(AllOf(
                                        NameMatches("fuchsia/memory"),
                                        PropertyList(UnorderedElementsAre(
                                            IntIs("num_calls", 1),
                                            IntIs("last_successful_time", testing::Gt(0))))))))),
                          AllOf(NodeMatches(NameMatches("per_method")),
                                ChildrenMatch(UnorderedElementsAre(
                                    NodeMatches(
                                        AllOf(NameMatches("method_1"),
                                              PropertyList(UnorderedElementsAre(
                                                  IntIs("num_calls", 1),
                                                  IntIs("last_successful_time", testing::Gt(0)))))),
                                    NodeMatches(AllOf(NameMatches("method_2"),
                                                      PropertyList(UnorderedElementsAre(
                                                          IntIs("num_calls", 2),
                                                          IntIs("last_successful_time",
                                                                testing::Gt(0))))))))))))))))))))));
}

TEST_F(DiagnosticsTest, TrackDiskUsage) {
  listener_->TrackDiskUsage(1, 256, 1024);
  EXPECT_THAT(InspectHierarchy(),
              AllOf(NodeMatches(NameMatches("root")),
                    ChildrenMatch(UnorderedElementsAre(AllOf(
                        NodeMatches(NameMatches("core")),
                        ChildrenMatch(Contains(AllOf(
                            NodeMatches(NameMatches("internal_metrics")),
                            ChildrenMatch(Contains(AllOf(
                                NodeMatches(NameMatches("disk_usage")),
                                ChildrenMatch(UnorderedElementsAre(AllOf(
                                    NodeMatches(NameMatches("per_storage_class")),
                                    ChildrenMatch(UnorderedElementsAre(NodeMatches(AllOf(
                                        NameMatches("storage_class_1"),
                                        PropertyList(UnorderedElementsAre(
                                            IntIs("current_bytes", 256), IntIs("max_bytes", 256),
                                            IntIs("byte_limit", 1024)))))))))))))))))))));
  listener_->TrackDiskUsage(2, 124, -1);
  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(Contains(AllOf(
                    NodeMatches(NameMatches("internal_metrics")),
                    ChildrenMatch(Contains(AllOf(
                        NodeMatches(NameMatches("disk_usage")),
                        ChildrenMatch(UnorderedElementsAre(AllOf(
                            NodeMatches(NameMatches("per_storage_class")),
                            ChildrenMatch(UnorderedElementsAre(
                                NodeMatches(AllOf(
                                    NameMatches("storage_class_1"),
                                    PropertyList(UnorderedElementsAre(IntIs("current_bytes", 256),
                                                                      IntIs("max_bytes", 256),
                                                                      IntIs("byte_limit", 1024))))),
                                NodeMatches(
                                    AllOf(NameMatches("storage_class_2"),
                                          PropertyList(UnorderedElementsAre(
                                              IntIs("current_bytes", 124), IntIs("max_bytes", 124),
                                              IntIs("byte_limit", 0)))))))))))))))))))));
  listener_->TrackDiskUsage(1, 100, 1024);
  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(Contains(AllOf(
                    NodeMatches(NameMatches("internal_metrics")),
                    ChildrenMatch(Contains(AllOf(
                        NodeMatches(NameMatches("disk_usage")),
                        ChildrenMatch(UnorderedElementsAre(AllOf(
                            NodeMatches(NameMatches("per_storage_class")),
                            ChildrenMatch(UnorderedElementsAre(
                                NodeMatches(AllOf(
                                    NameMatches("storage_class_1"),
                                    PropertyList(UnorderedElementsAre(IntIs("current_bytes", 100),
                                                                      IntIs("max_bytes", 256),
                                                                      IntIs("byte_limit", 1024))))),
                                NodeMatches(
                                    AllOf(NameMatches("storage_class_2"),
                                          PropertyList(UnorderedElementsAre(
                                              IntIs("current_bytes", 124), IntIs("max_bytes", 124),
                                              IntIs("byte_limit", 0)))))))))))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectUninitializedMakers) {
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kNetworkError,
                                      /*consecutive_fetch_failures=*/1, kDefaultNowSecs);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(AllOf(
                    Contains(NodeMatches(AllOf(
                        NameMatches("key_rotation"),
                        PropertyList(UnorderedElementsAre(
                            StringIs("shuffler_key_state", "uninitialized"),
                            StringIs("analyzer_key_state", "uninitialized"),
                            UintIs("analyzer_key_index", 0), UintIs("shuffler_key_index", 0),
                            IntIs("expiration_time_utc", 0),
                            IntIs("last_fetch_attempt_time", kDefaultNowSecs),
                            StringIs("last_fetch_status", "NetworkError"),
                            UintIs("consecutive_fetch_failures", 1)))))),
                    Contains(AllOf(NodeMatches(NameMatches("internal_metrics")),
                                   ChildrenMatch(Contains(
                                       NodeMatches(AllOf(NameMatches("key_fetch_status"),
                                                         PropertyList(UnorderedElementsAre(UintIs(
                                                             "NetworkError", 1))))))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectPartiallyInitializedMakers) {
  constexpr uint32_t kShufflerKeyIndex = 10;
  constexpr int64_t kShufflerExpirationUtc = 1800000000;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      MakeTestKey(kShufflerKeyIndex, kShufflerExpirationUtc), &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(&clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker,
                                      KeyFetchStatus::kUninitialized,
                                      /*consecutive_fetch_failures=*/0,
                                      /*last_fetch_attempt_time_secs=*/0);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(
                AllOf(NodeMatches(NameMatches("core")),
                      ChildrenMatch(Contains(NodeMatches(AllOf(
                          NameMatches("key_rotation"),
                          PropertyList(UnorderedElementsAre(
                              StringIs("shuffler_key_state", "valid"),
                              StringIs("analyzer_key_state", "uninitialized"),
                              UintIs("analyzer_key_index", 0),
                              UintIs("shuffler_key_index", kShufflerKeyIndex),
                              IntIs("expiration_time_utc", 0), IntIs("last_fetch_attempt_time", 0),
                              StringIs("last_fetch_status", "Uninitialized"),
                              UintIs("consecutive_fetch_failures", 0))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectUninitializedShufflerInitializedAnalyzer) {
  constexpr uint32_t kAnalyzerKeyIndex = 14;
  constexpr int64_t kAnalyzerExpirationUtc = 1800000000;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(&clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      MakeTestKey(kAnalyzerKeyIndex, kAnalyzerExpirationUtc), &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker,
                                      KeyFetchStatus::kUninitialized,
                                      /*consecutive_fetch_failures=*/0,
                                      /*last_fetch_attempt_time_secs=*/0);

  EXPECT_THAT(InspectHierarchy(),
              AllOf(NodeMatches(NameMatches("root")),
                    ChildrenMatch(UnorderedElementsAre(AllOf(
                        NodeMatches(NameMatches("core")),
                        ChildrenMatch(Contains(NodeMatches(AllOf(
                            NameMatches("key_rotation"),
                            PropertyList(UnorderedElementsAre(
                                StringIs("shuffler_key_state", "uninitialized"),
                                StringIs("analyzer_key_state", "valid"),
                                UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                                UintIs("shuffler_key_index", 0), IntIs("expiration_time_utc", 0),
                                IntIs("last_fetch_attempt_time", 0),
                                StringIs("last_fetch_status", "Uninitialized"),
                                UintIs("consecutive_fetch_failures", 0))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectBundledFallbackDoesNotCreateStatusCounter) {
  constexpr uint32_t kShufflerKeyIndex = 11;
  constexpr uint32_t kAnalyzerKeyIndex = 12;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      MakeTestKey(kShufflerKeyIndex, kBundledKeyExpirationSecs), &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      MakeTestKey(kAnalyzerKeyIndex, kBundledKeyExpirationSecs), &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker,
                                      KeyFetchStatus::kUninitialized,
                                      /*consecutive_fetch_failures=*/0,
                                      /*last_fetch_attempt_time_secs=*/0);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(AllOf(
                    Contains(NodeMatches(AllOf(
                        NameMatches("key_rotation"),
                        PropertyList(UnorderedElementsAre(
                            StringIs("shuffler_key_state", "bundled_fallback"),
                            StringIs("analyzer_key_state", "bundled_fallback"),
                            UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                            UintIs("shuffler_key_index", kShufflerKeyIndex),
                            IntIs("expiration_time_utc", 0), IntIs("last_fetch_attempt_time", 0),
                            StringIs("last_fetch_status", "Uninitialized"),
                            UintIs("consecutive_fetch_failures", 0)))))),
                    Contains(AllOf(
                        NodeMatches(NameMatches("internal_metrics")),
                        ChildrenMatch(Contains(NodeMatches(AllOf(
                            NameMatches("key_fetch_status"), PropertyList(IsEmpty()))))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectOneBundledFallbackOneValidExposesDisagreement) {
  constexpr uint32_t kShufflerKeyIndex = 11;
  constexpr uint32_t kAnalyzerKeyIndex = 12;
  constexpr int64_t kAnalyzerExpirationUtc = 1800000000;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      MakeTestKey(kShufflerKeyIndex, kBundledKeyExpirationSecs), &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      MakeTestKey(kAnalyzerKeyIndex, kAnalyzerExpirationUtc), &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker,
                                      KeyFetchStatus::kUninitialized,
                                      /*consecutive_fetch_failures=*/0,
                                      /*last_fetch_attempt_time_secs=*/0);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(
                AllOf(NodeMatches(NameMatches("core")),
                      ChildrenMatch(Contains(NodeMatches(AllOf(
                          NameMatches("key_rotation"),
                          PropertyList(UnorderedElementsAre(
                              StringIs("shuffler_key_state", "bundled_fallback"),
                              StringIs("analyzer_key_state", "valid"),
                              UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                              UintIs("shuffler_key_index", kShufflerKeyIndex),
                              IntIs("expiration_time_utc", 0), IntIs("last_fetch_attempt_time", 0),
                              StringIs("last_fetch_status", "Uninitialized"),
                              UintIs("consecutive_fetch_failures", 0))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectCumulativeStatusCounterIncrement) {
  constexpr uint32_t kShufflerKeyIndex = 10;
  constexpr uint32_t kAnalyzerKeyIndex = 11;
  constexpr int64_t kShufflerExpirationUtc = 1800000000;
  constexpr int64_t kAnalyzerExpirationUtc = 1850000000;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      MakeTestKey(kShufflerKeyIndex, kShufflerExpirationUtc), &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      MakeTestKey(kAnalyzerKeyIndex, kAnalyzerExpirationUtc), &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kSuccess,
                                      /*consecutive_fetch_failures=*/0, kDefaultNowSecs);
  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kSuccess,
                                      /*consecutive_fetch_failures=*/0, kDefaultNowSecs);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(AllOf(Contains(NodeMatches(AllOf(
                                        NameMatches("key_rotation"),
                                        PropertyList(UnorderedElementsAre(
                                            StringIs("shuffler_key_state", "valid"),
                                            StringIs("analyzer_key_state", "valid"),
                                            UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                                            UintIs("shuffler_key_index", kShufflerKeyIndex),
                                            IntIs("expiration_time_utc", kShufflerExpirationUtc),
                                            IntIs("last_fetch_attempt_time", kDefaultNowSecs),
                                            StringIs("last_fetch_status", "Success"),
                                            UintIs("consecutive_fetch_failures", 0)))))),
                                    Contains(AllOf(NodeMatches(NameMatches("internal_metrics")),
                                                   ChildrenMatch(Contains(NodeMatches(AllOf(
                                                       NameMatches("key_fetch_status"),
                                                       PropertyList(UnorderedElementsAre(
                                                           UintIs("Success", 2))))))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectSingleMakerExpiredReportsExpiredAndEarlierTime) {
  constexpr uint32_t kShufflerKeyIndex = 10;
  constexpr uint32_t kAnalyzerKeyIndex = 11;
  constexpr int64_t kShufflerValidExpirationUtc = 1800000000;
  constexpr int64_t kAnalyzerExpiredUtc = 1600000000;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(
      MakeTestKey(kShufflerKeyIndex, kShufflerValidExpirationUtc), &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(
      MakeTestKey(kAnalyzerKeyIndex, kAnalyzerExpiredUtc), &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kExpiredKey,
                                      /*consecutive_fetch_failures=*/1, kDefaultNowSecs);

  EXPECT_THAT(InspectHierarchy(),
              AllOf(NodeMatches(NameMatches("root")),
                    ChildrenMatch(UnorderedElementsAre(
                        AllOf(NodeMatches(NameMatches("core")),
                              ChildrenMatch(Contains(NodeMatches(
                                  AllOf(NameMatches("key_rotation"),
                                        PropertyList(UnorderedElementsAre(
                                            StringIs("shuffler_key_state", "valid"),
                                            StringIs("analyzer_key_state", "expired"),
                                            UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                                            UintIs("shuffler_key_index", kShufflerKeyIndex),
                                            IntIs("expiration_time_utc", kAnalyzerExpiredUtc),
                                            IntIs("last_fetch_attempt_time", kDefaultNowSecs),
                                            StringIs("last_fetch_status", "ExpiredKey"),
                                            UintIs("consecutive_fetch_failures", 1))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectExpiredKeys) {
  constexpr uint32_t kShufflerKeyIndex = 10;
  constexpr uint32_t kAnalyzerKeyIndex = 11;
  constexpr int64_t kExpirationUtc = 1600000000;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(MakeTestKey(kShufflerKeyIndex, kExpirationUtc),
                                                    &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(MakeTestKey(kAnalyzerKeyIndex, kExpirationUtc),
                                                    &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kExpiredKey,
                                      /*consecutive_fetch_failures=*/3, kDefaultNowSecs);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(
          NodeMatches(NameMatches("root")),
          ChildrenMatch(UnorderedElementsAre(AllOf(
              NodeMatches(NameMatches("core")),
              ChildrenMatch(AllOf(
                  Contains(NodeMatches(AllOf(NameMatches("key_rotation"),
                                             PropertyList(UnorderedElementsAre(
                                                 StringIs("shuffler_key_state", "expired"),
                                                 StringIs("analyzer_key_state", "expired"),
                                                 UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                                                 UintIs("shuffler_key_index", kShufflerKeyIndex),
                                                 IntIs("expiration_time_utc", kExpirationUtc),
                                                 IntIs("last_fetch_attempt_time", kDefaultNowSecs),
                                                 StringIs("last_fetch_status", "ExpiredKey"),
                                                 UintIs("consecutive_fetch_failures", 3)))))),
                  Contains(AllOf(
                      NodeMatches(NameMatches("internal_metrics")),
                      ChildrenMatch(Contains(NodeMatches(AllOf(
                          NameMatches("key_fetch_status"),
                          PropertyList(UnorderedElementsAre(UintIs("ExpiredKey", 1))))))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectMultipleStatusTransitions) {
  constexpr uint32_t kShufflerKeyIndex = 10;
  constexpr uint32_t kAnalyzerKeyIndex = 11;
  constexpr int64_t kExpirationUtc = 1800000000;
  constexpr int64_t kSecondAttemptUtc = 1700000600;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(MakeTestKey(kShufflerKeyIndex, kExpirationUtc),
                                                    &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(MakeTestKey(kAnalyzerKeyIndex, kExpirationUtc),
                                                    &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kNetworkError,
                                      /*consecutive_fetch_failures=*/2, kDefaultNowSecs);
  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kSuccess,
                                      /*consecutive_fetch_failures=*/0, kSecondAttemptUtc);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(AllOf(Contains(NodeMatches(AllOf(
                                        NameMatches("key_rotation"),
                                        PropertyList(UnorderedElementsAre(
                                            StringIs("shuffler_key_state", "valid"),
                                            StringIs("analyzer_key_state", "valid"),
                                            UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                                            UintIs("shuffler_key_index", kShufflerKeyIndex),
                                            IntIs("expiration_time_utc", kExpirationUtc),
                                            IntIs("last_fetch_attempt_time", kSecondAttemptUtc),
                                            StringIs("last_fetch_status", "Success"),
                                            UintIs("consecutive_fetch_failures", 0)))))),
                                    Contains(AllOf(NodeMatches(NameMatches("internal_metrics")),
                                                   ChildrenMatch(Contains(NodeMatches(AllOf(
                                                       NameMatches("key_fetch_status"),
                                                       PropertyList(UnorderedElementsAre(
                                                           UintIs("NetworkError", 1),
                                                           UintIs("Success", 1))))))))))))))));
}

TEST_F(DiagnosticsTest, UpdateKeyRotationInspectRefreshAfterClockAdvanceReportsExpired) {
  constexpr uint32_t kShufflerKeyIndex = 10;
  constexpr uint32_t kAnalyzerKeyIndex = 11;
  constexpr int64_t kExpirationUtc = 1800000000;
  constexpr int64_t kAfterExpirationUtc = 1900000000;
  FakeSystemClock clock(kDefaultNowSecs);
  util::DynamicEncryptedMessageMaker shuffler_maker(MakeTestKey(kShufflerKeyIndex, kExpirationUtc),
                                                    &clock);
  util::DynamicEncryptedMessageMaker analyzer_maker(MakeTestKey(kAnalyzerKeyIndex, kExpirationUtc),
                                                    &clock);

  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker,
                                      KeyFetchStatus::kUninitialized,
                                      /*consecutive_fetch_failures=*/0,
                                      /*last_fetch_attempt_time_secs=*/0);

  clock.SetNowSecs(kAfterExpirationUtc);
  listener_->UpdateKeyRotationInspect(shuffler_maker, analyzer_maker, KeyFetchStatus::kNetworkError,
                                      /*consecutive_fetch_failures=*/1, kAfterExpirationUtc);

  EXPECT_THAT(
      InspectHierarchy(),
      AllOf(NodeMatches(NameMatches("root")),
            ChildrenMatch(UnorderedElementsAre(AllOf(
                NodeMatches(NameMatches("core")),
                ChildrenMatch(AllOf(Contains(NodeMatches(AllOf(
                                        NameMatches("key_rotation"),
                                        PropertyList(UnorderedElementsAre(
                                            StringIs("shuffler_key_state", "expired"),
                                            StringIs("analyzer_key_state", "expired"),
                                            UintIs("analyzer_key_index", kAnalyzerKeyIndex),
                                            UintIs("shuffler_key_index", kShufflerKeyIndex),
                                            IntIs("expiration_time_utc", kExpirationUtc),
                                            IntIs("last_fetch_attempt_time", kAfterExpirationUtc),
                                            StringIs("last_fetch_status", "NetworkError"),
                                            UintIs("consecutive_fetch_failures", 1)))))),
                                    Contains(AllOf(NodeMatches(NameMatches("internal_metrics")),
                                                   ChildrenMatch(Contains(NodeMatches(AllOf(
                                                       NameMatches("key_fetch_status"),
                                                       PropertyList(UnorderedElementsAre(
                                                           UintIs("NetworkError", 1))))))))))))))));
}

}  // namespace cobalt
