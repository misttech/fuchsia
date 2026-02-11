// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/driver/devicetree/manager/publisher-dev.h"

#include <lib/driver/component/cpp/composite_node_spec.h>
#include <lib/driver/logging/cpp/logger.h>
#include <zircon/status.h>

namespace fdf_devicetree {

zx::result<> PublisherDev::AddPbusNode(fuchsia_hardware_platform_bus::Node& pbus_node) {
  fdf::Arena arena('PBUS');
  fidl::Arena fidl_arena;
  auto result = pbus_.buffer(arena)->NodeAdd(fidl::ToWire(fidl_arena, pbus_node));
  if (!result.ok()) {
    FDF_LOG(ERROR, "NodeAdd request failed: %s", result.FormatDescription().data());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    FDF_LOG(ERROR, "NodeAdd failed: %d", result->error_value());
    return zx::error(result->error_value());
  }
  return zx::ok();
}

zx::result<> PublisherDev::AddBoardChildNode(BoardChildNode args) {
  fuchsia_driver_framework::NodeAddArgs node_add_args;
  node_add_args.name() = std::move(args.name);
  if (args.driver_host) {
    node_add_args.driver_host() = *args.driver_host;
  }

  node_add_args.properties2() = std::move(args.properties);
  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_driver_framework::NodeController>::Create();
  auto result = fdf_node_->AddChild({std::move(node_add_args), std::move(server_end), {}});
  if (result.is_error()) {
    FDF_LOG(ERROR, "AddChild request failed: %s", result.error_value().FormatDescription().c_str());
    return zx::error(ZX_ERR_INTERNAL);
  }
  return zx::ok();
}

zx::result<> PublisherDev::AddCompositeNodeSpec(
    std::string name, std::vector<fuchsia_driver_framework::ParentSpec2> parents,
    std::optional<std::string> driver_host) {
  fuchsia_driver_framework::CompositeNodeSpec group{{
      .name = name,
      .parents2 = std::move(parents),
  }};
  if (driver_host) {
    group.driver_host() = *driver_host;
  }

  auto devicegroup_result = mgr_->AddSpec({std::move(group)});
  if (devicegroup_result.is_error()) {
    FDF_LOG(ERROR, "Failed to create composite node: %s",
            devicegroup_result.error_value().FormatDescription().data());
    return zx::error(devicegroup_result.error_value().is_framework_error()
                         ? devicegroup_result.error_value().framework_error().status()
                         : ZX_ERR_INVALID_ARGS);
  }
  return zx::ok();
}

zx::result<> PublisherDev::RegisterIommu(uint32_t iommu_id,
                                         fuchsia_hardware_platform_bus::Iommu iommu) {
  fdf::Arena arena('PBUS');
  auto iommu_wire = iommu.stub_iommu()
                        ? fuchsia_hardware_platform_bus::wire::Iommu::WithStubIommu({})
                        : fuchsia_hardware_platform_bus::wire::Iommu::WithArmSmmu(
                              arena, iommu.arm_smmu()->base_address());
  auto result = pbus_.buffer(arena)->RegisterIommu(iommu_id, iommu_wire);
  if (!result.ok()) {
    FDF_LOG(ERROR, "Failed to publish iommu: %d : %s", iommu_id,
            result.FormatDescription().c_str());
    return zx::error(result.status());
  }

  if (result->is_error()) {
    FDF_LOG(ERROR, "Failed to publish iommu: %d : %d", iommu_id, result->error_value());
    return zx::error(result->error_value());
  }
  return zx::ok();
}

}  // namespace fdf_devicetree
