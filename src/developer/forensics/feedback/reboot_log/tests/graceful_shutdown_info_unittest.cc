// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/graceful_shutdown_info.h"

#include <format>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/testing/gpretty_printers.h"  // IWYU pragma: keep
#include "src/developer/forensics/testing/stubs/cobalt_logger_factory.h"
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/files/file.h"
#include "src/lib/files/path.h"
#include "src/lib/files/scoped_temp_dir.h"
#include "src/lib/timekeeper/test_clock.h"

namespace forensics {
namespace feedback {
namespace {

constexpr char kFilename[] = "graceful_shutdown_info.json";

std::string BuildJsonWithAction(const std::string_view action) {
  // Use {{ and }} to escape the curly braces for std::format.
  return std::format(
      R"({{
    "action": "{}",
    "reasons": []
}})",
      action);
}

TEST(GracefulShutdownInfoTest, VerifyLegacyContentConversion) {
  // ToLegacyFileContentForTesting() & FromLegacyTxtFile() for shutdown reasons from
  // |power::statecontrol::ShutdownReason| should be reversible.

  const std::vector<GracefulShutdownReason> reasons = {
      GracefulShutdownReason::kUserRequest,
      GracefulShutdownReason::kSystemUpdate,
      GracefulShutdownReason::kRetrySystemUpdate,
      GracefulShutdownReason::kHighTemperature,
      GracefulShutdownReason::kSessionFailure,
      GracefulShutdownReason::kSysmgrFailure,
      GracefulShutdownReason::kCriticalComponentFailure,
      GracefulShutdownReason::kFdr,
      GracefulShutdownReason::kZbiSwap,
      GracefulShutdownReason::kNotSupported,
      GracefulShutdownReason::kNetstackMigration,
  };

  for (const auto reason : reasons) {
    EXPECT_THAT(FromLegacyTxtFile(ToLegacyFileContentForTesting({reason})),
                testing::ElementsAre(reason));
  }
}

TEST(GracefulShutdownInfoTest, VerifyLegacyContentConversionWithMultipleReasons) {
  // ToLegacyFileContentForTesting() & FromLegacyTxtFile() for shutdown reasons from
  // |power::statecontrol::ShutdownReason| should be reversible when there are multiple reasons.

  const std::vector<GracefulShutdownReason> reasons = {
      GracefulShutdownReason::kUserRequest,
      GracefulShutdownReason::kSystemUpdate,
      GracefulShutdownReason::kRetrySystemUpdate,
      GracefulShutdownReason::kHighTemperature,
      GracefulShutdownReason::kSessionFailure,
      GracefulShutdownReason::kSysmgrFailure,
      GracefulShutdownReason::kCriticalComponentFailure,
      GracefulShutdownReason::kFdr,
      GracefulShutdownReason::kZbiSwap,
      GracefulShutdownReason::kNotSupported,
      GracefulShutdownReason::kNetstackMigration,
  };

  // Verify all reasons at once.
  EXPECT_THAT(FromLegacyTxtFile(ToLegacyFileContentForTesting(reasons)),
              testing::ElementsAreArray(reasons));
}

TEST(GracefulShutdownInfoTest, VerifyLegacyContentConversionWithNoReasons) {
  // ToLegacyFileContentForTesting() & FromLegacyTxtFile() for shutdown reasons from
  // |power::statecontrol::ShutdownReason| should be reversible when there are no reasons.
  EXPECT_TRUE(FromLegacyTxtFile(ToLegacyFileContentForTesting({})).empty());
}

