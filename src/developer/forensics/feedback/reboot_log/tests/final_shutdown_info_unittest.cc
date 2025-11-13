// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"

#include <fuchsia/feedback/cpp/fidl.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/config.h"
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

TEST(FinalGracefulShutdownInfoTest, GenericGraceful) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt, std::vector<GracefulShutdownReason>(), /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kGenericGraceful);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-undetermined-userspace-reboot");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-undetermined-userspace-reboot");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), std::nullopt);
}

TEST(FinalGracefulShutdownInfoTest, UnexpectedMultipleGraceful) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({
              GracefulShutdownReason::kAndroidCriticalProcessFailure,
              GracefulShutdownReason::kSessionFailure,
          }),
          /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kUnexpectedReasonGraceful);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-unexpected-reason-userspace-reboot");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), std::nullopt);
}

TEST(FinalGracefulShutdownInfoTest, SystemUpdateAndNetstackMigration) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({
              GracefulShutdownReason::kSystemUpdate,
              GracefulShutdownReason::kNetstackMigration,
          }),
          /*not_a_fdr=*/true);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSystemUpdate);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::SYSTEM_UPDATE);
}

TEST(FinalGracefulShutdownInfoTest, UserRequest) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kUserRequest}),
          /*not_a_fdr=*/true);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kUserRequest);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::USER_REQUEST);
}

TEST(FinalGracefulShutdownInfoTest, SystemUpdate) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kSystemUpdate}),
          /*not_a_fdr=*/true);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSystemUpdate);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::SYSTEM_UPDATE);
}

TEST(FinalGracefulShutdownInfoTest, HighTemperature) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kHighTemperature}),
          /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kHighTemperature);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-reboot-high-temperature");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-reboot-high-temperature");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::HIGH_TEMPERATURE);
}

TEST(FinalGracefulShutdownInfoTest, SessionFailure) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kSessionFailure}),
          /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSessionFailure);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-session-failure");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-session-failure");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::SESSION_FAILURE);
}

TEST(FinalGracefulShutdownInfoTest, SysmgrFailure) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kSysmgrFailure}),
          /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kSysmgrFailure);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-sysmgr-failure");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-sysmgr-failure");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::SYSMGR_FAILURE);
}

TEST(FinalGracefulShutdownInfoTest, CriticalComponentFailure) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kCriticalComponentFailure}),
          /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kCriticalComponentFailure);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-critical-component-failure");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-critical-component-failure");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::CRITICAL_COMPONENT_FAILURE);
}

TEST(FinalGracefulShutdownInfoTest, RetrySystemUpdate) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kRetrySystemUpdate}),
          /*not_a_fdr=*/true);

  EXPECT_TRUE(final_shutdown_info->IsCrash());
  EXPECT_TRUE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kRetrySystemUpdate);
  EXPECT_EQ(
      final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, std::nullopt),
      "fuchsia-retry-system-update");
  EXPECT_EQ(final_shutdown_info->ToCrashSignature(SpontaneousRebootReason::kSpontaneous, "unused"),
            "fuchsia-retry-system-update");
  EXPECT_EQ(final_shutdown_info->ToCrashProgramName(), "system");
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::RETRY_SYSTEM_UPDATE);
}

TEST(FinalGracefulShutdownInfoTest, ZbiSwap) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kZbiSwap}),
          /*not_a_fdr=*/true);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(), cobalt::LastRebootReason::kZbiSwap);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(), fuchsia::feedback::RebootReason::ZBI_SWAP);
}

TEST(FinalGracefulShutdownInfoTest, OOM) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kOutOfMemory}),
          /*not_a_fdr=*/true);

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

TEST(FinalGracefulShutdownInfoTest, GracefulFdr) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kFdr}),
          /*not_a_fdr=*/true);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kFactoryDataReset);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::FACTORY_DATA_RESET);
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

TEST(FinalGracefulShutdownInfoTest, NetstackMigration) {
  std::unique_ptr<FinalShutdownInfo> final_shutdown_info =
      std::make_unique<FinalGracefulShutdownInfo>(
          /*action=*/std::nullopt,
          std::vector<GracefulShutdownReason>({GracefulShutdownReason::kNetstackMigration}),
          /*not_a_fdr=*/true);

  EXPECT_FALSE(final_shutdown_info->IsCrash());
  EXPECT_FALSE(final_shutdown_info->IsFatal());
  EXPECT_EQ(final_shutdown_info->ToCobaltLastRebootReason(),
            cobalt::LastRebootReason::kNetstackMigration);
  EXPECT_EQ(final_shutdown_info->ToFidlRebootReason(),
            fuchsia::feedback::RebootReason::NETSTACK_MIGRATION);
}

}  // namespace
}  // namespace forensics::feedback
