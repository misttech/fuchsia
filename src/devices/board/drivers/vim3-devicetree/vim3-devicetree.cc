// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/drivers/vim3-devicetree/vim3-devicetree.h"

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <lib/driver/component/cpp/driver_export.h>
#include <lib/driver/devicetree/manager/manager.h>
#include <lib/driver/devicetree/manager/publisher-dev.h>
#include <lib/driver/devicetree/visitors/load-visitors.h>
#include <lib/driver/logging/cpp/logger.h>

#include "visitors/vim3-adc-buttons.h"
#include "visitors/vim3-gpio-buttons.h"
#include "visitors/vim3-nna.h"
#include "visitors/vim3-wifi.h"

namespace vim3_dt {

zx::result<> Vim3Devicetree::Start() {
  node_.Bind(std::move(node()));

  zx::result manager = fdf_devicetree::Manager::CreateFromNamespace(*incoming());
  if (manager.is_error()) {
    fdf::error("Failed to create devicetree manager: {}", manager.error_value());
    return manager.take_error();
  }

  auto visitors = fdf_devicetree::LoadVisitors(symbols());
  if (visitors.is_error()) {
    fdf::error("Failed to create visitors: {}", visitors.status_string());
    return visitors.take_error();
  }

  // Insert visitors with workarounds for vim3.
  if (zx::result result = (*visitors)->RegisterVisitor<Vim3AdcButtonsVisitor>();
      result.is_error()) {
    fdf::error("Failed to register vim3 adc buttons visitor");
    return result.take_error();
  };

  if (zx::result result = (*visitors)->RegisterVisitor<Vim3GpioButtonsVisitor>();
      result.is_error()) {
    fdf::error("Failed to register vim3 gpio buttons visitor");
    return result.take_error();
  };

  if (zx::result result = (*visitors)->RegisterVisitor<Vim3WifiVisitor>(); result.is_error()) {
    fdf::error("Failed to register vim3 wifi visitor");
    return result.take_error();
  };

  if (zx::result result = (*visitors)->RegisterVisitor<Vim3NnaVisitor>(); result.is_error()) {
    fdf::error("Failed to register vim3 nna visitor");
    return result.take_error();
  };

  zx::result<> status = manager->Walk(*(visitors.value()));
  if (status.is_error()) {
    fdf::error("Failed to walk the device tree: {}", status.status_string());
    return status.take_error();
  }

  zx::result pbus = incoming()->Connect<fuchsia_hardware_platform_bus::Service::PlatformBus>();
  if (pbus.is_error() || !pbus->is_valid()) {
    fdf::error("Failed to connect to pbus: {}", pbus);
    return pbus.take_error();
  }

  zx::result group_manager = incoming()->Connect<fuchsia_driver_framework::CompositeNodeManager>();
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

}  // namespace vim3_dt

FUCHSIA_DRIVER_EXPORT(vim3_dt::Vim3Devicetree);