TEST(GracefulShutdownInfoTest, VerifyContentConversion) {
  // ToJson() & FromJson() for shutdown reasons from
  // |power::statecontrol::ShutdownReason| should be reversible.
  const std::vector<GracefulShutdownReason> reasons = {
      GracefulShutdownReason::kUserRequest,
      GracefulShutdownReason::kSystemUpdate,
      GracefulShutdownReason::kRetrySystemUpdate,
      GracefulShutdownReason::kHighTemperature,
      GracefulShutdownReason::kSessionFailure,
      GracefulShutdownReason::kSysmgrFailure,
      GracefulShutdownReason::kCriticalComponentFailure,
      GracefulShutdownReason::kFdr,
      GracefulShutdownReason::kZbiSwap,
      GracefulShutdownReason::kNotSupported,
      GracefulShutdownReason::kNetstackMigration,
  };

  for (const auto reason : reasons) {
    EXPECT_THAT(FromJson(ToJson({reason})).reasons, testing::ElementsAre(reason));
  }
}

TEST(GracefulShutdownInfoTest, VerifyContentConversionWithMultipleReasons) {
  // ToJson() & FromJson() for shutdown reasons from
  // |power::statecontrol::ShutdownReason| should be reversible when there are multiple reasons.
  const std::vector<GracefulShutdownReason> reasons = {
      GracefulShutdownReason::kUserRequest,
      GracefulShutdownReason::kSystemUpdate,
      GracefulShutdownReason::kRetrySystemUpdate,
      GracefulShutdownReason::kHighTemperature,
      GracefulShutdownReason::kSessionFailure,
      GracefulShutdownReason::kSysmgrFailure,
      GracefulShutdownReason::kCriticalComponentFailure,
      GracefulShutdownReason::kFdr,
      GracefulShutdownReason::kZbiSwap,
      GracefulShutdownReason::kNotSupported,
      GracefulShutdownReason::kNetstackMigration,
  };

  // Verify all reasons at once.
  EXPECT_THAT(FromJson(ToJson(reasons)).reasons, testing::ElementsAreArray(reasons));
}

struct ReadActionTestParam {
  std::string test_name;
  std::string input_action;
  GracefulShutdownAction expected_action;
};

class ReadActionTest : public UnitTestFixture,
                       public testing::WithParamInterface<ReadActionTestParam> {};

INSTANTIATE_TEST_SUITE_P(WithVariousShutdownActions, ReadActionTest,
                         ::testing::ValuesIn(std::vector<ReadActionTestParam>({
                             {
                                 "Poweroff",
                                 "POWEROFF",
                                 GracefulShutdownAction::kPoweroff,
                             },
                             {
                                 "Reboot",
                                 "REBOOT",
                                 GracefulShutdownAction::kReboot,
                             },
                             {
                                 "RebootToRecovery",
                                 "REBOOT_TO_RECOVERY",
                                 GracefulShutdownAction::kRebootToRecovery,
                             },
                             {
                                 "RebootToBootloader",
                                 "REBOOT_TO_BOOTLOADER",
                                 GracefulShutdownAction::kRebootToBootloader,
                             },
                             {
                                 "NotParseable",
                                 "SOMETHING_INVALID",
                                 GracefulShutdownAction::kNotParseable,
                             },
                         })),
                         [](const testing::TestParamInfo<ReadActionTestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(ReadActionTest, FromJson) {
  const ReadActionTestParam& param = GetParam();
  EXPECT_THAT(FromJson(BuildJsonWithAction(param.input_action)).action, param.expected_action);
}

TEST(GracefulShutdownInfoTest, VerifyContentConversionWithNoReasons) {
  // ToJson() & FromJson() for shutdown reasons from
  // |power::statecontrol::ShutdownReason| should be reversible when there are no reasons.
  EXPECT_TRUE(FromJson(ToJson({})).reasons.empty());
}

TEST(GracefulShutdownInfoTest, ActionIsNotAString) {
  EXPECT_EQ(FromJson(R"({ "action": [], "reasons" : [] })"),
            (GracefulShutdownInfo{
                .action = GracefulShutdownAction::kNotParseable,
                .reasons = {GracefulShutdownReason::kNotParseable},
            }));
}

TEST(GracefulShutdownInfoTest, ReasonsIsNotAnArray) {
  EXPECT_EQ(FromJson(R"({ "reasons" : "not-an-array" })"),
            (GracefulShutdownInfo{
                .action = GracefulShutdownAction::kNotParseable,
                .reasons = {GracefulShutdownReason::kNotParseable},
            }));
}

TEST(GracefulShutdownInfoTest, SpuriousField) {
  EXPECT_EQ(FromJson(R"({ "reasons" : [], "spurious_field": "spurious-value" })"),
            (GracefulShutdownInfo{
                .action = GracefulShutdownAction::kNotParseable,
                .reasons = {GracefulShutdownReason::kNotParseable},
            }));
}

struct TestParam {
  std::string test_name;
  GracefulShutdownReason input_shutdown_reason;
  std::string output_reason;
};

class WriteGracefulShutdownInfoTest : public UnitTestFixture,
                                      public testing::WithParamInterface<TestParam> {
 public:
  WriteGracefulShutdownInfoTest() : cobalt_(dispatcher(), services(), &clock_) {}

 protected:
  std::string Path() { return files::JoinPath(tmp_dir_.path(), kFilename); }

  static std::string BuildJson(const std::string& reason) {
    // Use {{ and }} to escape the curly braces for std::format.
    return std::format(
        R"({{
    "reasons": [
        "{}"
    ]
}})",
        reason);
  }

  timekeeper::TestClock clock_;
  cobalt::Logger cobalt_;

 private:
  files::ScopedTempDir tmp_dir_;
};

