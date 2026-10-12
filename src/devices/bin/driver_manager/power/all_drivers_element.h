// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_

#include <fidl/fuchsia.power.broker/cpp/wire.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/defer.h>

#include <memory>

#include "src/devices/bin/driver_manager/node.h"

namespace driver_manager {

class PowerManager;

// AllDriversElement is an ElementRunner server that handles leasing all leaf driver nodes in
// the system.
class AllDriversElement : public fidl::WireServer<fuchsia_power_broker::ElementRunner>,
                          public std::enable_shared_from_this<AllDriversElement> {
 public:
  friend class AllDriversElementTest;

  AllDriversElement(PowerManager* power_manager, std::shared_ptr<Node> root);

  void SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  void AttachElement(async_dispatcher_t* dispatcher, PowerElementHandles handles);

  // Called by DriverRunner whenever a new node is bound. If the node is a leaf driver, adds a
  // power dependency on the node to the AllDrivers element, passing the startup lease to be held
  // until the dependency addition completes.
  void OnNodeBound(std::shared_ptr<const Node> node);

 private:
  // Traverses the topology bottom-up (post-order) to identify leaf driver nodes and adds
  // power dependencies on them, passing their startup leases to be dropped once the dependencies
  // are added. Should only be called once when the system first switches to the "on" level.
  void CompleteStartupTransition();

  // Processes startup transition logic for a single |node|:
  // If the node is not bound, does nothing.
  // Otherwise, retrieves the node's startup lease.
  //   - If |has_bound_descendant| is false (the node is a leaf driver instance), adds a power
  //     dependency on the node and passes the startup lease.
  //   - If |has_bound_descendant| is true, the node is not a leaf; its startup lease is dropped.
  void ProcessStartupNode(const std::shared_ptr<const Node>& node, bool has_bound_descendant);

  // Adds a power dependency on a leaf driver node to the AllDrivers element.
  // The optional |startup_lease| is held until the AddDependency call completes asynchronously,
  // preventing the node from dropping power before the dependency is established.
  // Adding a dependency that already exists is harmless: Power Broker rejects it with
  // ALREADY_EXISTS, which is ignored.
  void AddDependency(const std::shared_ptr<const Node>& node,
                     std::optional<zx::eventpair> startup_lease = std::nullopt);

  fidl::ServerBindingGroup<fuchsia_power_broker::ElementRunner> element_runner_bindings_;
  fidl::Client<fuchsia_power_broker::ElementControl> element_control_;
  fidl::Client<fuchsia_power_broker::Lessor> lessor_;

  // The current power level of this element. 0 represents low power, 1 represents the active/on
  // state.
  uint32_t current_level_ = 0;

  // Set to true when the element first transitions to the "on" level (level 1), marking the
  // completion of the initial startup transition.
  bool transition_complete_ = false;

  // A weak pointer to the root of the node topology, used to discover all nodes during the
  // startup transition.
  std::weak_ptr<Node> root_;

  // Owns this AllDriversElement.
  PowerManager* power_manager_;
};

}  // namespace driver_manager

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_POWER_ALL_DRIVERS_ELEMENT_H_
