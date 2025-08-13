// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_DRIVERS_FAKE_FAKE_DISPLAY_STACK_H_
#define SRC_GRAPHICS_DISPLAY_DRIVERS_FAKE_FAKE_DISPLAY_STACK_H_

#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <fidl/fuchsia.sysmem2/cpp/wire.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async-loop/loop.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/driver_runtime.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/fdf/cpp/dispatcher.h>
#include <lib/fit/function.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/result.h>

#include <memory>
#include <optional>

#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/coordinator/controller.h"
#include "src/graphics/display/drivers/fake/fake-display.h"
#include "src/graphics/display/drivers/fake/sysmem-service-provider.h"
#include "src/graphics/display/lib/api-protocols/cpp/display-engine-events-fidl.h"
#include "src/graphics/display/lib/api-protocols/cpp/display-engine-fidl-adapter.h"

namespace fake_display {

// FakeDisplayStack creates and holds a FakeDisplay device as well as the
// Sysmem device and the display coordinator Controller which are attached to
// the fake display device and clients can connect to.
class FakeDisplayStack {
 public:
  FakeDisplayStack(std::unique_ptr<SysmemServiceProvider> sysmem_service_provider,
                   const FakeDisplayDeviceConfig& device_config);

  FakeDisplayStack(const FakeDisplayStack&) = delete;
  FakeDisplayStack(FakeDisplayStack&&) = delete;
  FakeDisplayStack& operator=(const FakeDisplayStack&) = delete;
  FakeDisplayStack& operator=(FakeDisplayStack&&) = delete;

  ~FakeDisplayStack();

  // Must not be called after SyncShutdown().
  FakeDisplay& display_engine();

  // Must not be called after SyncShutdown().
  //
  // The returned client is guaranteed to be valid.
  const fidl::WireSyncClient<fuchsia_hardware_display::Provider>& display_provider_client();

  // Must not be called after SyncShutdown().
  fidl::ClientEnd<fuchsia_sysmem2::Allocator> ConnectToSysmemAllocatorV2();

  // Must be called at least once.
  //
  // Join all threads providing display and sysmem protocols, and remove all
  // the devices bound to the mock root device.
  void SyncShutdown();

 private:
  std::optional<fdf_testing::ScopedGlobalLogger> logger_;

  std::shared_ptr<fdf_testing::DriverRuntime> driver_runtime_;
  std::unique_ptr<SysmemServiceProvider> sysmem_service_provider_;

  fdf::UnownedSynchronizedDispatcher engine_driver_dispatcher_;
  fdf::UnownedSynchronizedDispatcher coordinator_driver_dispatcher_;

  display::DisplayEngineEventsFidl engine_events_;
  std::unique_ptr<FakeDisplay> display_engine_;
  std::unique_ptr<display::DisplayEngineFidlAdapter> fidl_adapter_;

  async_patterns::TestDispatcherBound<std::unique_ptr<display_coordinator::Controller>>
      coordinator_controller_;

  bool shutdown_ = false;

  fidl::WireSyncClient<fuchsia_hardware_display::Provider> display_provider_client_;
};

}  // namespace fake_display

#endif  // SRC_GRAPHICS_DISPLAY_DRIVERS_FAKE_FAKE_DISPLAY_STACK_H_
