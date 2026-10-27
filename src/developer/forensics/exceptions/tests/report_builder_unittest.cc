// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be found in the LICENSE file.

#include "src/developer/forensics/exceptions/handler/report_builder.h"

#include <fidl/fuchsia.feedback/cpp/natural_ostream.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/developer/forensics/exceptions/constants.h"
#include "src/developer/forensics/exceptions/tests/crasher_wrapper.h"
#include "src/lib/fsl/vmo/strings.h"

namespace forensics {
namespace exceptions {
namespace handler {
namespace {

using ::testing::Contains;
using ::testing::Optional;

class CrashReportBuilderTest : public testing::Test {
 protected:
  // Use an invalid process/thread becuase we don't care about the specific name and koid of each.
  void SetUp() { builder_.SetProcess(zx::process{}).SetThread(zx::thread{}); }

  CrashReportBuilder builder_;
};

TEST_F(CrashReportBuilderTest, SetsMinidump) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()));

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  ASSERT_TRUE(crash_report.specific_report().has_value());
  ASSERT_EQ(crash_report.specific_report()->Which(),
            fuchsia_feedback::SpecificCrashReport::Tag::kNative);

  std::optional<fuchsia_mem::Buffer>& minidump =
      crash_report.specific_report()->native()->minidump();
  ASSERT_TRUE(minidump.has_value());

  std::string minidump_content;
  ASSERT_TRUE(fsl::StringFromVmo(fsl::SizedVmo(std::move(minidump->vmo()), minidump->size()),
                                 &minidump_content));
  EXPECT_STREQ(minidump_content.c_str(), "minidump");

  ASSERT_TRUE(crash_report.annotations().has_value());
  EXPECT_THAT(crash_report.annotations().value(),
              Contains(fuchsia_feedback::Annotation(kCrashProcessStateKey, "in exception")));
}

TEST_F(CrashReportBuilderTest, ExceptionReason_ChannelOverflow) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kChannelOverflow);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-unknown_process-channel-overflow");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PortObserverOverflow) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kPortObserverOverflow);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-unknown_process-port-observer-overflow");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PortPacketOverflow) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kPortPacketOverflow);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-unknown_process-port-packet-overflow");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PageFaultIo) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kPageFaultIo);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-io");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PageFaultIoDataIntegrity) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kPageFaultIoDataIntegrity);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-io_data_integrity");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PageFaultBadState) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kPageFaultBadState);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-bad_state");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PageFaultNoMemory) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kPageFaultNoMemory);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-no_memory");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PageFaultTimedOut) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kPageFaultTimedOut);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-timed_out");
}

TEST_F(CrashReportBuilderTest, ExceptionReason_PageFaultTimedOutMinidumpSkipped) {
  builder_.SetMinidumpSkipped().SetExceptionReason(ExceptionReason::kPageFaultTimedOut);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-timed_out");

  ASSERT_TRUE(crash_report.specific_report().has_value());
  ASSERT_EQ(crash_report.specific_report()->Which(),
            fuchsia_feedback::SpecificCrashReport::Tag::kNative);
  EXPECT_FALSE(crash_report.specific_report()->native()->minidump().has_value());

  ASSERT_TRUE(crash_report.annotations().has_value());
  EXPECT_THAT(crash_report.annotations().value(),
              Contains(fuchsia_feedback::Annotation(kCrashProcessStateKey, "in exception")));
}

TEST_F(CrashReportBuilderTest, ExceptionReason_Arm64UnknownException) {
  fsl::SizedVmo minidump_vmo;
  ASSERT_TRUE(fsl::VmoFromString("minidump", &minidump_vmo));

  builder_.SetMinidump(std::move(minidump_vmo.vmo()))
      .SetExceptionReason(ExceptionReason::kArm64UnknownException);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-arm64-unknown-exception");
}

TEST_F(CrashReportBuilderTest, ProcessTerminated) {
  builder_.SetProcessTerminated();

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.program_name(), "unknown_process");

  EXPECT_FALSE(crash_report.program_uptime().has_value());

  ASSERT_TRUE(crash_report.specific_report().has_value());
  ASSERT_EQ(crash_report.specific_report()->Which(),
            fuchsia_feedback::SpecificCrashReport::Tag::kNative);
  EXPECT_FALSE(crash_report.specific_report()->native()->minidump().has_value());

  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-no-minidump-process-terminated");

  ASSERT_TRUE(crash_report.annotations().has_value());
  EXPECT_THAT(crash_report.annotations().value(),
              Contains(fuchsia_feedback::Annotation(kCrashProcessStateKey, "terminated")));
}

TEST_F(CrashReportBuilderTest, ExpiredException) {
  builder_.SetExceptionExpired();

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  ASSERT_TRUE(crash_report.specific_report().has_value());
  ASSERT_EQ(crash_report.specific_report()->Which(),
            fuchsia_feedback::SpecificCrashReport::Tag::kNative);
  EXPECT_FALSE(crash_report.specific_report()->native()->minidump().has_value());

  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-no-minidump-exception-expired");
}

TEST_F(CrashReportBuilderTest, ExceptionReasonOverwritesExceptionExpired) {
  builder_.SetExceptionExpired();
  builder_.SetExceptionReason(ExceptionReason::kPageFaultIo);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-io");
}

TEST_F(CrashReportBuilderTest, ExceptionReasonOverwritesProcessTerminated) {
  builder_.SetProcessTerminated();
  builder_.SetExceptionReason(ExceptionReason::kPageFaultIo);

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_EQ(crash_report.crash_signature(), "fuchsia-page_fault-io");
}

TEST_F(CrashReportBuilderTest, IsFatal) {
  builder_.SetExceptionExpired();

  fuchsia_feedback::CrashReport crash_report = builder_.Consume();
  EXPECT_THAT(crash_report.is_fatal(), Optional(true));
}

TEST(ReportBuilderTest, TestUptime) {
  CrashReportBuilder builder;
  ExceptionContext exception;

  // Spawn the 'crasher' process.
  ASSERT_TRUE(SpawnCrasher(&exception));
  // If we don't mark it as handled, the exception will bubble out of our environment.
  ASSERT_TRUE(MarkExceptionAsHandled(&exception));

  zx::process process;
  zx::thread thread;
  ASSERT_EQ(exception.exception.get_process(&process), ZX_OK);
  ASSERT_EQ(exception.exception.get_thread(&thread), ZX_OK);
  builder.SetProcess(process).SetThread(thread);
  builder.SetProcessTerminated();

  fuchsia_feedback::CrashReport crash_report = builder.Consume();
  EXPECT_EQ(crash_report.program_name(), "crasher");
  ASSERT_TRUE(crash_report.program_uptime().has_value());
  EXPECT_GE(crash_report.program_uptime().value(), 0);

  // We kill the jobs. This kills the underlying process. We do this so that the crashed process
  // doesn't get rescheduled. Otherwise the exception on the crash program would bubble out of our
  // environment and create noise on the overall system.
  exception.job.kill();
}

}  // namespace
}  // namespace handler
}  // namespace exceptions
}  // namespace forensics
