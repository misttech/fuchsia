// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "usb-connector-visitor.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_properties.h>
#include <lib/driver/devicetree/visitors/common-types.h>
#include <lib/driver/devicetree/visitors/registration.h>
#include <lib/driver/logging/cpp/logger.h>

#include <string>
#include <vector>

#include <bind/fuchsia/cpp/bind.h>

namespace usb_connector_visitor_dt {

UsbConnectorVisitor::UsbConnectorVisitor() {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(
      std::make_unique<fdf_devicetree::ReferenceProperty>(kUsbConnector, 0u, /* required */ false));
  properties.emplace_back(std::make_unique<fdf_devicetree::StringListProperty>(
      kUsbConnectorNames, /* required */ false));
  parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

zx::result<> UsbConnectorVisitor::Visit(fdf_devicetree::Node& node,
                                        const devicetree::PropertyDecoder& decoder) {
  zx::result parser_output = parser_->Parse(node);
  if (parser_output.is_error()) {
    fdf::error("USB connector visitor failed for node '{}' : {}", node.name(), parser_output);
    return parser_output.take_error();
  }

  std::optional<fdf_devicetree::References> connectors =
      parser_output->Get<fdf_devicetree::References>(kUsbConnector);
  if (!connectors.has_value() || connectors->empty()) {
    return zx::ok();
  }

  std::optional<std::vector<std::string>> connector_names =
      parser_output->Get<std::vector<std::string>>(kUsbConnectorNames);
  if (!connector_names && connectors->size() != 1u) {
    fdf::error(
        "Node '{}' references {} USB connectors but has no '{}' property. A name is required to distinguish the bind parents.",
        node.name(), connectors->size(), kUsbConnectorNames);
    return zx::error(ZX_ERR_INVALID_ARGS);
  }
  if (connector_names && connector_names->size() != connectors->size()) {
    fdf::error("Node '{}' has {} '{}' entries but references {} USB connectors.", node.name(),
               connector_names->size(), kUsbConnectorNames, connectors->size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  for (size_t index = 0; index < connectors->size(); index++) {
    auto& connector = (*connectors)[index];
    if (!connector.reference_node()) {
      fdf::error("Node '{}' has an invalid '{}' reference.", node.name(), kUsbConnector);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
    std::string_view parent_name =
        connector_names ? std::string_view((*connector_names)[index]) : kDefaultConnectorName;
    if (zx::result result = ParseReferenceChild(node, connector.reference_node(), parent_name);
        result.is_error()) {
      return result.take_error();
    }
  }
  return zx::ok();
}

zx::result<> UsbConnectorVisitor::ParseReferenceChild(fdf_devicetree::Node& child,
                                                      fdf_devicetree::ReferenceNode& connector,
                                                      std::string_view parent_name) {
  static constexpr std::string_view kService = "fuchsia.hardware.usb.connector.Service";

  std::string_view output_name = kDefaultConnectorName;
  zx::result name = connector.GetProperty<std::string>(kUsbConnectorOutputName);
  if (name.is_ok()) {
    output_name = *name;
  } else if (name.error_value() != ZX_ERR_NOT_FOUND) {
    fdf::error("USB connector node '{}' has an invalid '{}' property: {}", connector.name(),
               kUsbConnectorOutputName, name);
    return name.take_error();
  }

  std::vector bind_rules = {
      fdf::MakeAcceptBindRule(bind_fuchsia::SERVICE, kService),
      fdf::MakeAcceptBindRule(bind_fuchsia::NAME, output_name),
  };
  std::vector bind_properties = {
      fdf::MakeProperty2(bind_fuchsia::SERVICE, kService),
      fdf::MakeProperty2(bind_fuchsia::NAME, parent_name),
  };

  child.AddNodeSpec(fuchsia_driver_framework::ParentSpec2{{
      .bind_rules = std::move(bind_rules),
      .properties = std::move(bind_properties),
  }});
  fdf::debug("Added USB connector parent '{}' ('{}') to node '{}'.", connector.name(), parent_name,
             child.name());
  return zx::ok();
}

}  // namespace usb_connector_visitor_dt

REGISTER_DEVICETREE_VISITOR(usb_connector_visitor_dt::UsbConnectorVisitor);
