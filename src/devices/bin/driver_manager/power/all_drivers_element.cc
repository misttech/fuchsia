// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/devices/bin/driver_manager/power/all_drivers_element.h"

#include <fidl/fuchsia.power.broker/cpp/fidl.h>

#include <memory>
#include <unordered_set>
#include <vector>

#include "src/devices/bin/driver_manager/power/power_manager.h"
#include "src/devices/lib/log/log.h"

namespace driver_manager {

namespace {

// Performs a bottom-up, post-order Depth-First-Search (DFS) over the node topology
// starting from |starting_node|.
void PerformPostOrderDFS(const std::shared_ptr<const Node>& starting_node,
                         fit::function<void(const std::shared_ptr<const Node>&)> visitor) {
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;

  stack.push_back(starting_node);
  visited.insert(starting_node.get());

  while (!stack.empty()) {
    auto curr = stack.back();

    bool pushed_child = false;
    for (const auto& child : curr->children()) {
      if (!visited.contains(child.get())) {
        visited.insert(child.get());
        stack.push_back(child);
        pushed_child = true;
        break;
      }
    }

    if (!pushed_child) {
      visitor(curr);
      stack.pop_back();
    }
  }
}

bool HasBoundDescendants(const std::shared_ptr<const Node>& start_node) {
  std::unordered_set<const Node*> visited;
  std::vector<std::shared_ptr<const Node>> stack;

  for (const auto& child : start_node->children()) {
    if (visited.insert(child.get()).second) {
      stack.push_back(child);
    }
  }

  while (!stack.empty()) {
    auto current = stack.back();
    stack.pop_back();

    if (current->is_bound()) {
      return true;
    }

    for (const auto& child : current->children()) {
      if (visited.insert(child.get()).second) {
        stack.push_back(child);
      }
    }
  }
  return false;
}

}  // namespace

AllDriversElement::AllDriversElement(PowerManager* power_manager, std::shared_ptr<Node> root)
    : root_(root), power_manager_(power_manager) {}

void AllDriversElement::AttachElement(async_dispatcher_t* dispatcher, PowerElementHandles handles) {
  element_control_ = std::move(handles.element_control);
  lessor_ = std::move(handles.lessor);
  if (handles.element_runner.is_valid()) {
    element_runner_bindings_.AddBinding(dispatcher, std::move(handles.element_runner), this,
                                        fidl::kIgnoreBindingClosure);
  }
}

void AllDriversElement::OnNodeBound(std::shared_ptr<const Node> node) {
  if (!transition_complete_) {
    return;
  }
  // During normal operation, every bound node takes its startup lease out. If it is not a
  // leaf node, this lease will immediately fall out of scope and be safely dropped.
  std::optional<zx::eventpair> startup_lease =
      std::const_pointer_cast<Node>(node)->TakeStartupLease();

  if (node->IsHermeticPowerTest()) {
    return;
  }

  if (HasBoundDescendants(node)) {
    // Already has bound descendants, so it is not a leaf and we want to drop its startup lease.
    return;
  }

  AddDependency(node, std::move(startup_lease));
}

void AllDriversElement::AddDependency(const std::shared_ptr<const Node>& node,
                                      std::optional<zx::eventpair> startup_lease) {
  if (!element_control_.is_valid()) {
    return;
  }

  zx::event power_token = node->DuplicatePowerToken();
  if (!power_token.is_valid()) {
    fdf_log::error(
        "Unable to duplicate power dependency token for node '{}' for All Drivers element",
        node->name());
    return;
  }

  std::vector<fuchsia_power_broker::PowerLevel> requires_levels;
  requires_levels.push_back(1);
  fuchsia_power_broker::LevelDependency dep{{
      .dependent_level = 1,
      .requires_token = std::move(power_token),
      .requires_level_by_preference = std::move(requires_levels),
      .remove_with_required_element = true,
  }};

  element_control_->AddDependency(std::move(dep))
      .Then([name = node->name(), startup_lease = std::move(startup_lease)](
                fidl::Result<fuchsia_power_broker::ElementControl::AddDependency>& result) {
        if (result.is_error()) {
          // A node can be added twice. Node::is_bound() becomes true as soon as we ask the driver
          // host to start the driver, but OnNodeBound() is only called once that start completes.
          // If the startup transition runs in between, CompleteStartupTransition() adds the
          // dependency first and OnNodeBound() then adds it again. Power Broker rejects the second
          // request with ALREADY_EXISTS, which just means the dependency is already in place.
          //
          // We deliberately don't track added dependencies ourselves. A restarted node gets a new
          // power element, which needs a new dependency even though its topological path and power
          // token are unchanged.
          if (result.error_value().is_domain_error() &&
              result.error_value().domain_error() ==
                  fuchsia_power_broker::ModifyDependencyError::kAlreadyExists) {
            fdf_log::debug("Dependency on node '{}' already exists", name);
            return;
          }
          fdf_log::error("Failed to add dependency on node '{}': {}", name, result.error_value());
        }
      });
}

void AllDriversElement::ProcessStartupNode(const std::shared_ptr<const Node>& node,
                                           bool has_bound_descendant) {
  // We only process driver-bound nodes. Unbound nodes do not take startup leases.
  if (!node->is_bound()) {
    return;
  }

  // Retrieve the startup lease. It is crucial to take it here regardless of whether
  // this is a leaf or non-leaf node, so it doesn't get left behind.
  std::optional<zx::eventpair> startup_lease =
      std::const_pointer_cast<Node>(node)->TakeStartupLease();

  if (node->IsHermeticPowerTest()) {
    return;
  }

  // If this node has bound descendants, it is not a leaf node in the active topology.
  if (has_bound_descendant) {
    // The startup lease goes out of scope here and is dropped. It is safe to drop it
    // because leaf nodes (visited earlier in post-order DFS) already held their leases
    // synchronously, ensuring the parent node remains powered.
    return;
  }

  AddDependency(node, std::move(startup_lease));
}

void AllDriversElement::CompleteStartupTransition() {
  if (current_level_ == 0) {
    return;
  }

  transition_complete_ = true;

  // Identify all leaf nodes and safely transfer their startup leases.
  if (auto root = root_.lock()) {
    // Memoization set to track which nodes have bound descendants.
    // Bottom-up post-order DFS guarantees that when we visit a node, we have already processed
    // all of its children, allowing O(1) checks for bound descendants.
    std::unordered_set<const Node*> has_bound_descendant_set;

    PerformPostOrderDFS(
        root, [this, &has_bound_descendant_set](const std::shared_ptr<const Node>& node) {
          // Check if this node has any descendants bound to a driver.
          bool has_bound_descendant = false;
          for (const auto& child : node->children()) {
            if (child->is_bound() || has_bound_descendant_set.contains(child.get())) {
              has_bound_descendant = true;
              has_bound_descendant_set.insert(node.get());
              break;
            }
          }

          ProcessStartupNode(node, has_bound_descendant);
        });
  }
}

void AllDriversElement::SetLevel(SetLevelRequestView request, SetLevelCompleter::Sync& completer) {
  current_level_ = request->level;

  if (!transition_complete_) {
    CompleteStartupTransition();
  }

  // We can reply before the AddDependency() calls in CompleteStartupTransition() are complete
  // because the leaf driver's startup lease will keep its power element on unti the dependencies
  // are added.
  completer.Reply();
}

void AllDriversElement::handle_unknown_method(
    fidl::UnknownMethodMetadata<fuchsia_power_broker::ElementRunner> metadata,
    fidl::UnknownMethodCompleter::Sync& completer) {
  fdf_log::info("Unknown AllDrivers element runner method request received: {}",
                metadata.method_ordinal);
}

}  // namespace driver_manager
