// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/testing/fake-coordinator-connector/service.h"

#include <lib/async/cpp/task.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/sys/cpp/component_context.h>
#include <lib/syslog/cpp/macros.h>

#include <memory>

#include "src/graphics/display/drivers/coordinator/client-priority.h"
#include "src/graphics/display/drivers/fake/sysmem-service-forwarder.h"

namespace display {

FakeDisplayCoordinatorConnector::FakeDisplayCoordinatorConnector(
    async_dispatcher_t* dispatcher,
    const fake_display::FakeDisplayDeviceConfig& fake_display_device_config) {
  FX_DCHECK(dispatcher);

  zx::result<std::unique_ptr<fake_display::SysmemServiceForwarder>>
      sysmem_service_forwarder_result = fake_display::SysmemServiceForwarder::Create();
  FX_CHECK(sysmem_service_forwarder_result.is_ok());

  auto fake_display_stack = std::make_unique<fake_display::FakeDisplayStack>(
      std::move(sysmem_service_forwarder_result).value(), fake_display_device_config);
  state_ = std::shared_ptr<State>(
      new State{.dispatcher = dispatcher, .fake_display_stack = std::move(fake_display_stack)});
}

FakeDisplayCoordinatorConnector::~FakeDisplayCoordinatorConnector() {
  state_->fake_display_stack->SyncShutdown();
}

void FakeDisplayCoordinatorConnector::OpenCoordinatorWithListenerForPrimary(
    OpenCoordinatorWithListenerForPrimaryRequest& request,
    OpenCoordinatorWithListenerForPrimaryCompleter::Sync& completer) {
  ConnectClient(
      OpenCoordinatorRequest{
          .is_virtcon = false,
          .coordinator_request = std::move(*request.coordinator()),
          .coordinator_listener_client_end = std::move(*request.coordinator_listener()),
          .on_coordinator_opened =
              [async_completer = completer.ToAsync()](zx::result<> result) mutable {
                async_completer.Reply(result);
              },
      },
      state_);
}

void FakeDisplayCoordinatorConnector::OpenCoordinatorWithListenerForVirtcon(
    OpenCoordinatorWithListenerForVirtconRequest& request,
    OpenCoordinatorWithListenerForVirtconCompleter::Sync& completer) {
  ConnectClient(
      OpenCoordinatorRequest{
          .is_virtcon = true,
          .coordinator_request = std::move(*request.coordinator()),
          .coordinator_listener_client_end = std::move(*request.coordinator_listener()),
          .on_coordinator_opened =
              [async_completer = completer.ToAsync()](zx::result<> result) mutable {
                async_completer.Reply(result);
              },
      },
      state_);
}

// static
void FakeDisplayCoordinatorConnector::ConnectClient(OpenCoordinatorRequest request,
                                                    const std::shared_ptr<State>& state) {
  FX_DCHECK(state);
  display_coordinator::ClientPriority client_priority =
      request.is_virtcon ? display_coordinator::ClientPriority::kVirtcon
                         : display_coordinator::ClientPriority::kPrimary;
  zx::result<> result = state->fake_display_stack->ConnectCoordinatorClient(
      client_priority, std::move(request.coordinator_request),
      std::move(request.coordinator_listener_client_end),
      /*on_client_disconnected=*/
      []() {});
  request.on_coordinator_opened(result);
}

}  // namespace display
