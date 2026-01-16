// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/composite/parent_set_collector.h"

#include <fidl/fuchsia.driver.framework/cpp/fidl.h>

#include "src/devices/lib/log/log.h"

namespace driver_manager {

zx::result<> ParentSetCollector::AddNode(
    uint32_t index, const std::vector<fuchsia_driver_framework::NodeProperty2>& node_properties,
    std::weak_ptr<Node> node) {
  ZX_ASSERT(HasCompositeInfo());
  ZX_ASSERT(index < parents_.size());

  if (parents_[index] != std::nullopt && !parents_[index]->expired()) {
    return zx::error(ZX_ERR_ALREADY_BOUND);
  }
  parents_[index] = std::move(node);
  parent_properties_[index] =
      fuchsia_driver_framework::NodePropertyEntry2(parent_names_.value()[index], node_properties);

  if (auto node_ptr = parents_[index]->lock(); node_ptr) {
    node_ptr->MarkAsCompositeParent();
  }

  return zx::ok();
}

void ParentSetCollector::ReleaseNodes() {
  for (auto& node : parents_) {
    if (node == std::nullopt) {
      continue;
    }
    if (auto node_ptr = node->lock(); node_ptr) {
      node_ptr->UnmarkAsCompositeParent();
    }
    node.reset();
  }
}

zx::result<std::shared_ptr<Node>> ParentSetCollector::TryToAssemble(
    std::string_view name, NodeManager* node_manager, async_dispatcher_t* dispatcher) {
  ZX_ASSERT(HasCompositeInfo());
  if (completed_composite_node_ && !completed_composite_node_->expired()) {
    return zx::error(ZX_ERR_ALREADY_EXISTS);
  }

  std::vector<NodeWkPtr> parent_nodes;
  parent_nodes.reserve(parents_.size());
  for (auto& node : parents_) {
    if (node == std::nullopt) {
      return zx::error(ZX_ERR_SHOULD_WAIT);
    }
    parent_nodes.emplace_back(node.value());
  }

  auto result = Node::CreateCompositeNode(std::string(name), parent_nodes, parent_names_.value(),
                                          parent_properties_, node_manager, dispatcher,
                                          driver_host_name_for_colocation_, primary_index_.value());
  if (result.is_error()) {
    return result.take_error();
  }

  fdf_log::info("Built composite node '{}' for completed composite node spec", name);
  completed_composite_node_.emplace(result.value());
  return zx::ok(result.value());
}

fuchsia_driver_development::wire::CompositeNodeInfo ParentSetCollector::GetCompositeInfo(
    fidl::AnyArena& arena,
    const std::optional<fuchsia_driver_framework::CompositeInfo>& composite_info) const {
  namespace fdd = fuchsia_driver_development;

  auto composite_node_info = fdd::wire::CompositeNodeInfo::Builder(arena);
  if (!HasCompositeInfo()) {
    fidl::VectorView<fidl::StringView> parent_topological_paths(arena, size());
    composite_node_info.parent_topological_paths(parent_topological_paths);
    return composite_node_info.Build();
  }

  if (composite_info.has_value()) {
    composite_node_info.composite(fdd::wire::CompositeInfo::WithComposite(
        arena, fidl::ToWire(arena, composite_info.value())));
  }

  composite_node_info.parent_topological_paths(GetParentTopologicalPaths(arena));

  std::optional<NodeWkPtr> composite_node = completed_composite_node();
  if (composite_node) {
    if (auto node_ptr = composite_node->lock(); node_ptr) {
      composite_node_info.topological_path(node_ptr->MakeTopologicalPath());
    }
  }
  return composite_node_info.Build();
}

fidl::VectorView<fidl::StringView> ParentSetCollector::GetParentTopologicalPaths(
    fidl::AnyArena& arena) const {
  fidl::VectorView<fidl::StringView> parent_topological_paths(arena, parents_.size());
  for (uint32_t i = 0; i < parents_.size(); i++) {
    if (parents_[i] == std::nullopt) {
      parent_topological_paths[i] = fidl::StringView();
      continue;
    }

    if (auto node = parents_[i]->lock(); node) {
      parent_topological_paths[i] = fidl::StringView(arena, node->MakeTopologicalPath());
    } else {
      parent_topological_paths[i] = fidl::StringView();
    }
  }
  return parent_topological_paths;
}

}  // namespace driver_manager
