// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/developer/forensics/feedback/reboot_log/reboot_log.h"

#include <fuchsia/hardware/power/statecontrol/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>

#include <string>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/feedback/reboot_log/graceful_shutdown_info.h"
#include "src/developer/forensics/testing/gpretty_printers.h"  // IWYU pragma: keep
#include "src/developer/forensics/testing/unit_test_fixture.h"
#include "src/lib/files/file.h"
#include "src/lib/files/scoped_temp_dir.h"
#include "src/lib/fxl/strings/join_strings.h"
#include "src/lib/timekeeper/test_clock.h"

namespace forensics {
namespace feedback {
namespace {

using fuchsia::hardware::power::statecontrol::ShutdownAction;
using fuchsia::hardware::power::statecontrol::ShutdownReason;

struct RebootReasonTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::optional<ShutdownReason> shutdown_reason;
  std::string output_reboot_reason;
};

struct ShutdownActionTestParam {
  std::string test_name;
  std::optional<ShutdownAction> shutdown_action;
  std::optional<GracefulShutdownAction> output_shutdown_action;
};

struct RebootMultiReasonTestParam {
  std::string test_name;
  std::vector<ShutdownReason> reasons;
  std::string output_reboot_reason;
};

struct TimeTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::optional<zx::duration> output_uptime;
  std::optional<zx::duration> output_runtime;
};

struct CriticalProcessTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::optional<std::string> output_critical_process;
};

struct RebootLogStrTestParam {
  std::string test_name;
  std::optional<std::string> zircon_reboot_log;
  std::vector<ShutdownReason> shutdown_reasons;
  std::optional<std::string> output_reboot_log_str;
};

template <typename TestParam>
class RebootLogTest : public UnitTestFixture, public testing::WithParamInterface<TestParam> {
 protected:
  void WriteZirconRebootLogContents(const std::string& contents) {
    FX_CHECK(tmp_dir_.NewTempFileWithData(contents, &zircon_reboot_log_path_))
        << "Failed to create temporary Zircon reboot log";
  }

  void WriteGracefulShutdownInfoContents(const std::string& contents) {
    FX_CHECK(tmp_dir_.NewTempFileWithData(contents, &graceful_shutdown_info_path_))
        << "Failed to create temporary graceful shutdown info";
  }

  void WriteGracefulShutdownInfoContents(
      fuchsia::hardware::power::statecontrol::ShutdownOptions options) {
    FX_CHECK(tmp_dir_.NewTempFileWithData("", &graceful_shutdown_info_path_))
        << "Failed to create temporary graceful shutdown info";

    FX_CHECK(files::WriteFile(
        graceful_shutdown_info_path_,
        ToJson(ToGracefulShutdownAction(options), ToGracefulShutdownReasons(options))));
  }

  std::string WriteLegacyGracefulRebootLogContents(
      fuchsia::hardware::power::statecontrol::ShutdownOptions options) {
    std::string path;
    FX_CHECK(tmp_dir_.NewTempFileWithData("", &path))
        << "Failed to create temporary graceful reboot log";

    FX_CHECK(
        files::WriteFile(path, ToLegacyFileContentForTesting(ToGracefulShutdownReasons(options))));
    return path;
  }

  std::string zircon_reboot_log_path_;
  std::string graceful_shutdown_info_path_;

 private:
  timekeeper::TestClock clock_;
  files::ScopedTempDir tmp_dir_;
};

using RebootLogReasonTest = RebootLogTest<RebootReasonTestParam>;

