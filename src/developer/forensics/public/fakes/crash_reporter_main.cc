// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.feedback.testing/cpp/fidl.h>
#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include <cstdlib>
#include <memory>
#include <utility>

#include "src/developer/forensics/public/fakes/crash_reporter.h"

int main(int argc, const char** argv) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"forensics", "test"}).BuildAndInitialize();

  FX_LOGS(INFO) << "Starting FakeCrashReporter";

  component::OutgoingDirectory outgoing(loop.dispatcher());

  if (const zx::result result = outgoing.ServeFromStartupInfo(); result.is_error()) {
    FX_LOGS(ERROR) << "Failed to serve outgoing directory: " << result.status_string();
    return EXIT_FAILURE;
  }

  auto crash_reporter = std::make_unique<forensics::fakes::CrashReporter>(loop.dispatcher());
  forensics::fakes::CrashReporter* crash_reporter_ptr = crash_reporter.get();

  if (const zx::result result =
          outgoing.AddProtocol<fuchsia_feedback::CrashReporter>(std::move(crash_reporter));
      result.is_error()) {
    FX_LOGS(ERROR) << "Failed to add CrashReporter protocol: " << result.status_string();
    return EXIT_FAILURE;
  }

  if (const zx::result result =
          outgoing.AddUnmanagedProtocol<fuchsia_feedback_testing::FakeCrashReporterQuerier>(
              [crash_reporter_ptr](
                  fidl::ServerEnd<fuchsia_feedback_testing::FakeCrashReporterQuerier> server_end) {
                crash_reporter_ptr->SetQuerier(std::move(server_end));
              });
      result.is_error()) {
    FX_LOGS(ERROR) << "Failed to add FakeCrashReporterQuerier protocol: " << result.status_string();
    return EXIT_FAILURE;
  }

  loop.Run();

  return EXIT_SUCCESS;
}
