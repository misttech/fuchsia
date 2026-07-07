// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/resource.h"

#include <zircon/assert.h>

#include "src/devices/bin/driver_manager/node.h"
#include "src/devices/lib/log/log.h"

namespace driver_manager {

Resource::Resource(std::weak_ptr<Node> owner, fuchsia_driver_framework::ResourceArgs args,
                   fidl::ServerEnd<fuchsia_driver_framework::ResourceController> server_end,
                   async_dispatcher_t* dispatcher)
    : binding_(dispatcher, std::move(server_end), this,
               [](Resource* resource, fidl::UnbindInfo info) {
                 if (auto owner = resource->owner_.lock()) {
                   owner->RemoveResource(resource);
                 }
               }),
      owner_(std::move(owner)) {
  ZX_ASSERT_MSG(
      args.name().has_value() && args.properties().has_value() && args.offers().has_value(),
      "ResourceArgs must contain name, properties, and offers");

  name_ = std::move(args.name().value());
  properties_ = std::move(args.properties().value());
  offers_ = std::move(args.offers().value());
  bus_info_ = std::move(args.bus_info());
}

void Resource::Remove(RemoveCompleter::Sync& completer) { binding_.Close(ZX_OK); }

void Resource::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_driver_framework::ResourceController> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf_log::warn("ResourceController received unknown method. Ordinal: {}", metadata.method_ordinal);
}

}  // namespace driver_manager
