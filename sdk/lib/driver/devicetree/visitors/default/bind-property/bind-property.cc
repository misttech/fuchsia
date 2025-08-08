// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include "lib/driver/devicetree/visitors/default/bind-property/bind-property.h"

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/logging/cpp/structured_logger.h>

#include <bind/fuchsia/devicetree/cpp/bind.h>

namespace fdf {
using namespace fuchsia_driver_framework;
}

namespace fdf_devicetree {

constexpr const char kCompatibleProp[] = "compatible";

zx::result<> BindPropertyVisitor::Visit(Node& node, const devicetree::PropertyDecoder& decoder) {
  auto compatible = node.GetProperty<std::vector<std::string>>(kCompatibleProp);
  if (compatible.is_error() && compatible.status_value() != ZX_ERR_NOT_FOUND) {
    FDF_SLOG(WARNING, "Node has invalid compatible property", KV("node_name", node.name()),
             KV("status", compatible.status_string()));
    return compatible.take_error();
  }

  if (!compatible.is_ok() || compatible->empty()) {
    FDF_LOG(DEBUG, "Node '%s' has no compatible property.", node.name().data());
    return zx::ok();
  }

  fdf::NodeProperty2 prop(bind_fuchsia_devicetree::FIRST_COMPATIBLE,
                          fdf::NodePropertyValue::WithStringValue(compatible->front()));

  FDF_LOG(DEBUG, "Added property %s to node '%s'", compatible->front().c_str(),
          node.name().c_str());
  node.AddBindProperty(std::move(prop));

  return zx::ok();
}

}  // namespace fdf_devicetree