INSTANTIATE_TEST_SUITE_P(WithVariousShutdownReasons, WriteGracefulShutdownInfoTest,
                         ::testing::ValuesIn(std::vector<TestParam>({
                             {
                                 "UserRequest",
                                 GracefulShutdownReason::kUserRequest,
                                 "USER REQUEST",
                             },
                             {
                                 "SystemUpdate",
                                 GracefulShutdownReason::kSystemUpdate,
                                 "SYSTEM UPDATE",
                             },
                             {
                                 "RetrySystemUpdate",
                                 GracefulShutdownReason::kRetrySystemUpdate,
                                 "RETRY SYSTEM UPDATE",
                             },
                             {
                                 "HighTemperature",
                                 GracefulShutdownReason::kHighTemperature,
                                 "HIGH TEMPERATURE",
                             },
                             {
                                 "SessionFailure",
                                 GracefulShutdownReason::kSessionFailure,
                                 "SESSION FAILURE",
                             },
                             {
                                 "SystemFailure",
                                 GracefulShutdownReason::kSysmgrFailure,
                                 "SYSMGR FAILURE",
                             },
                             {
                                 "CriticalComponentFailure",
                                 GracefulShutdownReason::kCriticalComponentFailure,
                                 "CRITICAL COMPONENT FAILURE",
                             },
                             {
                                 "FactoryDataReset",
                                 GracefulShutdownReason::kFdr,
                                 "FACTORY DATA RESET",
                             },
                             {
                                 "ZbiSwap",
                                 GracefulShutdownReason::kZbiSwap,
                                 "ZBI SWAP",
                             },
                             {
                                 "OutOfMemory",
                                 GracefulShutdownReason::kOutOfMemory,
                                 "OUT OF MEMORY",
                             },
                             {
                                 "NetstackMigration",
                                 GracefulShutdownReason::kNetstackMigration,
                                 "NETSTACK MIGRATION",
                             },
                             {
                                 "NotSupported",
                                 static_cast<GracefulShutdownReason>(100u),
                                 "NOT SUPPORTED",
                             },
                         })),
                         [](const testing::TestParamInfo<TestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(WriteGracefulShutdownInfoTest, Succeed) {
  const auto param = GetParam();

  SetUpCobaltServer(std::make_unique<stubs::CobaltLoggerFactory>());

  WriteGracefulShutdownInfo({param.input_shutdown_reason}, &cobalt_, Path());

  std::string contents;
  ASSERT_TRUE(files::ReadFileToString(Path(), &contents));
  EXPECT_EQ(contents, BuildJson(param.output_reason));

  RunLoopUntilIdle();
}

}  // namespace
}  // namespace feedback
}  // namespace forensics
