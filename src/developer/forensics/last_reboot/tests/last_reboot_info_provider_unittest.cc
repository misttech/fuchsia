// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/last_reboot/last_reboot_info_provider.h"

#include <optional>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/reboot_log/final_shutdown_info.h"
#include "src/developer/forensics/feedback/reboot_log/reboot_log.h"
#include "src/developer/forensics/testing/gpretty_printers.h"  // IWYU pragma: keep

namespace forensics {
namespace last_reboot {
namespace {

fuchsia::feedback::LastReboot GetLastRebootGraceful(
    const std::vector<feedback::GracefulShutdownReason>& reboot_reasons,
    const std::optional<zx::duration> uptime = std::nullopt,
    const std::optional<zx::duration> runtime = std::nullopt) {
  auto final_shutdown_info = std::make_unique<feedback::FinalGracefulShutdownInfo>(
      feedback::GracefulShutdownAction::kReboot, reboot_reasons,
      /*not_a_fdr=*/true);
  const feedback::RebootLog reboot_log(std::move(final_shutdown_info), "",
                                       /*dlog=*/std::nullopt, uptime, runtime,
                                       /*critical_process=*/std::nullopt);

  fuchsia::feedback::LastReboot out_last_reboot;

  LastRebootInfoProvider last_reboot_info_provider(reboot_log);
  last_reboot_info_provider.Get(
      [&](fuchsia::feedback::LastReboot last_reboot) { out_last_reboot = std::move(last_reboot); });

  return out_last_reboot;
}

fuchsia::feedback::LastReboot GetLastRebootUnGraceful(
    const feedback::ZirconRebootReason reboot_reason,
    const std::optional<zx::duration> uptime = std::nullopt,
    const std::optional<zx::duration> runtime = std::nullopt) {
  auto final_shutdown_info = std::make_unique<feedback::FinalZirconShutdownInfo>(reboot_reason);
  const feedback::RebootLog reboot_log(std::move(final_shutdown_info), "",
                                       /*dlog=*/std::nullopt, uptime, runtime,
                                       /*critical_process=*/std::nullopt);

  fuchsia::feedback::LastReboot out_last_reboot;

  LastRebootInfoProvider last_reboot_info_provider(reboot_log);
  last_reboot_info_provider.Get(
      [&](fuchsia::feedback::LastReboot last_reboot) { out_last_reboot = std::move(last_reboot); });

  return out_last_reboot;
}

TEST(LastRebootInfoProviderTest, Succeed_Graceful) {
  const auto last_reboot = GetLastRebootGraceful({});

  ASSERT_TRUE(last_reboot.has_graceful());
  EXPECT_TRUE(last_reboot.graceful());

  EXPECT_FALSE(last_reboot.has_reason());
}

TEST(LastRebootInfoProviderTest, Succeed_NotGraceful) {
  const auto last_reboot = GetLastRebootUnGraceful(feedback::ZirconRebootReason::kKernelPanic);

  ASSERT_TRUE(last_reboot.has_graceful());
  EXPECT_FALSE(last_reboot.graceful());

  ASSERT_TRUE(last_reboot.has_reason());
  EXPECT_EQ(last_reboot.reason(), ::fuchsia::feedback::RebootReason::KERNEL_PANIC);
}

TEST(LastRebootInfoProviderTest, Succeed_Planned) {
  const auto last_reboot = GetLastRebootGraceful({feedback::GracefulShutdownReason::kSystemUpdate});

  ASSERT_TRUE(last_reboot.has_planned());
  EXPECT_TRUE(last_reboot.planned());

  ASSERT_TRUE(last_reboot.has_reason());
  EXPECT_EQ(last_reboot.reason(), ::fuchsia::feedback::RebootReason::SYSTEM_UPDATE);
}

TEST(LastRebootInfoProviderTest, Succeed_NotPlanned) {
  const auto last_reboot = GetLastRebootGraceful({feedback::GracefulShutdownReason::kUserRequest});

  ASSERT_TRUE(last_reboot.has_planned());
  EXPECT_FALSE(last_reboot.planned());

  ASSERT_TRUE(last_reboot.has_reason());
  EXPECT_EQ(last_reboot.reason(), ::fuchsia::feedback::RebootReason::USER_REQUEST);
}

TEST(LastRebootInfoProviderTest, Succeed_HasUptime) {
  const zx::duration uptime = zx::msec(100);

  const auto last_reboot =
      GetLastRebootGraceful({feedback::GracefulShutdownReason::kUserRequest}, uptime);

  ASSERT_TRUE(last_reboot.has_uptime());
  EXPECT_EQ(last_reboot.uptime(), uptime.to_nsecs());
}

TEST(LastRebootInfoProviderTest, Succeed_DoesNotHaveUptime) {
  const auto last_reboot = GetLastRebootGraceful({feedback::GracefulShutdownReason::kUserRequest},
                                                 /*uptime=*/std::nullopt);

  EXPECT_FALSE(last_reboot.has_uptime());
}

TEST(LastRebootInfoProviderTest, Succeed_HasRuntime) {
  const zx::duration runtime = zx::msec(78);

  const auto last_reboot = GetLastRebootGraceful({feedback::GracefulShutdownReason::kUserRequest},
                                                 /*uptime=*/std::nullopt, runtime);

  ASSERT_TRUE(last_reboot.has_runtime());
  EXPECT_EQ(last_reboot.runtime(), runtime.to_nsecs());
}

TEST(LastRebootInfoProviderTest, Succeed_DoesNotHaveRuntime) {
  const auto last_reboot = GetLastRebootGraceful({feedback::GracefulShutdownReason::kUserRequest},
                                                 /*uptime=*/std::nullopt,
                                                 /*runtime=*/std::nullopt);

  EXPECT_FALSE(last_reboot.has_runtime());
}

TEST(LastRebootInfoProviderTest, Succeed_NotParseable) {
  const auto last_reboot = GetLastRebootUnGraceful(feedback::ZirconRebootReason::kNotParseable);

  EXPECT_FALSE(last_reboot.has_graceful());
  EXPECT_FALSE(last_reboot.has_reason());
}

}  // namespace
}  // namespace last_reboot
}  // namespace forensics
