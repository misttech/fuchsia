// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_ALL_DRIVERS_ELEMENT_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_ALL_DRIVERS_ELEMENT_H_

#include <fidl/fuchsia.power.broker/cpp/wire.h>
#include <lib/async/dispatcher.h>
#include <lib/fit/defer.h>

#include <variant>
#include <vector>

#include "src/devices/bin/driver_manager/node.h"

namespace driver_manager {

class DriverRunner;

// AllDriversElement is an ElementRunner server that handles leasing all leaf driver nodes in
// the system.
class AllDriversElement : public fidl::WireServer<fuchsia_power_broker::ElementRunner> {
 public:
  AllDriversElement(DriverRunner* runner, std::shared_ptr<Node> root);

  void SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) override;

  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  void AttachElement(async_dispatcher_t* dispatcher, PowerElementHandles handles);

  // Called by DriverRunner whenever a new node is bound. If the node is a leaf driver instance,
  // it will be added to |leaf_driver_instances_|.
  void OnNodeBound(std::shared_ptr<const Node> node);

 private:
  // Asynchronously acquires a lease for the given node. |deferred| will be executed when the
  // operation completes (and all references to its shared pointer are destroyed).
  void AcquireLease(std::shared_ptr<const Node> node,
                    std::shared_ptr<fit::deferred_callback> deferred);
  void RemoveAncestors(std::shared_ptr<const Node> node);

  fidl::ServerBindingGroup<fuchsia_power_broker::ElementRunner> element_runner_bindings_;
  fidl::Client<fuchsia_power_broker::ElementControl> element_control_;
  fidl::Client<fuchsia_power_broker::Lessor> lessor_;

  // Set to false after the first time the level is set to 1.
  bool first_time_level_1_ = true;

  std::unordered_map<std::string, std::weak_ptr<const Node>> leaf_driver_instances_;

  // Leases are acquired when the level is set to 1 for all current leaf nodes,
  // and when a new leaf node is bound. They are dropped when the level is set to 0,
  // or when a node is no longer a driver-bounded leaf node.
  // The keys in |leases_| must match with |leaf_driver_instances_|.
  std::unordered_map<std::string, zx::eventpair> leases_;

  uint32_t current_level_ = 0;

  // Owns this AllDriversElement.
  DriverRunner* driver_runner_;
};

}  // namespace driver_manager

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_ALL_DRIVERS_ELEMENT_H_
