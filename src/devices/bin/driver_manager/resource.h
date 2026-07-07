// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_DEVICES_BIN_DRIVER_MANAGER_RESOURCE_H_
#define SRC_DEVICES_BIN_DRIVER_MANAGER_RESOURCE_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <lib/fidl/cpp/wire/server.h>
#include <lib/fit/function.h>

#include <memory>
#include <optional>
#include <string>
#include <vector>

namespace driver_manager {

class Node;

// Represents a resource provided by a Node.
class Resource : public fidl::WireServer<fuchsia_driver_framework::ResourceController> {
 public:
  Resource(std::weak_ptr<Node> owner, fuchsia_driver_framework::ResourceArgs args,
           fidl::ServerEnd<fuchsia_driver_framework::ResourceController> server_end,
           async_dispatcher_t* dispatcher);
  ~Resource() override = default;

  // Exposed for testing.
  std::weak_ptr<Node> owner() const { return owner_; }
  const std::string& name() const { return name_; }

 private:
  // fidl::WireServer<fuchsia_driver_framework::ResourceController>
  void Remove(RemoveCompleter::Sync& completer) override;
  void handle_unknown_method(
      fidl::UnknownMethodMetadata<fuchsia_driver_framework::ResourceController> metadata,
      fidl::UnknownMethodCompleter::Sync& completer) override;

  std::string name_;
  std::vector<fuchsia_driver_framework::NodeProperty2> properties_;
  std::vector<fuchsia_driver_framework::Offer> offers_;
  std::optional<fuchsia_driver_framework::BusInfo> bus_info_;

  fidl::ServerBinding<fuchsia_driver_framework::ResourceController> binding_;

  // The node that owns this resource.
  std::weak_ptr<Node> owner_;
};

}  // namespace driver_manager

#endif  // SRC_DEVICES_BIN_DRIVER_MANAGER_RESOURCE_H_