fuchsia::hardware::power::statecontrol::ShutdownOptions NewShutdownOptions(
    ShutdownAction action, std::vector<ShutdownReason> reasons) {
  fuchsia::hardware::power::statecontrol::ShutdownOptions options;
  options.set_action(action);
  options.set_reasons(std::move(reasons));
  return options;
}

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogReasonTest,
    ::testing::ValuesIn(std::vector<RebootReasonTestParam>({
        {
            "ZirconCleanNoGraceful",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            std::nullopt,
            "GENERIC GRACEFUL",
        },
        {
            "ZirconCleanGracefulUserRequest",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "USER REQUEST",
        },
        {
            "ZirconCleanGracefulSystemUpdate",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::SYSTEM_UPDATE,
            "SYSTEM UPDATE",
        },
        {
            "ZirconCleanGracefulNetstackMigration",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::NETSTACK_MIGRATION,
            "NETSTACK MIGRATION",
        },
        {
            "ZirconCleanGracefulHighTemperature",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::HIGH_TEMPERATURE,
            "HIGH TEMPERATURE",
        },
        {
            "ZirconCleanGracefulSessionFailure",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::SESSION_FAILURE,
            "SESSION FAILURE",
        },
        {
            "ZirconCleanGracefulNotSupported",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            static_cast<ShutdownReason>(1000u),
            "GENERIC GRACEFUL",
        },
        {
            "Cold",
            std::nullopt,
            ShutdownReason::USER_REQUEST,
            "COLD",
        },
        {
            "KernelPanic",
            "ZIRCON REBOOT REASON (KERNEL PANIC)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "KERNEL PANIC",
        },
        {
            "OOM",
            "ZIRCON REBOOT REASON (OOM)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "OOM",
        },
        {
            "SwWatchdog",
            "ZIRCON REBOOT REASON (SW WATCHDOG)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "SOFTWARE WATCHDOG TIMEOUT",
        },
        {
            "HwWatchdog",
            "ZIRCON REBOOT REASON (HW WATCHDOG)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "HARDWARE WATCHDOG TIMEOUT",
        },
        {
            "Brownout",
            "ZIRCON REBOOT REASON (BROWNOUT)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "BROWNOUT",
        },
        {
            "Spontaneous",
            "ZIRCON REBOOT REASON (UNKNOWN)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "SPONTANEOUS",
        },
        {
            "RootJobTermination",
            "ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            ShutdownReason::USER_REQUEST,
            "ROOT JOB TERMINATION",
        },
        {
            "NotParseable",
            "NOT PARSEABLE",
            ShutdownReason::USER_REQUEST,
            "NOT PARSEABLE",
        },
    })),
    [](const testing::TestParamInfo<RebootReasonTestParam>& info) { return info.param.test_name; });

TEST_P(RebootLogReasonTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  if (param.shutdown_reason.has_value()) {
    WriteGracefulShutdownInfoContents(
        NewShutdownOptions(ShutdownAction::REBOOT, {param.shutdown_reason.value()}));
  }

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), param.output_reboot_reason);
}

using ColdBootTest = RebootLogTest<ShutdownActionTestParam>;

INSTANTIATE_TEST_SUITE_P(WithVariousShutdownActions, ColdBootTest,
                         ::testing::ValuesIn(std::vector<ShutdownActionTestParam>({
                             {
                                 "Reboot",
                                 ShutdownAction::REBOOT,
                                 std::nullopt,
                             },
                             {
                                 "RebootToRecovery",
                                 ShutdownAction::REBOOT_TO_RECOVERY,
                                 std::nullopt,
                             },
                             {
                                 "RebootToBootloader",
                                 ShutdownAction::REBOOT_TO_BOOTLOADER,
                                 std::nullopt,
                             },
                         })),
                         [](const testing::TestParamInfo<ShutdownActionTestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(ColdBootTest, OnlyPoweroffOverridesColdBoot) {
  const ShutdownActionTestParam& param = GetParam();

  WriteGracefulShutdownInfoContents(
      NewShutdownOptions(*param.shutdown_action, {ShutdownReason::USER_REQUEST}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_FALSE(reboot_log.GetFinalShutdownInfo().ToGracefulShutdownAction().has_value());
  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "COLD");
}

TEST_F(ColdBootTest, UsesGracefulPoweroffReasons) {
  WriteGracefulShutdownInfoContents(
      NewShutdownOptions(ShutdownAction::POWEROFF, {ShutdownReason::USER_REQUEST}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToGracefulShutdownAction(),
            GracefulShutdownAction::kPoweroff);
  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "USER REQUEST");
}

TEST_F(ColdBootTest, EmptyGracefulReasonsIsGenericGraceful) {
  WriteGracefulShutdownInfoContents(NewShutdownOptions(ShutdownAction::POWEROFF, {}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToGracefulShutdownAction(),
            GracefulShutdownAction::kPoweroff);
  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "GENERIC GRACEFUL");
}

TEST_F(ColdBootTest, EmptyGracefulInfoIsCold) {
  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_FALSE(reboot_log.GetFinalShutdownInfo().ToGracefulShutdownAction().has_value());
  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "COLD");
}

using ShutdownActionTest = RebootLogTest<ShutdownActionTestParam>;

INSTANTIATE_TEST_SUITE_P(WithVariousShutdownAction, ShutdownActionTest,
                         ::testing::ValuesIn(std::vector<ShutdownActionTestParam>({
                             {
                                 "Poweroff",
                                 ShutdownAction::POWEROFF,
                                 GracefulShutdownAction::kPoweroff,
                             },
                             {
                                 "Reboot",
                                 ShutdownAction::REBOOT,
                                 GracefulShutdownAction::kReboot,
                             },
                             {
                                 "RebootToRecovery",
                                 ShutdownAction::REBOOT_TO_RECOVERY,
                                 GracefulShutdownAction::kRebootToRecovery,
                             },
                             {
                                 "RebootToBootloader",
                                 ShutdownAction::REBOOT_TO_BOOTLOADER,
                                 GracefulShutdownAction::kRebootToBootloader,
                             },
                             {
                                 "Ungraceful",
                                 std::nullopt,
                                 std::nullopt,
                             },
                         })),
                         [](const testing::TestParamInfo<ShutdownActionTestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(ShutdownActionTest, ActionParsed) {
  const ShutdownActionTestParam& param = GetParam();
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");

  if (param.shutdown_action.has_value()) {
    WriteGracefulShutdownInfoContents(NewShutdownOptions(*param.shutdown_action, {}));
  }

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToGracefulShutdownAction(),
            param.output_shutdown_action);
}

TEST_P(RebootLogReasonTest, LegacyTxtFallback) {
  const RebootReasonTestParam& param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  std::string legacy_graceful_reboot_log_path;
  if (param.shutdown_reason.has_value()) {
    legacy_graceful_reboot_log_path = WriteLegacyGracefulRebootLogContents(
        NewShutdownOptions(ShutdownAction::REBOOT, {param.shutdown_reason.value()}));
  }

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                legacy_graceful_reboot_log_path, /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), param.output_reboot_reason);
}

TEST_F(RebootLogReasonTest, Succeed_ZirconCleanGracefulFdr) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulShutdownInfoContents(
      NewShutdownOptions(ShutdownAction::REBOOT, {ShutdownReason::SYSTEM_UPDATE}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/false));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "FACTORY DATA RESET");
}

