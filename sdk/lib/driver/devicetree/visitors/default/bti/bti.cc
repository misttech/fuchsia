// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
#include "lib/driver/devicetree/visitors/default/bti/bti.h"

#include <fidl/fuchsia.hardware.platform.bus/cpp/fidl.h>
#include <lib/devicetree/devicetree.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/driver/logging/cpp/structured_logger.h>

#include <cstdint>
#include <optional>

namespace fdf_devicetree {

constexpr const char kBtiProp[] = "iommus";
constexpr const char kBtiNameProp[] = "iommu-names";
constexpr const char kIommuCellsProp[] = "#iommu-cells";

// #iommu-cells == 1.
// This is because fuchsia_hardware_platform_bus::Bti only takes a
// bti_id as a specifier which is u32. Therefore iommu specifier should be 1 cell wide.
constexpr const uint32_t kIommuCellSize = 1;

class IommuCell {
 public:
  using IommuPropertyElement = devicetree::PropEncodedArrayElement<kIommuCellSize>;

  explicit IommuCell(PropertyCells cells) : property_array_(cells, kIommuCellSize) {}

  uint32_t bti_id() {
    IommuPropertyElement element = property_array_[0];
    std::optional<uint64_t> cell = element[0];
    return static_cast<uint32_t>(*cell);
  }

 private:
  devicetree::PropEncodedArray<IommuPropertyElement> property_array_;
};

BtiVisitor::BtiVisitor() {
  fdf_devicetree::Properties properties = {};
  properties.emplace_back(std::make_unique<fdf_devicetree::ReferenceProperty>(
      kBtiProp, kIommuCellsProp, /* required */ false));
  properties.emplace_back(
      std::make_unique<fdf_devicetree::StringListProperty>(kBtiNameProp, /* required */ false));
  reference_parser_ = std::make_unique<fdf_devicetree::PropertyParser>(std::move(properties));
}

zx::result<> BtiVisitor::Visit(Node& node, const devicetree::PropertyDecoder& decoder) {
  zx::result parser_output = reference_parser_->Parse(node);
  if (parser_output.is_error()) {
    FDF_LOG(ERROR, "Failed to parse reference for node '%s'", node.name().c_str());
    return parser_output.take_error();
  }

  auto btis = parser_output->Get<References>(kBtiProp);
  if (!btis) {
    return zx::ok();
  }

  auto iommu_names = parser_output->Get<std::vector<std::string>>(kBtiNameProp);
  if (iommu_names && iommu_names->size() > btis->size()) {
    FDF_LOG(ERROR, "Node '%s' has %zu iommu entries but has %zu iommmu names.", node.name().c_str(),
            btis->size(), iommu_names->size());
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
    }
  }

  return zx::ok();
}

zx::result<> BtiVisitor::ReferenceChildVisit(Node& child, ReferenceNode& parent,
                                             PropertyCells reference_cells,
                                             std::optional<std::string> iommu_name) {
  std::optional<uint32_t> iommu_index;

  // Check if iommu is already registered.
  for (uint32_t i = 0; i < iommu_nodes_.size(); i++) {
    if (iommu_nodes_[i] == parent.phandle()) {
      iommu_index = i;
      break;
    }
  }

  // Register iommu if not found.
  if (!iommu_index) {
    iommu_nodes_.push_back(*parent.phandle());
    iommu_index = iommu_nodes_.size() - 1;
  }

  auto iommu_cell = IommuCell(reference_cells);

  fuchsia_hardware_platform_bus::Bti bti = {{
      .iommu_index = iommu_index,
      .bti_id = iommu_cell.bti_id(),
      .name = std::move(iommu_name),
  }};
  FDF_LOG(DEBUG, "BTI %s index: 0x%0x, id: 0x%0x, added to node '%s'.",
          bti.name().has_value() ? bti.name()->c_str() : "(no name)", *bti.iommu_index(),
          *bti.bti_id(), child.name().c_str());
  child.AddBti(bti);

  return zx::ok();
}

}  // namespace fdf_devicetree
