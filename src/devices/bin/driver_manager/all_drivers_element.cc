// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/all_drivers_element.h"

#include "src/devices/bin/driver_manager/driver_runner.h"
#include "src/devices/lib/log/log.h"

namespace driver_manager {

namespace {

// Return the driver-bounded leaf nodes in the node topology. A driver-bounded leaf node
// is a driver-bounded node that does not have any driver-bounded descendants.
std::unordered_set<std::shared_ptr<const Node>> GetDriverBoundLeafNodes(
    const std::shared_ptr<Node>& root) {
  std::unordered_set<std::shared_ptr<const Node>> leaf_nodes;
  for (const auto& child : root->children()) {
    auto children_leaves = GetDriverBoundLeafNodes(child);
    leaf_nodes.insert(children_leaves.begin(), children_leaves.end());
  }

  // If there are no leaf nodes, then insert the root if it's driver-bound.
  if (leaf_nodes.empty() && root->is_bound()) {
    leaf_nodes.insert(root);
  }
  return leaf_nodes;
}

bool HasBoundDescendants(std::shared_ptr<const Node> node) {
  for (const auto& child : node->children()) {
    if (child->is_bound() || HasBoundDescendants(child)) {
      return true;
    }
  }
  return false;
}

}  // namespace

AllDriversElement::AllDriversElement(DriverRunner* runner, std::shared_ptr<Node> root)
    : driver_runner_(runner) {
  auto leaves = GetDriverBoundLeafNodes(root);
  for (const auto& node : leaves) {
    leaf_driver_instances_.emplace(node->MakeComponentMoniker(), node);
  }
}

void AllDriversElement::AttachElement(async_dispatcher_t* dispatcher, PowerElementHandles handles) {
  element_control_ = std::move(handles.element_control);
  lessor_ = std::move(handles.lessor);
  element_runner_bindings_.AddBinding(dispatcher, std::move(handles.element_runner), this,
                                      fidl::kIgnoreBindingClosure);
}

void AllDriversElement::OnNodeBound(std::shared_ptr<const Node> node) {
  // Add the node to |leaf_driver_instances_| if it has no descendants that are bound to a driver.
  if (!HasBoundDescendants(node)) {
    leaf_driver_instances_.emplace(node->MakeComponentMoniker(), node);

    std::shared_ptr<fit::deferred_callback> on_lease_acquire =
        std::make_shared<fit::deferred_callback>(
            fit::defer_callback([this, node]() { RemoveAncestors(node); }));
    AcquireLease(node, on_lease_acquire);
  }
}

void AllDriversElement::RemoveAncestors(std::shared_ptr<const Node> node) {
  for (const auto& parent_weak : node->parents()) {
    if (auto parent = parent_weak.lock()) {
      leaf_driver_instances_.erase(parent->MakeComponentMoniker());
      leases_.erase(parent->MakeComponentMoniker());
      RemoveAncestors(parent);
    }
  }
}

void AllDriversElement::AcquireLease(std::shared_ptr<const Node> node,
                                     std::shared_ptr<fit::deferred_callback> deferred) {
  std::string moniker = node->MakeComponentMoniker();
  zx::eventpair lease_token, lease_token_peer;
  if (zx::eventpair::create(0, &lease_token, &lease_token_peer) != ZX_OK) {
    fdf_log::error("Failed to create lease token for node '{}'", moniker);
    return;
  }

  fuchsia_power_broker::LeaseSchema schema;
  schema.lease_token(std::move(lease_token_peer));
  schema.lease_name(node->name());

  zx::event power_token = node->DuplicatePowerToken();
  if (power_token.is_valid()) {
    std::vector<fuchsia_power_broker::LeaseDependency> deps;
    deps.push_back(fuchsia_power_broker::LeaseDependency{{
        .requires_token = std::move(power_token),
        .requires_level = 1,
    }});
    schema.dependencies(std::move(deps));
  }

  driver_runner_->power_topology()
      ->Lease(std::move(schema))
      .Then([this, moniker, lease_token = std::move(lease_token), deferred = std::move(deferred)](
                fidl::Result<fuchsia_power_broker::Topology::Lease>& result) mutable {
        if (!result.is_ok()) {
          fdf_log::error("Failed to acquire lease for node '{}': {}", moniker,
                         result.error_value());
        } else if (current_level_ == 1) {
          leases_.emplace(moniker, std::move(lease_token));
        }
      });
}

void AllDriversElement::SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) {
  current_level_ = request->level;
  if (current_level_ == 0) {
    leases_.clear();
    completer.Reply();
  } else {
    ZX_ASSERT(current_level_ == 1);

    // This defer function is executed once all the AcquireLeases() calls have completed.
    auto defer =
        std::make_shared<fit::deferred_callback>([this, should_drop_leases = first_time_level_1_,
                                                  completer = completer.ToAsync()]() mutable {
          if (should_drop_leases) {
            driver_runner_->ClearBootupLeases();
          }
          completer.Reply();
        });
    first_time_level_1_ = false;

    for (const auto& node : leaf_driver_instances_) {
      if (auto locked_node = node.second.lock()) {
        AcquireLease(std::move(locked_node), defer);
      }
    }
  }
}

void AllDriversElement::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf_log::info("Unknown AllDrivers element runner method request received: {}",
                metadata.method_ordinal);
}

}  // namespace driver_manager
