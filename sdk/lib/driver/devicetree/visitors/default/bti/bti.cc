// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include "lib/driver/devicetree/visitors/default/bti/bti.h"

#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/devicetree/devicetree.h>
#include <lib/driver/devicetree/visitors/common-types.h>
#include <lib/driver/logging/cpp/logger.h>

#include <cstdint>
#include <optional>

namespace fdf_devicetree {

namespace {

constexpr const char kBtiProp[] = "iommus";
constexpr const char kBtiNameProp[] = "iommu-names";
constexpr const char kIommuCellsProp[] = "#iommu-cells";
constexpr const char kRegProp[] = "reg";

// #iommu-cells == 1 (<id>) or 2 (<id mask>).
class IommuCell {
 public:
  explicit IommuCell(PropertyCells cells) : property_array_(cells) {}

  uint32_t bti_id() const { return property_array_[0]; }

  std::optional<uint32_t> bti_id_mask() const {
    if (property_array_.size() < 2) {
      return std::nullopt;
    }
    return property_array_[1];
  }

 private:
  Uint32Array property_array_;
};

}  // namespace

BtiVisitor::BtiVisitor() {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kBtiProp, kIommuCellsProp, /* required */ false));
  properties.emplace_back(
      std::make_unique<fdf_devicetree::StringListProperty>(kBtiNameProp, /* required */ false));
  reference_parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

zx::result<> BtiVisitor::Visit(Node& node, const devicetree::PropertyDecoder& decoder) {
  if (IsIommu(node.name())) {
    const auto property = node.properties().find(kRegProp);
    if (property != node.properties().end()) {
      const auto reg_props = property->second.AsReg(decoder);
      if (!reg_props || reg_props->size() == 0) {
        fdf::error("IOMMU node '{}' has invalid reg property.", node.name());
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
      const std::optional<uint64_t> reg_address = (*reg_props)[0].address();
      if (!reg_address) {
        fdf::error("IOMMU node '{}' has invalid reg address.", node.name());
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
      const std::optional<uint64_t> base_address = decoder.TranslateAddress(*reg_address);
      if (!base_address) {
        fdf::error("IOMMU node '{}' failed to translate reg base address.", node.name());
        return zx::error(ZX_ERR_INVALID_ARGS);
      }
      iommu_base_addresses_[node.id()] = *base_address;
    }
  }

  zx::result parser_output = reference_parser_->Parse(node);
  if (parser_output.is_error()) {
    fdf::error("Failed to parse reference for node '{}'", node.name());

    return parser_output.take_error();
  }

  auto btis = parser_output->Get<References>(kBtiProp);
  if (!btis) {
    return zx::ok();
  }

  auto iommu_names = parser_output->Get<std::vector<std::string>>(kBtiNameProp);
  if (iommu_names && iommu_names->size() > btis->size()) {
    fdf::error("Node '{}' has {} iommu entries but has {} iommu names.", node.name(), btis->size(),
               iommu_names->size());

    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  for (uint32_t index = 0; index < btis->size(); index++) {
    auto& reference = (*btis)[index];
    if (IsIommu(reference.reference_node().name())) {
      std::optional<std::string> name;
      if (iommu_names && index < iommu_names->size()) {
        name = (*iommu_names)[index];
      }
      auto result =
          ReferenceChildVisit(node, reference.reference_node(), reference.property_cells(), name);
      if (result.is_error()) {
        return result.take_error();
      }
    } else {
      fdf::warn("Node {} iommu references illegal parent: {}", node.name(),
                reference.reference_node().name());
    }
  }

  return zx::ok();
}

zx::result<> BtiVisitor::ReferenceChildVisit(Node& child, ReferenceNode& parent,
                                             PropertyCells reference_cells,
                                             std::optional<std::string> iommu_name) {
  const size_t cell_count = reference_cells.size_bytes() / sizeof(uint32_t);
  if (cell_count != 1 && cell_count != 2) {
    fdf::error("Incorrect number of iommu cells (expected 1 or 2, found {}) for node '{}'.",
               cell_count, child.name());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  const uint32_t iommu_id = parent.id();
  iommu_nodes_.insert(iommu_id);

  const IommuCell iommu_cell(reference_cells);

  fuchsia_hardware_platform_bus::Bti bti = {{
      .iommu_id = iommu_id,
      .bti_id = iommu_cell.bti_id(),
      .name = std::move(iommu_name),
      .bti_id_mask = iommu_cell.bti_id_mask(),
  }};
  fdf::debug("BTI {} index: {:#x}, id: {:#x}, mask: {:#x}, added to node '{}'.",
             bti.name().value_or("(no name)"), *bti.iommu_id(), *bti.bti_id(),
             bti.bti_id_mask().value_or(0), child.name());

  child.AddBti(bti);

  return zx::ok();
}

zx::result<> BtiVisitor::FinalizeNode(Node& node) {
  const uint32_t iommu_id = node.id();
  if (iommu_nodes_.find(iommu_id) != iommu_nodes_.end()) {
    if (const auto it = iommu_base_addresses_.find(iommu_id); it != iommu_base_addresses_.end()) {
      std::ignore = node.RegisterIommu(
          iommu_id, fuchsia_hardware_platform_bus::Iommu::WithArmSmmu(it->second));
    } else {
      std::ignore =
          node.RegisterIommu(iommu_id, fuchsia_hardware_platform_bus::Iommu::WithStubIommu({}));
    }
  }
  return zx::ok();
}

}  // namespace fdf_devicetree