TEST_F(RebootLogReasonTest, Succeed_ZirconCleanGracefulNotParseable) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulShutdownInfoContents("NOT PARSEABLE");

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "GENERIC GRACEFUL");

  ASSERT_TRUE(reboot_log.Uptime().has_value());
  EXPECT_EQ(*reboot_log.Uptime(), zx::msec(1234));

  ASSERT_TRUE(reboot_log.Runtime().has_value());
  EXPECT_EQ(*reboot_log.Runtime(), zx::msec(1098));
}

TEST_F(RebootLogReasonTest, Succeed_RebootReasonsUnset) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  fuchsia::hardware::power::statecontrol::ShutdownOptions options;
  options.set_action(ShutdownAction::REBOOT);
  WriteGracefulShutdownInfoContents(std::move(options));
  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "GENERIC GRACEFUL");
}

TEST_F(RebootLogReasonTest, Succeed_RebootReasonsEmpty) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulShutdownInfoContents(NewShutdownOptions(ShutdownAction::REBOOT, {}));
  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "GENERIC GRACEFUL");
}

TEST_F(RebootLogReasonTest, SucceedNoGracefulShutdownInfo) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToGracefulShutdownAction(), std::nullopt);
  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "GENERIC GRACEFUL");
}

using RebootLogMultiReasonTest = RebootLogTest<RebootMultiReasonTestParam>;

INSTANTIATE_TEST_SUITE_P(WithVariousRebootLogs, RebootLogMultiReasonTest,
                         ::testing::ValuesIn(std::vector<RebootMultiReasonTestParam>(
                             {{
                                  "SystemUpdateThenNetstackMigration",
                                  {
                                      ShutdownReason::SYSTEM_UPDATE,
                                      ShutdownReason::NETSTACK_MIGRATION,
                                  },
                                  "SYSTEM UPDATE",
                              },
                              {
                                  "NetstackMigrationThenSystemUpdate",
                                  {
                                      ShutdownReason::NETSTACK_MIGRATION,
                                      ShutdownReason::SYSTEM_UPDATE,
                                  },
                                  "SYSTEM UPDATE",
                              },
                              {
                                  "UnexpectedCombination",
                                  {
                                      ShutdownReason::OUT_OF_MEMORY,
                                      ShutdownReason::SYSTEM_UPDATE,
                                  },
                                  "UNEXPECTED REASON GRACEFUL",
                              }})),
                         [](const testing::TestParamInfo<RebootMultiReasonTestParam>& info) {
                           return info.param.test_name;
                         });

TEST_P(RebootLogMultiReasonTest, Succeed) {
  const auto param = GetParam();

  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");

  WriteGracefulShutdownInfoContents(NewShutdownOptions(ShutdownAction::REBOOT, param.reasons));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), param.output_reboot_reason);
}

