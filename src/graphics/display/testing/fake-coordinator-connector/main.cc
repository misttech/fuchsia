// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/default.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/syslog/cpp/macros.h>

#include <chrono>
#include <thread>

#include "src/graphics/display/lib/api-types/cpp/engine-info.h"
#include "src/graphics/display/lib/api-types/cpp/mode.h"
#include "src/graphics/display/lib/fake-display-stack/fake-display-device-config.h"
#include "src/graphics/display/lib/fake-display-stack/fake-display-stack.h"
#include "src/graphics/display/lib/fake-display-stack/sysmem-service-forwarder.h"

int main(int argc, const char** argv) {
  FX_LOGS(INFO) << "Starting fake fuchsia.hardware.display.Service service.";

  static constexpr fake_display::FakeDisplayDeviceConfig kFakeDisplayDeviceConfig = {
      // TODO(https://fxbug.dev/42079786): Populate from structured configuration.
      .display_mode = display::Mode({
          .active_width = 1280,
          .active_height = 800,
          .refresh_rate_millihertz = 60'000,
      }),
      .engine_info = display::EngineInfo({
          .max_layer_count = 1,
          .max_connected_display_count = 1,
          .is_capture_supported = true,
      }),
      .periodic_vsync = true,
  };

  zx::result<std::unique_ptr<fake_display::SysmemServiceForwarder>>
      sysmem_service_forwarder_result = fake_display::SysmemServiceForwarder::Create();
  FX_CHECK(sysmem_service_forwarder_result.is_ok());

  std::unique_ptr<fake_display::SysmemServiceForwarder> sysmem_service_forwarder =
      std::move(sysmem_service_forwarder_result).value();

  fake_display::FakeDisplayStack fake_display_stack(std::move(sysmem_service_forwarder),
                                                    kFakeDisplayDeviceConfig);

  fake_display_stack.ServeCoordinatorToProcessOutgoingDirectory();

  while (true) {
    std::this_thread::sleep_for(std::chrono::seconds(1));
  }
  return 0;
}
