// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/drivers/vim3-devicetree/vim3.h"

#include <fidl/fuchsia.boot/cpp/fidl.h>
#include <lib/driver2/driver_base.h>
#include <zircon/boot/image.h>

#include "fidl/fuchsia.driver.framework/cpp/markers.h"
#include "fidl/fuchsia.hardware.platform.bus/cpp/driver/wire_messaging.h"
#include "lib/driver2/service_client.h"

namespace vim3_dt {

zx::status<> Vim3Devicetree::Start() {
  FDF_LOG(INFO, "Hello there!");

  auto manager = fdf_devicetree::Manager::CreateFromNamespace(*context().incoming(), logger());
  if (manager.is_error()) {
    return manager.take_error();
  }

  manager_.emplace(std::move(*manager));

  auto status = manager_->Discover();
  if (status.is_error()) {
    FDF_LOG(ERROR, "Failed to discover devices: %s", status.status_string());
    return status.take_error();
  }

  auto pbus =
      driver::Connect<fuchsia_hardware_platform_bus::Service::PlatformBus>(*context().incoming());
  if (pbus.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to pbus: %s", pbus.status_string());
    return pbus.take_error();
  }

  auto group_manager =
      context().incoming()->Connect<fuchsia_driver_framework::DeviceGroupManager>();
  if (group_manager.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to device group manager: %s", group_manager.status_string());
    return group_manager.take_error();
  }

  status = manager_->PublishDevices(std::move(*pbus), std::move(node()), std::move(*group_manager));
  if (status.is_error()) {
    FDF_LOG(ERROR, "Failed to publish devices: %s", status.status_string());
    return status.take_error();
  }

  FDF_LOG(INFO, "Vim3 driver has added itself!");
  return zx::ok();
}

void Vim3Devicetree::Stop() { FDF_LOG(INFO, "Vim3 driver is being unloaded"); }

}  // namespace vim3_dt

FUCHSIA_DRIVER_RECORD_CPP_V2(driver::Record<vim3_dt::Vim3Devicetree>);
