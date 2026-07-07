// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/resource.h"

#include <lib/async/cpp/task.h>
#include <zircon/assert.h>

#include "src/devices/bin/driver_manager/node.h"
#include "src/devices/lib/log/log.h"

namespace driver_manager {

Resource::Resource(ResourceId id, std::weak_ptr<Node> owner, std::string name,
                   std::vector<fuchsia_driver_framework::NodeProperty2> properties,
                   std::vector<NodeOffer> node_offers,
                   std::optional<fuchsia_driver_framework::BusInfo> bus_info,
                   async_dispatcher_t* dispatcher)
    : id_(id),
      name_(std::move(name)),
      properties_(std::move(properties)),
      offers_(std::move(node_offers)),
      bus_info_(std::move(bus_info)),
      owner_(std::move(owner)),
      dispatcher_(dispatcher) {}

void Resource::Bind(fidl::ServerEnd<fuchsia_driver_framework::ResourceController> server_end) {
  if (server_end.is_valid()) {
    binding_.emplace(dispatcher_, std::move(server_end), this,
                     [self = weak_from_this()](Resource* resource, fidl::UnbindInfo info) {
                       if (auto shared_self = self.lock()) {
                         if (auto owner = shared_self->owner_.lock()) {
                           owner->RemoveResource(shared_self);
                         }
                       }
                     });
  }
}

void Resource::Remove(RemoveCompleter::Sync& completer) {
  if (auto owner = owner_.lock()) {
    owner->RemoveResource(shared_from_this());
  }
}

void Resource::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_framework::ResourceController> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf_log::warn("ResourceController received unknown method. Ordinal: {}", metadata.method_ordinal);
}

}  // namespace driver_manager
