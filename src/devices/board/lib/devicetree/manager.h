// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BOARD_LIB_DEVICETREE_MANAGER_H_
#define SRC_DEVICES_BOARD_LIB_DEVICETREE_MANAGER_H_

#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#include <lib/devicetree/devicetree.h>
#include <lib/driver2/logger.h>
#include <lib/stdcompat/span.h>
#include <lib/zx/status.h>

#include <unordered_map>
#include <vector>

#include "src/devices/board/lib/devicetree/node.h"

namespace fdf_devicetree {

class Manager {
 public:
  // Create a new device tree manager using the given FDT blob.
  explicit Manager(std::vector<uint8_t> fdt_blob, driver::Logger& logger);

  static zx::status<Manager> CreateFromNamespace(driver::Namespace& ns, driver::Logger& logger);

  using PropertyCallback = fit::function<void(Node* node, devicetree::Property property)>;
  // Add a callback that is called whenever a new property is seen.
  // Must be called before |Discover|.
  void AddPropertyCallback(PropertyCallback cb) { property_callbacks_.emplace_back(std::move(cb)); }

  // Walk the tree, creating nodes and calling any registered property callbacks for each property.
  zx::status<> Discover();

  // Publish the discovered devices.
  // |pbus| should be the platform bus.
  // |parent_node| is the root node of the devicetree. This will eventually be used for housing the
  // metadata nodes.
  // |mgr| is the device group manager.
  zx::status<> PublishDevices(fdf::ClientEnd<fuchsia_hardware_platform_bus::PlatformBus> pbus,
                              fidl::ClientEnd<fuchsia_driver_framework::Node> parent_node,
                              fidl::ClientEnd<fuchsia_driver_framework::DeviceGroupManager> mgr);

  driver::Logger& logger() { return logger_; }

  const std::vector<std::unique_ptr<Node>>& nodes() { return nodes_publish_order_; }

 private:
  // Populates the |nodes_by_phandle_| map.
  void PhandlePropertyCallback(Node* node, devicetree::Property property);
  // Adds bind rules to the node.
  void BindRulePropertyCallback(Node* node, devicetree::Property property);

  std::vector<uint8_t> fdt_blob_;
  devicetree::Devicetree tree_;

  driver::Logger& logger_;

  std::vector<PropertyCallback> property_callbacks_;
  // List of nodes, in the order that they were seen in the tree.
  std::vector<std::unique_ptr<Node>> nodes_publish_order_;
  // Nodes by phandle. Note that not every node in the tree has a phandle.
  std::unordered_map<uint32_t, Node*> nodes_by_phandle_;
  uint32_t node_id_ = 0;
};

}  // namespace fdf_devicetree

#endif  // SRC_DEVICES_BOARD_LIB_DEVICETREE_MANAGER_H_
