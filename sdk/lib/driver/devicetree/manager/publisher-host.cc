// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "sdk/lib/driver/devicetree/manager/publisher-host.h"

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>
#include <lib/driver/logging/cpp/logger.h>
#include <zircon/errors.h>

namespace fdf_devicetree {

zx::result<> PublisherHost::AddPbusNode(fuchsia_hardware_platform_bus::Node& pbus_node) {
  pbus_nodes_.push_back(std::move(pbus_node));
  return zx::ok();
}

zx::result<> PublisherHost::AddBoardChildNode(BoardChildNode args) {
  board_child_nodes_.push_back(std::move(args));
  return zx::ok();
}

const std::vector<BoardChildNode>& PublisherHost::GetBoardChildNodes() {
  return board_child_nodes_;
}

zx::result<> PublisherHost::AddCompositeNodeSpec(
    std::string name, std::vector<fuchsia_driver_framework::ParentSpec2> parents,
    std::optional<std::string> driver_host) {
  fuchsia_driver_framework::CompositeNodeSpec spec;
  spec.name() = std::move(name);
  spec.parents2() = std::move(parents);
  composite_node_specs_.push_back(std::move(spec));
  return zx::ok();
}

zx::result<> PublisherHost::RegisterIommu(uint32_t iommu_id,
                                          fuchsia_hardware_platform_bus::Iommu iommu) {
  iommus_.insert_or_assign(iommu_id, std::move(iommu));
  return zx::ok();
}

namespace testing {

namespace {
class PublisherHostWrapper : public PublisherHost {
 public:
  PublisherHostWrapper() {
    // Initialize host logger
    logger_ = std::make_unique<fdf::Logger>("manager-test", FUCHSIA_LOG_INFO);
    fdf::Logger::SetGlobalInstance(logger_.get());
  }
  ~PublisherHostWrapper() override { fdf::Logger::SetGlobalInstance(nullptr); }

 private:
  std::unique_ptr<fdf::Logger> logger_;
};
}  // namespace

std::unique_ptr<TestPublisher> CreateTestPublisher() {
  return std::make_unique<PublisherHostWrapper>();
}

}  // namespace testing

}  // namespace fdf_devicetree
