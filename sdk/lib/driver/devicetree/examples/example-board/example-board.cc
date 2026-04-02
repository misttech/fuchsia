// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sdk/lib/driver/devicetree/examples/example-board/example-board.h"

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/devicetree/manager/publisher-dev.h>
#include <lib/driver/devicetree/visitors/load-visitors.h>
#include <lib/driver/logging/cpp/logger.h>

namespace example_board {

zx::result<> ExampleBoard::Start() {
  node_.Bind(std::move(node()));

  auto manager = fdf_devicetree::Manager::CreateFromNamespace(*incoming());
  if (manager.is_error()) {
    fdf::error("Failed to create devicetree manager: {}", manager);

    return manager.take_error();
  }

  auto visitors = fdf_devicetree::LoadVisitors(symbols());
  if (visitors.is_error()) {
    fdf::error("Failed to create visitors: {}", visitors);

    return visitors.take_error();
  }

  auto status = manager->Walk(*(visitors.value()));
  if (status.is_error()) {
    fdf::error("Failed to walk the device tree: {}", status);

    return status.take_error();
  }

  auto pbus = incoming()->Connect<fuchsia_hardware_platform_bus::Service::PlatformBus>();
  if (pbus.is_error() || !pbus->is_valid()) {
    fdf::error("Failed to connect to pbus: {}", pbus);

    return pbus.take_error();
  }

  auto group_manager = incoming()->Connect<fuchsia_driver_framework::CompositeNodeManager>();
  if (group_manager.is_error()) {
    fdf::error("Failed to connect to device group manager: {}", group_manager);

    return group_manager.take_error();
  }

  auto pbus_client = fdf::WireSyncClient(std::move(pbus.value()));
  auto mgr_client = fidl::SyncClient(std::move(group_manager.value()));
  fdf_devicetree::PublisherDev publisher(pbus_client, mgr_client, node_);
  status = manager->PublishDevices(publisher);
  if (status.is_error()) {
    fdf::error("Failed to publish devices: {}", status);

    return status.take_error();
  }

  return zx::ok();
}

}  // namespace example_board

FUCHSIA_DRIVER_EXPORT(example_board::ExampleBoard);
