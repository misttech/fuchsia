// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "lib/driver/devicetree/manager/manager.h"

#ifdef __Fuchsia__
#include <fidl/fuchsia.hardware.platform.bus/cpp/driver/fidl.h>
#endif

#include <lib/devicetree/devicetree.h>
#include <lib/driver/logging/cpp/logger.h>
#include <lib/zbi-format/zbi.h>
#include <lib/zx/result.h>
#include <zircon/status.h>

#include <algorithm>
#include <cstddef>

namespace fdf {
using namespace fuchsia_driver_framework;
}

namespace fhpb = fuchsia_hardware_platform_bus;

namespace {
std::string GetPath(const devicetree::NodePath& node_path) {
  std::string path;
  for (std::string_view p : node_path) {
    // Skip adding '/' for the root node.
    if (path.length() != 1) {
      path.append("/");
    }
    path.append(p);
  }
  return path;
}

std::string GetParentPath(const devicetree::NodePath& node_path) {
  if (node_path.size() <= 1) {
    // root node.
    return "";
  }

  std::string path;
  auto it = node_path.begin();
  for (size_t i = 0; i < (node_path.size() - 1); i++, it++) {
    // Skip adding '/' for the root node.
    if (path.length() != 1) {
      path.append("/");
    }
    path.append(it->data());
  }
  return path;
}

}  // namespace

