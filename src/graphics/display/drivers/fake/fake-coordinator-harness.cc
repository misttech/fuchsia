// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/fake/fake-coordinator-harness.h"

#include <fidl/fuchsia.hardware.display.engine/cpp/wire.h>
#include <fidl/fuchsia.hardware.display/cpp/wire.h>
#include <zircon/assert.h>
#include <zircon/status.h>

#include <memory>
#include <utility>

#include "src/graphics/display/drivers/coordinator/controller.h"
#include "src/graphics/display/drivers/coordinator/engine-driver-client-fidl.h"

namespace fake_display {

FakeCoordinatorHarness::FakeCoordinatorHarness(
    fdf_testing::DriverRuntime* driver_runtime,
    fdf::ClientEnd<fuchsia_hardware_display_engine::Engine> engine_client)
    : driver_runtime_(*driver_runtime),
      coordinator_driver_dispatcher_(driver_runtime_.StartBackgroundDispatcher()),
      coordinator_controller_(coordinator_driver_dispatcher_->async_dispatcher(), std::in_place) {
  ZX_ASSERT(driver_runtime != nullptr);

  coordinator_controller_.SyncCall(
      [&](std::unique_ptr<display_coordinator::Controller>* controller) {
        auto engine_driver_client =
            std::make_unique<display_coordinator::EngineDriverClientFidl>(std::move(engine_client));
        zx::result<std::unique_ptr<display_coordinator::Controller>> create_result =
            display_coordinator::Controller::Create(std::move(engine_driver_client),
                                                    coordinator_driver_dispatcher_->borrow());
        ZX_ASSERT_MSG(create_result.is_ok(),
                      "Failed to create display coordinator Controller device: %s",
                      create_result.status_string());
        *controller = std::move(create_result).value();
      });

  auto [provider_client, provider_server] =
      fidl::Endpoints<fuchsia_hardware_display::Provider>::Create();
  provider_client_ =
      fidl::WireSyncClient<fuchsia_hardware_display::Provider>(std::move(provider_client));
  coordinator_controller_.SyncCall(
      [&](std::unique_ptr<display_coordinator::Controller>* controller) {
        fidl::BindServer(coordinator_driver_dispatcher_->async_dispatcher(),
                         std::move(provider_server), controller->get());
      });
}

FakeCoordinatorHarness::~FakeCoordinatorHarness() {
  ZX_ASSERT_MSG(shutdown_, "SyncShutdown() not called before FakeCoordinatorHarness destruction");
}

void FakeCoordinatorHarness::SyncShutdown() {
  if (shutdown_) {
    // SyncShutdown() was already called.
    return;
  }
  shutdown_ = true;

  coordinator_controller_.SyncCall(
      [&](std::unique_ptr<display_coordinator::Controller>* controller) {
        (*controller)->PrepareStop();
      });

  std::unique_ptr<display_coordinator::Controller> controller_to_reset =
      coordinator_controller_.SyncCall(
          [&](std::unique_ptr<display_coordinator::Controller>* controller) {
            return std::move(*controller);
          });

  libsync::Completion coordinator_is_shut_down;
  driver_runtime_.ShutdownBackgroundDispatcher(coordinator_driver_dispatcher_->get(), [&] {
    controller_to_reset.reset();
    coordinator_is_shut_down.Signal();
  });
  coordinator_is_shut_down.Wait();
}

const fidl::WireSyncClient<fuchsia_hardware_display::Provider>&
FakeCoordinatorHarness::provider_client() const {
  ZX_ASSERT_MSG(!shutdown_, "provider_client() called after SyncShutdown()");
  return provider_client_;
}

}  // namespace fake_display
