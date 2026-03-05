// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "ufs-phy-visitor.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/component/cpp/node_properties.h>
#include <lib/driver/devicetree/visitors/registration.h>
#include <lib/driver/logging/cpp/logger.h>

#include <bind/fuchsia/hardware/ufs/phy/cpp/bind.h>

namespace ufs_phy_visitor_dt {

UfsPhyVisitor::UfsPhyVisitor() {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(
      std::make_unique<fdf_devicetree::ReferenceProperty>(kPhys, kPhyCells, /* required */ false));
  properties.emplace_back(
      std::make_unique<fdf_devicetree::StringListProperty>(kPhyNames, /* required */ false));
  parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

bool UfsPhyVisitor::is_match(const std::string& name) {
  return name.find("ufs-phy") != std::string::npos;
}

zx::result<> UfsPhyVisitor::Visit(fdf_devicetree::Node& node,
                                  const devicetree::PropertyDecoder& decoder) {
  zx::result parser_output = parser_->Parse(node);
  if (parser_output.is_error()) {
    FDF_LOG(ERROR, "Ufs visitor parse failed for node '%s' : %s", node.name().c_str(),
            parser_output.status_string());
    return parser_output.take_error();
  }

  auto phys = parser_output->Get<fdf_devicetree::References>(kPhys);
  if (!phys) {
    return zx::ok();
  }

  auto phy_names = parser_output->Get<std::vector<std::string>>(kPhyNames);
  if (phy_names) {
    if (phy_names->size() != phys->size()) {
      FDF_LOG(ERROR,
              "Node '%s' does not have required number of phy-names. Expected (%zu), actual (%zu).",
              node.name().c_str(), phys->size(), phy_names->size());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
  } else {
    if (phys->size() > 1) {
      FDF_LOG(ERROR, "Node '%s' has more than one phy but is missing phy-names.",
              node.name().c_str());
      return zx::error(ZX_ERR_INVALID_ARGS);
    }
  }

  for (uint32_t index = 0; index < phys->size(); index++) {
    auto& reference = (*phys)[index];
    if (!reference.reference_node()) {
      FDF_LOG(ERROR, "Node '%s' has invalid phy reference at %d index.", node.name().c_str(),
              index);
      return zx::error(ZX_ERR_INVALID_ARGS);
    }

    if (!is_match(reference.reference_node().name())) {
      continue;
    }

    auto result = AddChildNodeSpec(node, phy_names ? (*phy_names)[index] : "");
    if (result.is_error()) {
      return result.take_error();
    }
  }

  return zx::ok();
}

zx::result<> UfsPhyVisitor::AddChildNodeSpec(fdf_devicetree::Node& child,
                                             std::string_view phy_name) {
  std::vector bind_rules = {
      fdf::MakeAcceptBindRule2(bind_fuchsia_hardware_ufs_phy::SERVICE,
                               bind_fuchsia_hardware_ufs_phy::SERVICE_ZIRCONTRANSPORT),
  };
  std::vector bind_properties = {
      fdf::MakeProperty2(bind_fuchsia_hardware_ufs_phy::SERVICE,
                         bind_fuchsia_hardware_ufs_phy::SERVICE_ZIRCONTRANSPORT),
  };

  auto phy_node = fuchsia_driver_framework::ParentSpec2{{bind_rules, bind_properties}};

  child.AddNodeSpec(phy_node);

  FDF_LOG(DEBUG, "Added '%s' bind rules to node '%s'.", std::string(phy_name).c_str(),
          child.name().c_str());
  return zx::ok();
}

}  // namespace ufs_phy_visitor_dt

REGISTER_DEVICETREE_VISITOR(ufs_phy_visitor_dt::UfsPhyVisitor);
