// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef LIB_DRIVER_DEVICETREE_MANAGER_PUBLISHER_HOST_H_
#define LIB_DRIVER_DEVICETREE_MANAGER_PUBLISHER_HOST_H_

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>

#include "lib/driver/devicetree/manager/publisher.h"
#include "lib/driver/devicetree/manager/test-publisher.h"

namespace fdf_devicetree {

class PublisherHost : public testing::TestPublisher {
 public:
  zx::result<> AddPbusNode(fuchsia_hardware_platform_bus::Node& pbus_node) override;

  zx::result<> AddBoardChildNode(BoardChildNode args) override;

  zx::result<> AddCompositeNodeSpec(std::string name,
                                    std::vector<fuchsia_driver_framework::ParentSpec2> parents,
                                    std::optional<std::string> driver_host) override;

  zx::result<> RegisterIommu(uint32_t iommu_id,
                             fuchsia_hardware_platform_bus::Iommu iommu) override;

  const std::vector<fuchsia_hardware_platform_bus::Node>& GetPbusNodes() override {
    return pbus_nodes_;
  }
  const std::vector<BoardChildNode>& GetBoardChildNodes() override;
  const std::vector<fuchsia_driver_framework::CompositeNodeSpec>& GetCompositeNodeSpecs() override {
    return composite_node_specs_;
  }
  const std::unordered_map<uint32_t, fuchsia_hardware_platform_bus::Iommu>& GetIommus() override {
    return iommus_;
  }

 private:
  std::vector<fuchsia_hardware_platform_bus::Node> pbus_nodes_;
  std::vector<BoardChildNode> board_child_nodes_;
  std::vector<fuchsia_driver_framework::CompositeNodeSpec> composite_node_specs_;
  std::unordered_map<uint32_t, fuchsia_hardware_platform_bus::Iommu> iommus_;
};

}  // namespace fdf_devicetree

#endif  // LIB_DRIVER_DEVICETREE_MANAGER_PUBLISHER_HOST_H_
