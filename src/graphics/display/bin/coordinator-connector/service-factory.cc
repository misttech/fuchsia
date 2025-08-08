// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/graphics/display/bin/coordinator-connector/service-factory.h"

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <lib/component/incoming/cpp/service_member_watcher.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/result.h>
#include <zircon/status.h>

namespace display {

ServiceCoordinatorFactory::ServiceCoordinatorFactory() = default;

ServiceCoordinatorFactory::~ServiceCoordinatorFactory() = default;

// static
zx::result<> ServiceCoordinatorFactory::OpenCoordinatorWithListenerForPrimary(
    fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server,
    fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener> listener_client) {
  // Watch for the first available fuchsia.hardware.display.Service instance.
  component::SyncServiceMemberWatcher<fuchsia_hardware_display::Service::Provider> watcher;
  zx::result<fidl::ClientEnd<fuchsia_hardware_display::Provider>> provider_result =
      watcher.GetNextInstance(/*stop_at_idle=*/ false);
  if (provider_result.is_error()) {
    FX_PLOGS(ERROR, provider_result.error_value())
        << "Failed to open display Provider provided by fuchsia.hardware.display/Service";

    // We could try to match the value of the C "errno" macro to the closest ZX error, but
    // this would give rise to many corner cases.  We never expect this to fail anyway, since
    // |filename| is given to us by the device watcher.
    return zx::error(ZX_ERR_INTERNAL);
  }

  fidl::ClientEnd<fuchsia_hardware_display::Provider> provider = std::move(provider_result).value();

  // TODO(https://fxbug.dev/42135096): Pass an async completer asynchronously into
  // OpenCoordinator(), rather than blocking on a synchronous call.
  fidl::Arena arena;
  auto request =
      fuchsia_hardware_display::wire::ProviderOpenCoordinatorWithListenerForPrimaryRequest::Builder(
          arena)
          .coordinator(std::move(coordinator_server))
          .coordinator_listener(std::move(listener_client))
          .Build();
  fidl::WireResult result =
      fidl::WireCall(provider)->OpenCoordinatorWithListenerForPrimary(std::move(request));
  if (!result.ok()) {
    FX_PLOGS(ERROR, result.status()) << "Failed to call service handle";

    // There's not a clearly-better value to return here.  Returning the FIDL error would be
    // somewhat unexpected, since the caller wouldn't receive it as a FIDL status, rather as
    // the return value of a "successful" method invocation.
    return zx::error(ZX_ERR_INTERNAL);
  }
  if (result.value().is_error()) {
    FX_PLOGS(ERROR, result.value().error_value()) << "Failed to open display coordinator";
    return zx::error(result.value().error_value());
  }

  return zx::ok();
}

void ServiceCoordinatorFactory::OpenCoordinatorWithListenerForVirtcon(
    OpenCoordinatorWithListenerForVirtconRequest& request,
    OpenCoordinatorWithListenerForVirtconCompleter::Sync& completer) {
  completer.Reply(fit::error(ZX_ERR_NOT_SUPPORTED));
}

void ServiceCoordinatorFactory::OpenCoordinatorWithListenerForPrimary(
    OpenCoordinatorWithListenerForPrimaryRequest& request,
    OpenCoordinatorWithListenerForPrimaryCompleter::Sync& completer) {
  zx::result<> open_coordinator_status = OpenCoordinatorWithListenerForPrimary(
      std::move(*request.coordinator()), std::move(*request.coordinator_listener()));
  completer.Reply(open_coordinator_status);
}

}  // namespace display
