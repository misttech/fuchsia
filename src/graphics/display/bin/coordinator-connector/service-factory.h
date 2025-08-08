// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_BIN_COORDINATOR_CONNECTOR_SERVICE_FACTORY_H_
#define SRC_GRAPHICS_DISPLAY_BIN_COORDINATOR_CONNECTOR_SERVICE_FACTORY_H_

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <lib/zx/result.h>

namespace display {

// Provides access to the Coordinator protocol by forwarding the service
// member from fuchsia.hardware.display.Service.
//
// Only provides access to the coordinator as a primary client, but not a
// virtcon client.
//
// TODO(https://fxbug.dev/437141187): Revisit whether we still need this
// class.
class ServiceCoordinatorFactory : public fidl::Server<fuchsia_hardware_display::Provider> {
 public:
  ServiceCoordinatorFactory();
  ~ServiceCoordinatorFactory() override;

  // Disallow copying, moving and assignment.
  ServiceCoordinatorFactory(const ServiceCoordinatorFactory&) = delete;
  ServiceCoordinatorFactory(ServiceCoordinatorFactory&&) = delete;
  ServiceCoordinatorFactory& operator=(const ServiceCoordinatorFactory&) = delete;
  ServiceCoordinatorFactory& operator=(ServiceCoordinatorFactory&&) = delete;

  // `fidl::Server<fuchsia_hardware_display::Provider>`
  void OpenCoordinatorWithListenerForVirtcon(
      OpenCoordinatorWithListenerForVirtconRequest& request,
      OpenCoordinatorWithListenerForVirtconCompleter::Sync& completer) override;
  void OpenCoordinatorWithListenerForPrimary(
      OpenCoordinatorWithListenerForPrimaryRequest& request,
      OpenCoordinatorWithListenerForPrimaryCompleter::Sync& completer) override;

 private:
  // Opens fuchsia.hardware.display.Coordinator service for a primary client
  // using the server end channel `coordinator_server` and listener client end
  // channel `listener_client`.
  //
  // Both `coordinator_server` and `listener_client` must be valid.
  static zx::result<> OpenCoordinatorWithListenerForPrimary(
      fidl::ServerEnd<fuchsia_hardware_display::Coordinator> coordinator_server,
      fidl::ClientEnd<fuchsia_hardware_display::CoordinatorListener> listener_client);
};

}  // namespace display

#endif  // SRC_GRAPHICS_DISPLAY_BIN_COORDINATOR_CONNECTOR_SERVICE_FACTORY_H_
