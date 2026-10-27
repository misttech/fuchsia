// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.feedback/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/log_settings.h>
#include <lib/syslog/cpp/macros.h>

#include <cstdlib>
#include <memory>

#include "src/developer/forensics/testing/fakes/data_provider.h"

int main(int argc, const char** argv) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  fuchsia_logging::LogSettingsBuilder builder;
  builder.WithTags({"forensics", "test"}).BuildAndInitialize();

  FX_LOGS(INFO) << "Starting FakeDataProvider";

  component::OutgoingDirectory outgoing(loop.dispatcher());

  if (const zx::result result = outgoing.ServeFromStartupInfo(); result.is_error()) {
    FX_LOGS(ERROR) << "Failed to serve outgoing directory: " << result.status_string();
    return EXIT_FAILURE;
  }

  if (const zx::result result = outgoing.AddProtocol<fuchsia_feedback::DataProvider>(
          std::make_unique<forensics::fakes::DataProvider>());
      result.is_error()) {
    FX_LOGS(ERROR) << "Failed to add DataProvider protocol: " << result.status_string();
    return EXIT_FAILURE;
  }

  loop.Run();

  return EXIT_SUCCESS;
}
