// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/board/lib/devicetree/node.h"

#include <fidl/fuchsia.driver.framework/cpp/natural_types.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>

#include <bind/fuchsia/platform/cpp/bind.h>
#include <sdk/lib/driver/legacy-bind-constants/legacy-bind-constants.h>

namespace fdf {
using namespace fuchsia_driver_framework;
}

namespace fdf_devicetree {

void Node::AddBindProperty(fuchsia_driver_framework::NodeProperty prop) {
  node_properties_.emplace_back(std::move(prop));
}

zx::status<> Node::Publish(driver::Logger &logger,
                           fdf::WireSyncClient<fuchsia_hardware_platform_bus::PlatformBus> &pbus,
                           fidl::SyncClient<fuchsia_driver_framework::Node> &root_node_parent,
                           fidl::SyncClient<fuchsia_driver_framework::DeviceGroupManager> &mgr) {
  if (node_properties_.empty()) {
    FDF_LOGL(DEBUG, logger, "Not publishing node '%.*s' because it has no bind properties.",
             (int)name().size(), name().data());
    return zx::ok();
  }
  fdf::Arena arena('PBUS');
  fidl::Arena fidl_arena;
  auto result = pbus.buffer(arena)->NodeAdd(fidl::ToWire(fidl_arena, pbus_node_));
  if (!result.ok()) {
    FDF_LOGL(ERROR, logger, "NodeAdd request failed: %s", result.FormatDescription().data());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    FDF_LOGL(ERROR, logger, "NodeAdd failed: %s", zx_status_get_string(result->error_value()));
    return zx::error(result->error_value());
  }

  // Construct the platform bus node.
  fdf::DeviceGroupNode platform_node;
  platform_node.bind_properties() = std::vector<fdf::NodeProperty>(node_properties_);
  platform_node.bind_properties().emplace_back(fdf::NodeProperty{{
      .key = fdf::NodePropertyKey::WithIntValue(BIND_PROTOCOL),
      .value = fdf::NodePropertyValue::WithIntValue(bind_fuchsia_platform::BIND_PROTOCOL_DEVICE),
  }});
  platform_node.bind_rules() = std::vector<fdf::BindRule>{
      fdf::BindRule{{
          .key = fdf::NodePropertyKey::WithIntValue(BIND_PLATFORM_DEV_VID),
          .condition = fdf::Condition::kAccept,
          .values = {fdf::NodePropertyValue::WithIntValue(
              bind_fuchsia_platform::BIND_PLATFORM_DEV_VID_GENERIC)},
      }},
      fdf::BindRule{{
          .key = fdf::NodePropertyKey::WithIntValue(BIND_PLATFORM_DEV_DID),
          .condition = fdf::Condition::kAccept,
          .values = {fdf::NodePropertyValue::WithIntValue(
              bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_DEVICETREE)},
      }},
      fdf::BindRule{{
          .key = fdf::NodePropertyKey::WithIntValue(BIND_PLATFORM_DEV_INSTANCE_ID),
          .condition = fdf::Condition::kAccept,
          .values = {fdf::NodePropertyValue::WithIntValue(id_)},
      }},
  };

  // pbus node is always the primary parent for now.
  device_group_nodes_.insert(device_group_nodes_.begin(), std::move(platform_node));

  fdf::DeviceGroup group;
  group.topological_path() = name();
  group.nodes() = std::move(device_group_nodes_);
  auto devicegroup_result = mgr->CreateDeviceGroup({std::move(group)});
  if (devicegroup_result.is_error()) {
    FDF_LOGL(ERROR, logger, "Failed to create device group: %s",
             devicegroup_result.error_value().FormatDescription().data());
    return zx::error(devicegroup_result.error_value().is_framework_error()
                         ? devicegroup_result.error_value().framework_error().status()
                         : ZX_ERR_INVALID_ARGS);
  }

  return zx::ok();
}

}  // namespace fdf_devicetree