using RebootLogTimeTest = RebootLogTest<TimeTestParam>;

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogTimeTest,
    ::testing::ValuesIn(std::vector<TimeTestParam>({
        {
            "WellFormedLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            zx::msec(1234),
            zx::msec(1098),
        },
        {
            "NoZirconRebootLog",
            std::nullopt,
            std::nullopt,
            std::nullopt,
        },
        {
            "EmptyZirconRebootLog",
            "",
            std::nullopt,
            std::nullopt,
        },
        {
            "TooFewLinesForUptime",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n",
            std::nullopt,
            std::nullopt,
        },
        {
            "BadUptimeString",
            "BAD REBOOT REASON (NO CRASH)\n\nDOWNTIME (ms)\n1234",
            std::nullopt,
            std::nullopt,
        },
        {
            "TooFewLinesForRuntime",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n",
            zx::msec(1234),
            std::nullopt,
        },
        {
            "BadRuntimeString",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nWALKTIME (ms)\n1098",
            zx::msec(1234),
            std::nullopt,
        },
    })),
    [](const testing::TestParamInfo<TimeTestParam>& info) { return info.param.test_name; });

TEST_P(RebootLogTimeTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  if (param.output_uptime.has_value()) {
    ASSERT_TRUE(reboot_log.Uptime().has_value());
    EXPECT_EQ(*reboot_log.Uptime(), param.output_uptime.value());
  } else {
    EXPECT_FALSE(reboot_log.Uptime().has_value());
  }

  if (param.output_runtime.has_value()) {
    ASSERT_TRUE(reboot_log.Runtime().has_value());
    EXPECT_EQ(*reboot_log.Runtime(), param.output_runtime.value());
  } else {
    EXPECT_FALSE(reboot_log.Runtime().has_value());
  }
}

using RebootLogCriticalProcessTest = RebootLogTest<CriticalProcessTestParam>;

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogCriticalProcessTest,
    ::testing::ValuesIn(std::vector<CriticalProcessTestParam>({
        {
            "WellFormedLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "ROOT JOB TERMINATED BY CRITICAL PROCESS DEATH: foo (1)",
            "foo",
        },
        {
            "NoZirconRebootLog",
            std::nullopt,
            std::nullopt,
        },
        {
            "EmptyZirconRebootLog",
            "",
            std::nullopt,
        },
        {
            "TooFewLines",
            "BAD REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n",
            std::nullopt,
        },
        {
            "BadCriticalProcessString",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "ROOT JOB TERMINATED BY CRITICAL PROCESS ALIVE: foo (1)",
            std::nullopt,
        },
    })),
    [](const testing::TestParamInfo<CriticalProcessTestParam>& info) {
      return info.param.test_name;
    });

TEST_P(RebootLogCriticalProcessTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  if (param.output_critical_process.has_value()) {
    ASSERT_TRUE(reboot_log.CriticalProcess().has_value());
    EXPECT_EQ(*reboot_log.CriticalProcess(), param.output_critical_process.value());
  } else {
    EXPECT_FALSE(reboot_log.CriticalProcess().has_value());
  }
}

using RebootLogStrTest = RebootLogTest<RebootLogStrTestParam>;

INSTANTIATE_TEST_SUITE_P(
    WithVariousRebootLogs, RebootLogStrTest,
    ::testing::ValuesIn(std::vector<RebootLogStrTestParam>({
        {
            "ConcatenatesZirconAndGraceful",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {ShutdownReason::USER_REQUEST},
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: (USER "
            "REQUEST)\n\nFINAL REBOOT REASON (USER REQUEST)",
        },
        {
            // This test is the same as the above test, but is used to show that there may be an
            // ungraceful zircon reboot reason and a graceful reboot reason.
            "ConcatenatesZirconUngracefulAndGraceful",
            "ZIRCON REBOOT REASON (KERNEL PANIC)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {ShutdownReason::USER_REQUEST},
            "ZIRCON REBOOT REASON (KERNEL PANIC)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: "
            "(USER REQUEST)\n\nFINAL REBOOT REASON (KERNEL PANIC)",
        },
        {
            "NoGracefulRebootLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {},
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: "
            "(NONE)\n\nFINAL REBOOT REASON (GENERIC GRACEFUL)",
        },
        {
            "MultipleGracefulRebootLog",
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098",
            {ShutdownReason::NETSTACK_MIGRATION, ShutdownReason::SYSTEM_UPDATE},
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\nGRACEFUL REBOOT REASONS: "
            "(NETSTACK MIGRATION,SYSTEM UPDATE)\n\nFINAL REBOOT REASON (SYSTEM UPDATE)",
        },
        {
            "NoZirconRebootLog",
            std::nullopt,
            {ShutdownReason::USER_REQUEST},
            "GRACEFUL REBOOT REASONS: (USER REQUEST)\n\nFINAL REBOOT REASON (COLD)",
        },
    })),
    [](const testing::TestParamInfo<RebootLogStrTestParam>& info) { return info.param.test_name; });

TEST_P(RebootLogStrTest, Succeed) {
  const auto param = GetParam();
  if (param.zircon_reboot_log.has_value()) {
    WriteZirconRebootLogContents(param.zircon_reboot_log.value());
  }

  if (!param.shutdown_reasons.empty()) {
    WriteGracefulShutdownInfoContents(
        NewShutdownOptions(ShutdownAction::REBOOT, param.shutdown_reasons));
  }

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));

  if (param.output_reboot_log_str.has_value()) {
    EXPECT_EQ(reboot_log.RebootLogStr(), param.output_reboot_log_str.value());
  }
}

