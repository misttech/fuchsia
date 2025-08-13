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
    const fake_display::FakeDisplayDeviceConfig& fake_display_device_config) {
  zx::result<std::unique_ptr<fake_display::SysmemServiceForwarder>>
      sysmem_service_forwarder_result = fake_display::SysmemServiceForwarder::Create();
  FX_CHECK(sysmem_service_forwarder_result.is_ok());

  fake_display_stack_ = std::make_unique<fake_display::FakeDisplayStack>(
      std::move(sysmem_service_forwarder_result).value(), fake_display_device_config);
}

FakeDisplayCoordinatorConnector::~FakeDisplayCoordinatorConnector() {
  fake_display_stack_->SyncShutdown();
}

void FakeDisplayCoordinatorConnector::OpenCoordinatorWithListenerForPrimary(
    OpenCoordinatorWithListenerForPrimaryRequest& request,
    OpenCoordinatorWithListenerForPrimaryCompleter::Sync& completer) {
  fidl::Arena arena;
  auto provider_request =
      fuchsia_hardware_display::wire::ProviderOpenCoordinatorWithListenerForPrimaryRequest::Builder(
          arena)
          .coordinator(std::move(*request.coordinator()))
          .coordinator_listener(std::move(*request.coordinator_listener()))
          .Build();
  fidl::WireResult<::fuchsia_hardware_display::Provider::OpenCoordinatorWithListenerForPrimary>
      fidl_transport_result =
          fake_display_stack_->display_provider_client()->OpenCoordinatorWithListenerForPrimary(
              provider_request);
  if (!fidl_transport_result.ok()) {
    completer.Close(fidl_transport_result.status());
    return;
  }
  fit::result<zx_status_t> fidl_domain_result = fidl_transport_result.value();
  completer.Reply(fidl_domain_result);
}

void FakeDisplayCoordinatorConnector::OpenCoordinatorWithListenerForVirtcon(
    OpenCoordinatorWithListenerForVirtconRequest& request,
    OpenCoordinatorWithListenerForVirtconCompleter::Sync& completer) {
  fidl::Arena arena;
  auto provider_request =
      fuchsia_hardware_display::wire::ProviderOpenCoordinatorWithListenerForVirtconRequest::Builder(
          arena)
          .coordinator(std::move(*request.coordinator()))
          .coordinator_listener(std::move(*request.coordinator_listener()))
          .Build();
  fidl::WireResult<::fuchsia_hardware_display::Provider::OpenCoordinatorWithListenerForVirtcon>
      fidl_transport_result =
          fake_display_stack_->display_provider_client()->OpenCoordinatorWithListenerForVirtcon(
              provider_request);
  if (!fidl_transport_result.ok()) {
    completer.Close(fidl_transport_result.status());
    return;
  }
  fit::result<zx_status_t> fidl_domain_result = fidl_transport_result.value();
  completer.Reply(fidl_domain_result);
}

}  // namespace display
