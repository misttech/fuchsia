// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

#include <fuchsia/feedback/cpp/fidl.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/config.h"
#include "src/developer/forensics/testing/gpretty_printers.h"  // IWYU pragma: keep
#include "src/developer/forensics/utils/cobalt/metrics.h"

namespace forensics::feedback {
namespace {

TEST(FinalZirconShutdownInfoTest, NotParseable) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kNotParseable);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), cobalt::LastRebootReason::kUnknown);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-reboot-log-not-parseable");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-reboot-log-not-parseable");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "reboot-log");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), std::nullopt);
}

TEST(FinalZirconShutdownInfoTest, Cold) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kCold);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), cobalt::LastRebootReason::kCold);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), fuchsia::feedback::RebootReason::COLD);
}

TEST(FinalZirconShutdownInfoTest, Spontaneous) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kUnknown);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kBriefPowerLoss);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-spontaneous-reboot");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-spontaneous-reboot");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::BRIEF_POWER_LOSS);
}

TEST(FinalZirconShutdownInfoTest, BriefPowerLoss) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kUnknown);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kBriefPowerLoss);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kBriefPowerLoss, std::nullopt),
      "fuchsia-brief-power-loss");
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kBriefPowerLoss, "unused"),
      "fuchsia-brief-power-loss");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::BRIEF_POWER_LOSS);
}

TEST(FinalZirconShutdownInfoTest, HardReset) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kUnknown);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kBriefPowerLoss);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kHardReset, std::nullopt),
      "fuchsia-hard-reset");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kHardReset, "unused"),
            "fuchsia-hard-reset");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::BRIEF_POWER_LOSS);
}

TEST(FinalZirconShutdownInfoTest, KernelPanic) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kKernelPanic);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kKernelPanic);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-kernel-panic");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-kernel-panic");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "kernel");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::KERNEL_PANIC);
}

TEST(FinalZirconShutdownInfoTest, OOM) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kOOM);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSystemOutOfMemory);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-oom");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-oom");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::SYSTEM_OUT_OF_MEMORY);
}

TEST(FinalZirconShutdownInfoTest, HardwareWatchdogTimeout) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kHwWatchdog);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kHardwareWatchdogTimeout);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-hw-watchdog-timeout");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-hw-watchdog-timeout");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::HARDWARE_WATCHDOG_TIMEOUT);
}

TEST(FinalZirconShutdownInfoTest, SoftwareWatchdogTimeout) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kSwWatchdog);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSoftwareWatchdogTimeout);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-sw-watchdog-timeout");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-sw-watchdog-timeout");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::SOFTWARE_WATCHDOG_TIMEOUT);
}

TEST(FinalZirconShutdownInfoTest, Brownout) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kBrownout);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), cobalt::LastRebootReason::kBrownout);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-brownout");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-brownout");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "device");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), fuchsia::feedback::RebootReason::BROWNOUT);
}

TEST(FinalZirconShutdownInfoTest, RootJobTermination) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalZirconShutdownInfo>(ZirconRebootReason::kRootJobTermination);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kRootJobTermination);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-root-job-termination");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous,
                                                  "critical_process"),
            "fuchsia-reboot-critical_process-terminated");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::ROOT_JOB_TERMINATION);
}

TEST(FinalZirconShutdownInfoDeathTest, NoCrash) {
  ASSERT_DEATH((FinalZirconShutdownInfo(ZirconRebootReason::kNoCrash)), "");
}

TEST(FinalZirconShutdownInfoDeathTest, NotSet) {
  ASSERT_DEATH((FinalZirconShutdownInfo(ZirconRebootReason::kNotSet)), "");
}

struct GracefulNoReportTestParams {
  std::string test_name;
  std::vector<GracefulShutdownReason> reasons;
  cobalt::LastRebootReason expected_cobalt_reason;
  fuchsia::feedback::RebootReason expected_fidl_reboot_reason;
};

class FinalGracefulShutdownInfoNoReportTest
    : public testing::TestWithParam<GracefulNoReportTestParams> {};

INSTANTIATE_TEST_SUITE_P(WithVariousReasons, FinalGracefulShutdownInfoNoReportTest,
                         ::testing::ValuesIn(std::vector<GracefulNoReportTestParams>({
                             {
                                 "SystemUpdateAndNetstackMigration",
                                 {
                                     GracefulShutdownReason::kSystemUpdate,
                                     GracefulShutdownReason::kNetstackMigration,
                                 },
                                 cobalt::LastRebootReason::kSystemUpdate,
                                 fuchsia::feedback::RebootReason::SYSTEM_UPDATE,
                             },
                             {
                                 "UserRequest",
                                 {GracefulShutdownReason::kUserRequest},
                                 cobalt::LastRebootReason::kUserRequest,
                                 fuchsia::feedback::RebootReason::USER_REQUEST,
                             },
                             {
                                 "SystemUpdate",
                                 {GracefulShutdownReason::kSystemUpdate},
                                 cobalt::LastRebootReason::kSystemUpdate,
                                 fuchsia::feedback::RebootReason::SYSTEM_UPDATE,
                             },
                             {
                                 "ZbiSwap",
                                 {GracefulShutdownReason::kZbiSwap},
                                 cobalt::LastRebootReason::kZbiSwap,
                                 fuchsia::feedback::RebootReason::ZBI_SWAP,
                             },
                             {
                                 "GracefulFdr",
                                 {GracefulShutdownReason::kFdr},
                                 cobalt::LastRebootReason::kFactoryDataReset,
                                 fuchsia::feedback::RebootReason::FACTORY_DATA_RESET,
                             },
                             {
                                 "NetstackMigration",
                                 {GracefulShutdownReason::kNetstackMigration},
                                 cobalt::LastRebootReason::kNetstackMigration,
                                 fuchsia::feedback::RebootReason::NETSTACK_MIGRATION,
                             },
                         })),
                         [](const testing::TestParamInfo<GracefulNoReportTestParams>& info) {
                           return info.param.test_name;
                         });

