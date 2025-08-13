// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/drivers/fake/fake-display-stack.h"

#include <fidl/fuchsia.hardware.display.engine/cpp/fidl.h>
#include <fidl/fuchsia.sysmem2/cpp/fidl.h>
#include <lib/async_patterns/testing/cpp/dispatcher_bound.h>
#include <lib/component/incoming/cpp/service.h>
#include <lib/fdio/directory.h>
#include <lib/sync/cpp/completion.h>
#include <zircon/assert.h>
#include <zircon/errors.h>
#include <zircon/status.h>

#include <memory>
#include <utility>

#include "src/devices/testing/mock-ddk/mock-device.h"
#include "src/graphics/display/drivers/coordinator/controller.h"
#include "src/graphics/display/drivers/coordinator/engine-driver-client-fidl.h"
#include "src/graphics/display/drivers/fake/fake-display.h"

namespace fake_display {

FakeDisplayStack::FakeDisplayStack(std::unique_ptr<SysmemServiceProvider> sysmem_service_provider,
                                   const FakeDisplayDeviceConfig& device_config)
    : driver_runtime_(mock_ddk::GetDriverRuntime()),
      sysmem_service_provider_(std::move(sysmem_service_provider)),
      engine_driver_dispatcher_(driver_runtime_->StartBackgroundDispatcher()),
      coordinator_driver_dispatcher_(driver_runtime_->StartBackgroundDispatcher()),
      coordinator_controller_(coordinator_driver_dispatcher_->async_dispatcher(), std::in_place) {
  if (!fdf::Logger::HasGlobalInstance()) {
    logger_.emplace();
  }

  fidl::ClientEnd<fuchsia_sysmem2::Allocator> sysmem_client = ConnectToSysmemAllocatorV2();
  display_engine_ = std::make_unique<FakeDisplay>(&engine_events_, std::move(sysmem_client),
                                                  device_config, inspect::Inspector{});
  fidl_adapter_ =
      std::make_unique<display::DisplayEngineFidlAdapter>(display_engine_.get(), &engine_events_);

  auto [engine_client, engine_server] =
      fdf::Endpoints<fuchsia_hardware_display_engine::Engine>::Create();
  zx::result<> bind_engine_result =
      fdf::RunOnDispatcherSync(engine_driver_dispatcher_->async_dispatcher(), [&]() {
        fidl::ProtocolHandler<fuchsia_hardware_display_engine::Engine> fidl_handler =
            fidl_adapter_->CreateHandler(*(engine_driver_dispatcher_->get()));
        fidl_handler(std::move(engine_server));
      });
  if (bind_engine_result.is_error()) {
    ZX_PANIC("Failed to handle engine protocol: %s", bind_engine_result.status_string());
  }

  coordinator_controller_.SyncCall(
      [&](std::unique_ptr<display_coordinator::Controller>* controller) {
        auto engine_driver_client =
            std::make_unique<display_coordinator::EngineDriverClientFidl>(std::move(engine_client));
        zx::result<std::unique_ptr<display_coordinator::Controller>> create_result =
            display_coordinator::Controller::Create(std::move(engine_driver_client),
                                                    coordinator_driver_dispatcher_->borrow());
        if (create_result.is_error()) {
          ZX_PANIC("Failed to create display coordinator Controller device: %s",
                   create_result.status_string());
        }
        *controller = std::move(create_result).value();
      });

  auto [display_provider_client, display_provider_server] =
      fidl::Endpoints<fuchsia_hardware_display::Provider>::Create();
  display_provider_client_ =
      fidl::WireSyncClient<fuchsia_hardware_display::Provider>(std::move(display_provider_client));
  coordinator_controller_.SyncCall(
      [&](std::unique_ptr<display_coordinator::Controller>* controller) {
        fidl::BindServer(coordinator_driver_dispatcher_->async_dispatcher(),
                         std::move(display_provider_server), controller->get());
      });
}

FakeDisplayStack::~FakeDisplayStack() {
  ZX_ASSERT_MSG(shutdown_, "FakeDisplayStack::SyncShutdown() not called");
}

FakeDisplay& FakeDisplayStack::display_engine() {
  ZX_ASSERT(!shutdown_);
  ZX_ASSERT(display_engine_ != nullptr);
  return *display_engine_;
}

const fidl::WireSyncClient<fuchsia_hardware_display::Provider>&
FakeDisplayStack::display_provider_client() {
  ZX_ASSERT(!shutdown_);
  ZX_ASSERT(display_provider_client_.is_valid());
  return display_provider_client_;
}

fidl::ClientEnd<fuchsia_sysmem2::Allocator> FakeDisplayStack::ConnectToSysmemAllocatorV2() {
  ZX_ASSERT(!shutdown_);

  zx::result<fidl::ClientEnd<fuchsia_sysmem2::Allocator>> connect_allocator_result =
      sysmem_service_provider_->ConnectAllocator2();
  if (connect_allocator_result.is_error()) {
    ZX_PANIC("Failed to connect to sysmem Allocator service: %s",
             connect_allocator_result.status_string());
  }
  return std::move(connect_allocator_result).value();
}

void FakeDisplayStack::SyncShutdown() {
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
  driver_runtime_->ShutdownBackgroundDispatcher(coordinator_driver_dispatcher_->get(), [&] {
    controller_to_reset.reset();
    coordinator_is_shut_down.Signal();
  });
  coordinator_is_shut_down.Wait();

  libsync::Completion engine_driver_dispatcher_is_shut_down;
  driver_runtime_->ShutdownBackgroundDispatcher(
      engine_driver_dispatcher_->get(), [&] { engine_driver_dispatcher_is_shut_down.Signal(); });
  engine_driver_dispatcher_is_shut_down.Wait();

  display_engine_.reset();

  sysmem_service_provider_.reset();
}

}  // namespace fake_display
