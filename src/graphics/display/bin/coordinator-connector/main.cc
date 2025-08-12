// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace-provider/provider.h>

#include "src/graphics/display/bin/coordinator-connector/service-factory.h"

int main(int argc, const char** argv) {
  async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
  trace::TraceProviderWithFdio trace_provider(loop.dispatcher());

  component::OutgoingDirectory outgoing(loop.dispatcher());

  zx::result<> serve_outgoing_directory_result = outgoing.ServeFromStartupInfo();
  if (serve_outgoing_directory_result.is_error()) {
    FX_LOGS(ERROR) << "Failed to serve outgoing directory: "
                   << serve_outgoing_directory_result.status_string();
    return -1;
  }

  FX_LOGS(INFO) << "Starting standalone fuchsia.hardware.display.Service service.";

  display::ServiceCoordinatorFactory service_coordinator_factory;

  fuchsia_hardware_display::Service::InstanceHandler display_service_handler({
      .provider = service_coordinator_factory.bind_handler(loop.dispatcher()),
  });
  zx::result<> publish_service_result =
      outgoing.AddService<fuchsia_hardware_display::Service>(std::move(display_service_handler));
  if (publish_service_result.is_error()) {
    FX_LOGS(ERROR) << "Cannot publish display Service to default service directory: "
                   << publish_service_result.status_string();
    return -1;
  }

  loop.Run();

  FX_LOGS(INFO) << "Quit Display Coordinator Connector main loop.";

  return 0;
}