namespace fdf_devicetree {

#ifdef __Fuchsia__
zx::result<Manager> Manager::CreateFromNamespace(fdf::Namespace& ns) {
  zx::result client = ns.Connect<fhpb::Service::Firmware>();
  if (client.is_error()) {
    FDF_LOG(ERROR, "Failed to connect to fuchsia.hardware.platform.bus.Firmware: %d",
            client.status_value());
    return client.take_error();
  }

  fdf::Arena arena('dtdt');
  auto result =
      fdf::WireCall(*client).buffer(arena)->GetFirmware(fhpb::wire::FirmwareType::kDeviceTree);

  if (!result.ok()) {
    FDF_LOG(ERROR, "Failed to send GetFirmware request: %s", result.FormatDescription().data());
    return zx::error(result.status());
  }
  if (result->is_error()) {
    FDF_LOG(ERROR, "Failed to GetFirmware: %d", result->error_value());
    return zx::error(result->error_value());
  }

  auto& [vmo, length] = result->value()->blobs[0];
  std::vector<uint8_t> data;
  data.resize(length);

  zx_status_t status = vmo.read(data.data(), 0, length);
  if (status != ZX_OK) {
    FDF_LOG(ERROR, "Failed to read %lu bytes from the devicetree: %d", length, status);
    return zx::error(status);
  }

  return zx::ok(Manager(std::move(data)));
}
#endif

zx::result<> Manager::Walk(Visitor& visitor) {
  // Walk the tree and create all nodes before calling the visitor. This is required for
  // |GetReferenceNode| method to work properly.
  tree_.Walk([&, this](const devicetree::NodePath& path,
                       const devicetree::PropertyDecoder& decoder) {
    FDF_LOG(DEBUG, "Found node - %.*s", static_cast<int>(path.back().length()), path.back().data());

    Node* parent = nullptr;
    if (path != "/") {
      parent = nodes_by_path_[GetParentPath(path)];
    }

    // Create a node.
    const devicetree::Properties& properties = decoder.properties();
    auto node = std::make_unique<Node>(parent, path.back(), properties, node_id_++, this);
    Node* ptr = node.get();
    nodes_publish_order_.emplace_back(std::move(node));
    FDF_LOG(DEBUG, "Node[%d] - %s added for publishing", node_id_, path.back().data());

    if (ptr->phandle()) {
      nodes_by_phandle_.emplace(*(ptr->phandle()), ptr);
    }
    nodes_by_path_.emplace(GetPath(path), ptr);
    return true;
  });

  zx::result<> visit_status = zx::ok();
  tree_.Walk([&, this](const devicetree::NodePath& path,
                       const devicetree::PropertyDecoder& decoder) {
    FDF_LOG(DEBUG, "Visit node - %.*s", static_cast<int>(path.back().length()), path.back().data());
    auto node = nodes_by_path_[GetPath(path)];
    zx::result<> status = visitor.Visit(*node, decoder);
    if (status.is_error()) {
      FDF_LOG(ERROR, "Node visit failed. node_name: %s, status_str: %d", node->name().c_str(),
              status.status_value());
      visit_status = status;
    }
    return true;
  });

  if (visit_status.is_error()) {
    FDF_LOG(ERROR, "Devicetree walk failed. status_str: %d", visit_status.status_value());
    return visit_status;
  }

  // Call |FinalizeNode| method of the visitor on all nodes to complete the parsing. At this point
  // all references to the node is known and so the visitor can use that information to update any
  // Node properties if needed.
  for (auto& node : nodes_publish_order_) {
    FDF_LOG(DEBUG, "Finalize node - %s", node->name().c_str());
    zx::result finalize_status = visitor.FinalizeNode(*node);
    if (finalize_status.is_error()) {
      FDF_LOG(ERROR, "Node finalize failed. node_name: %s, status_str: %d", node->name().c_str(),
              finalize_status.status_value());
      return finalize_status;
    }
  }

  return zx::ok();
}

zx::result<> Manager::PublishDevices(PublisherInterface& publisher) {
  for (const auto& [iommu_id, iommu] : iommus_) {
    zx::result<> result = publisher.RegisterIommu(iommu_id, iommu);
    if (result.is_error()) {
      FDF_LOG(ERROR, "Failed to register IOMMU for node ID %d: %d", iommu_id, result.error_value());
    }
  }

  for (auto& node : nodes_publish_order_) {
    zx::result<> status = node->Publish(publisher);
    if (status.is_error()) {
      FDF_LOG(ERROR, "Failed to publish device for node ID %d: %d", node->id(),
              status.status_value());
    }
  }
  return zx::ok();
}

zx::result<ReferenceNode> Manager::GetReferenceNode(Phandle id) {
  auto node = nodes_by_phandle_.find(id);
  if (node == nodes_by_phandle_.end()) {
    return zx::error(ZX_ERR_NOT_FOUND);
  }

  return zx::ok(ReferenceNode(node->second));
}

uint32_t Manager::GetPublishIndex(uint32_t node_id) {
  for (uint32_t index = 0; index < nodes_publish_order_.size(); index++) {
    if (nodes_publish_order_[index]->id() == node_id) {
      return index;
    }
  }
  ZX_ASSERT_MSG(false, "Should not reach here. Node id should always be valid.");
  return 0;
}

zx::result<> Manager::ChangePublishOrder(uint32_t node_id, uint32_t new_index) {
  if (new_index >= nodes_publish_order_.size()) {
    FDF_LOG(
        ERROR,
        "The change publish order request index (%d) is out of range. The list only contains %zu items.",
        new_index, nodes_publish_order_.size());
    return zx::error(ZX_ERR_INVALID_ARGS);
  }

  std::swap(nodes_publish_order_[new_index], nodes_publish_order_[GetPublishIndex(node_id)]);

  return zx::ok();
}

zx::result<> Manager::RegisterIommu(uint32_t iommu_id, fhpb::Iommu iommu) {
  auto [_, inserted] = iommus_.insert({iommu_id, iommu});
  if (!inserted) {
    return zx::error(ZX_ERR_ALREADY_EXISTS);
  }
  return zx::ok();
}

std::optional<Node*> Manager::FindNode(std::string_view name) {
  for (auto& node : nodes_publish_order_) {
    if (node->name() == name) {
      return node.get();
    }
  }
  return std::nullopt;
}

}  // namespace fdf_devicetree