TEST_F(RebootLogStrTest, Succeed_SetGracefulFDR) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");
  WriteGracefulShutdownInfoContents(
      NewShutdownOptions(ShutdownAction::REBOOT, {ShutdownReason::FACTORY_DATA_RESET}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.RebootLogStr(),
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "GRACEFUL REBOOT REASONS: (FACTORY DATA RESET)\n\n"
            "FINAL REBOOT REASON (FACTORY DATA RESET)");
}

TEST_F(RebootLogStrTest, Succeed_InferFDR) {
  WriteZirconRebootLogContents(
      "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098");

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/false));
  EXPECT_EQ(reboot_log.GetFinalShutdownInfo().ToRebootReasonString(), "FACTORY DATA RESET");
  EXPECT_EQ(reboot_log.RebootLogStr(),
            "ZIRCON REBOOT REASON (NO CRASH)\n\nUPTIME (ms)\n1234\nRUNTIME (ms)\n1098\n"
            "GRACEFUL REBOOT REASONS: (NONE)\n\nFINAL REBOOT REASON (FACTORY DATA RESET)");
}

TEST_F(RebootLogStrTest, Succeed_SetDlog) {
  constexpr std::string_view kContents =
      R"(ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)

UPTIME (ms)
1234
RUNTIME (ms)
1098

--- BEGIN DLOG DUMP ---
test dlog dump line1
test dlog dump line2

--- END DLOG DUMP ---

GRACEFUL REBOOT REASONS: (NONE)

FINAL REBOOT REASON (ROOT JOB TERMINATION))";

  WriteZirconRebootLogContents(std::string(kContents));
  WriteGracefulShutdownInfoContents(
      NewShutdownOptions(ShutdownAction::REBOOT, {ShutdownReason::CRITICAL_COMPONENT_FAILURE}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.Dlog(), "test dlog dump line1\ntest dlog dump line2");
}

TEST_F(RebootLogStrTest, Succeed_EmptyDlog) {
  constexpr std::string_view kContents =
      R"(ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)

  UPTIME (ms)
  1234
  RUNTIME (ms)
  1098

  --- BEGIN DLOG DUMP ---
  --- END DLOG DUMP ---

  GRACEFUL REBOOT REASONS: (NONE)

  FINAL REBOOT REASON (ROOT JOB TERMINATION))";

  WriteZirconRebootLogContents(std::string(kContents));
  WriteGracefulShutdownInfoContents(
      NewShutdownOptions(ShutdownAction::REBOOT, {ShutdownReason::CRITICAL_COMPONENT_FAILURE}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.Dlog(), "");
}

TEST_F(RebootLogStrTest, Succeed_NoDlog) {
  constexpr std::string_view kContents =
      R"(ZIRCON REBOOT REASON (USERSPACE ROOT JOB TERMINATION)

  UPTIME (ms)
  1234
  RUNTIME (ms)
  1098

  GRACEFUL REBOOT REASONS: (NONE)

  FINAL REBOOT REASON (ROOT JOB TERMINATION))";

  WriteZirconRebootLogContents(std::string(kContents));
  WriteGracefulShutdownInfoContents(
      NewShutdownOptions(ShutdownAction::REBOOT, {ShutdownReason::CRITICAL_COMPONENT_FAILURE}));

  const RebootLog reboot_log(
      RebootLog::ParseRebootLog(zircon_reboot_log_path_, graceful_shutdown_info_path_,
                                /*legacy_graceful_reboot_log_path=*/"", /*not_a_fdr=*/true));
  EXPECT_EQ(reboot_log.Dlog(), std::nullopt);
}

}  // namespace
}  // namespace feedback
}  // namespace forensics
