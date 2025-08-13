// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_GRAPHICS_DISPLAY_TESTING_FAKE_COORDINATOR_CONNECTOR_SERVICE_H_
#define SRC_GRAPHICS_DISPLAY_TESTING_FAKE_COORDINATOR_CONNECTOR_SERVICE_H_

#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <lib/component/outgoing/cpp/outgoing_directory.h>
#include <lib/driver/testing/cpp/scoped_global_logger.h>
#include <lib/fidl/cpp/wire/channel.h>
#include <lib/fit/function.h>
#include <lib/zx/result.h>

#include <memory>
#include <queue>

#include "src/graphics/display/drivers/fake/fake-display-stack.h"

namespace display {

// Connects clients to a fake display-coordinator device with a fake-display
// display engine.
//
// FakeDisplayCoordinatorConnector is not thread-safe. All public methods must
// be invoked on a single-threaded event loop with the same `dispatcher`
// provided on FakeDisplayCoordinatorConnector creation.
class FakeDisplayCoordinatorConnector : public fidl::Server<fuchsia_hardware_display::Provider> {
 public:
  // Creates a FakeDisplayCoordinatorConnector where the fake display driver
  // is initialized using `fake_display_device_config`.
  // Callers are responsible for binding incoming FIDL clients to it.
  explicit FakeDisplayCoordinatorConnector(
      const fake_display::FakeDisplayDeviceConfig& fake_display_device_config);
  ~FakeDisplayCoordinatorConnector() override;

  // Disallow copy, assign and move.
  FakeDisplayCoordinatorConnector(const FakeDisplayCoordinatorConnector&) = delete;
  FakeDisplayCoordinatorConnector(FakeDisplayCoordinatorConnector&&) = delete;
  FakeDisplayCoordinatorConnector operator=(const FakeDisplayCoordinatorConnector&) = delete;
  FakeDisplayCoordinatorConnector operator=(FakeDisplayCoordinatorConnector&&) = delete;

  // `fidl::Server<fuchsia_hardware_display::Provider>`
  void OpenCoordinatorWithListenerForVirtcon(
      OpenCoordinatorWithListenerForVirtconRequest& request,
      OpenCoordinatorWithListenerForVirtconCompleter::Sync& completer) override;
  void OpenCoordinatorWithListenerForPrimary(
      OpenCoordinatorWithListenerForPrimaryRequest& request,
      OpenCoordinatorWithListenerForPrimaryCompleter::Sync& completer) override;

 private:
  fdf_testing::ScopedGlobalLogger logger_;
  std::unique_ptr<fake_display::FakeDisplayStack> fake_display_stack_;
};

}  // namespace display

#endif  // SRC_GRAPHICS_DISPLAY_TESTING_FAKE_COORDINATOR_CONNECTOR_SERVICE_H_