TEST_P(FinalGracefulShutdownInfoNoReportTest, CheckProperties) {
  const GracefulNoReportTestParams& params = GetParam();
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt, params.reasons, /*not_a_fdr=*/true);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), params.expected_cobalt_reason);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), params.expected_fidl_reboot_reason);
}

struct GracefulTestParams {
  std::string test_name;
  std::vector<GracefulShutdownReason> reasons;
  bool is_fatal;
  cobalt::LastRebootReason expected_cobalt_reason;
  std::optional<fuchsia::feedback::RebootReason> expected_fidl_reboot_reason;
  std::string expected_crash_signature;
  std::string expected_crash_program_name;
};

class FinalGracefulShutdownInfoTest : public testing::TestWithParam<GracefulTestParams> {};

INSTANTIATE_TEST_SUITE_P(WithVariousReasons, FinalGracefulShutdownInfoTest,
                         ::testing::ValuesIn(std::vector<GracefulTestParams>(
                             {{
                                  "GenericGraceful",
                                  /*reasons=*/{},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kGenericGraceful,
                                  /*expected_fidl_reboot_reason=*/std::nullopt,
                                  "fuchsia-shutdown-undetermined-userspace-reason",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "UnexpectedMultipleGraceful",
                                  {
                                      GracefulShutdownReason::kAndroidCriticalProcessFailure,
                                      GracefulShutdownReason::kSessionFailure,
                                  },
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kUnexpectedReasonGraceful,
                                  /*expected_fidl_reboot_reason=*/std::nullopt,
                                  "fuchsia-shutdown-unexpected-userspace-reason",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "HighTemperature",
                                  {GracefulShutdownReason::kHighTemperature},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kHighTemperature,
                                  fuchsia::feedback::RebootReason::HIGH_TEMPERATURE,
                                  "fuchsia-shutdown-high-temperature",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "SessionFailure",
                                  {GracefulShutdownReason::kSessionFailure},
                                  /*is_fatal=*/false,
                                  cobalt::LastRebootReason::kSessionFailure,
                                  fuchsia::feedback::RebootReason::SESSION_FAILURE,
                                  "fuchsia-session-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "SysmgrFailure",
                                  {GracefulShutdownReason::kSysmgrFailure},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kSysmgrFailure,
                                  fuchsia::feedback::RebootReason::SYSMGR_FAILURE,
                                  "fuchsia-sysmgr-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "CriticalComponentFailure",
                                  {GracefulShutdownReason::kCriticalComponentFailure},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kCriticalComponentFailure,
                                  fuchsia::feedback::RebootReason::CRITICAL_COMPONENT_FAILURE,
                                  "fuchsia-critical-component-failure",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "RetrySystemUpdate",
                                  {GracefulShutdownReason::kRetrySystemUpdate},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kRetrySystemUpdate,
                                  fuchsia::feedback::RebootReason::RETRY_SYSTEM_UPDATE,
                                  "fuchsia-retry-system-update",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "OOM",
                                  {GracefulShutdownReason::kOutOfMemory},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kSystemOutOfMemory,
                                  fuchsia::feedback::RebootReason::SYSTEM_OUT_OF_MEMORY,
                                  "fuchsia-oom",
                                  /*expected_crash_program_name=*/"system",
                              },
                              {
                                  "AndroidUnexpectedReason",
                                  {GracefulShutdownReason::kAndroidUnexpectedReason},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kAndroidUnexpectedReason,
                                  fuchsia::feedback::RebootReason::ANDROID_UNEXPECTED_REASON,
                                  "fuchsia-shutdown-android-unexpected-reason",
                                  /*expected_crash_program_name=*/"android",
                              },
                              {
                                  "AndroidRescueParty",
                                  {GracefulShutdownReason::kAndroidRescueParty},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kAndroidRescueParty,
                                  fuchsia::feedback::RebootReason::ANDROID_RESCUE_PARTY,
                                  "fuchsia-shutdown-android-rescue-party",
                                  /*expected_crash_program_name=*/"android",
                              },
                              {
                                  "AndroidCriticalProcessFailure",
                                  {GracefulShutdownReason::kAndroidCriticalProcessFailure},
                                  /*is_fatal=*/true,
                                  cobalt::LastRebootReason::kAndroidCriticalProcessFailure,
                                  fuchsia::feedback::RebootReason::ANDROID_CRITICAL_PROCESS_FAILURE,
                                  "fuchsia-shutdown-android-critical-process-failure",
                                  /*expected_crash_program_name=*/"android",
                              }})),
                         [](const testing::TestParamInfo<GracefulTestParams>& info) {
                           return info.param.test_name;
                         });

TEST_P(FinalGracefulShutdownInfoTest, CheckProperties) {
  const GracefulTestParams& params = GetParam();
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt, params.reasons, /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_EQ(final_shutdown_info->IsFatal(), params.is_fatal);
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), params.expected_cobalt_reason);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), params.expected_fidl_reboot_reason);

  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      params.expected_crash_signature);
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            params.expected_crash_signature);

  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), params.expected_crash_program_name);
}

TEST(FinalGracefulShutdownInfoTest, InferredFdr) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt, std::vector<GracefulShutdownReason>(),
          /*not_a_fdr=*/false);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kFactoryDataReset);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::FACTORY_DATA_RESET);
}

}  // namespace
}  // namespace forensics::feedback
