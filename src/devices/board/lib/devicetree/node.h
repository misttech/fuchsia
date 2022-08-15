// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BOARD_LIB_DEVICETREE_NODE_H_
#define SRC_DEVICES_BOARD_LIB_DEVICETREE_NODE_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <fidl/fuchsia.driver.framework/cpp/markers.h>
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <lib/devicetree/devicetree.h>
#include <lib/driver2/logger.h>

#include <utility>

#include <bind/fuchsia/platform/cpp/bind.h>

namespace fdf_devicetree {

class Node {
 public:
  explicit Node(Node* parent, const std::string_view name, devicetree::Properties properties,
                uint32_t id)
      : parent_(parent), name_(name), properties_(std::move(properties)), id_(id) {
    pbus_node_.did() = bind_fuchsia_platform::BIND_PLATFORM_DEV_DID_DEVICETREE;
    pbus_node_.vid() = bind_fuchsia_platform::BIND_PLATFORM_DEV_VID_GENERIC;
    pbus_node_.instance_id() = id;
    pbus_node_.name() = std::string(name_);
  }

  Node* parent() { return parent_; }

  // Add |prop| as a bind property of the device, when it is eventually published.
  void AddBindProperty(fuchsia_driver_framework::NodeProperty prop);

  // Publish this node.
  zx::status<> Publish(driver::Logger& logger,
                       fdf::WireSyncClient<fuchsia_hardware_platform_bus::PlatformBus>& pbus,
                       fidl::SyncClient<fuchsia_driver_framework::Node>& root_node_parent,
                       fidl::SyncClient<fuchsia_driver_framework::DeviceGroupManager>& mgr);

  std::string_view name() { return name_; }

 private:
  Node* parent_;
  std::vector<fuchsia_driver_framework::NodeProperty> bind_properties_;

  const std::string_view name_;
  devicetree::Properties properties_;

  // Our platform bus node.
  fuchsia_hardware_platform_bus::Node pbus_node_;

  // Properties of the nodes after they have been transformed in the device group.
  std::vector<fuchsia_driver_framework::NodeProperty> node_properties_;

  // Our other device group nodes.
  std::vector<fuchsia_driver_framework::DeviceGroupNode> device_group_nodes_;

  // This is a unique ID we use to match our device group with the correct
  // platform bus node. It is generated at runtime and not stable across boots.
  uint32_t id_;
};

}  // namespace fdf_devicetree

#endif  // SRC_DEVICES_BOARD_LIB_DEVICETREE_NODE_H_
