// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/view_tree/input_ownership_evaluator.h"

#include <fidl/fuchsia.ui.input.internal/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/trace/event.h>
#include <zircon/status.h>

#include <algorithm>
#include <array>
#include <limits>

#include "src/ui/scenic/lib/utils/math.h"

namespace view_tree {

using fuchsia_ui_input_internal::InputOwnershipSignal;

void InputOwnershipEvaluator::SetOwnershipEvent(const zx::event& ownership_event) {
  zx::event dup_event;
  if (ownership_event.is_valid()) {
    zx_status_t status = ownership_event.duplicate(ZX_RIGHT_SAME_RIGHTS, &dup_event);
    if (status != ZX_OK) {
      FX_LOGS(ERROR) << "Failed to duplicate ownership event: " << zx_status_get_string(status);
    }
    FX_DCHECK(status == ZX_OK);
  }
  ownership_event_ = std::move(dup_event);
  last_is_eligible_.reset();
  if (latest_snapshot_) {
    Evaluate();
  }
}

void InputOwnershipEvaluator::RegisterViewRef(zx_koid_t koid) {
  if (koid != ZX_KOID_INVALID) {
    registered_view_koid_ = koid;
    registered_view_seen_ = false;
    if (latest_snapshot_) {
      Evaluate();
    }
  }
}

bool InputOwnershipEvaluator::OnNewViewTreeSnapshot(std::shared_ptr<const Snapshot> snapshot) {
  latest_snapshot_ = std::move(snapshot);
  return Evaluate();
}

bool InputOwnershipEvaluator::Evaluate() {
  TRACE_DURATION("gfx", "InputOwnershipEvaluator::Evaluate");
  if (!latest_snapshot_) {
    return false;
  }
  PruneRegisteredView(*latest_snapshot_);
  const bool is_eligible = IsSingleViewFullscreen(*latest_snapshot_, registered_view_koid_);

  if (ownership_event_.is_valid()) {
    if (!last_is_eligible_.has_value() || *last_is_eligible_ != is_eligible) {
      zx_status_t status = ZX_OK;
      if (is_eligible) {
        status = ownership_event_.signal(
            static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
            static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
      } else {
        status = ownership_event_.signal(
            static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
            static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
      }
      FX_DCHECK(status == ZX_OK);
      last_is_eligible_ = is_eligible;
    }
  }
  return is_eligible;
}

void InputOwnershipEvaluator::PruneRegisteredView(const Snapshot& snapshot) {
  if (snapshot.root == ZX_KOID_INVALID || snapshot.view_tree.empty()) {
    return;
  }
  if (!registered_view_koid_.has_value()) {
    return;
  }
  const zx_koid_t koid = *registered_view_koid_;
  if (snapshot.view_tree.contains(koid) || snapshot.unconnected_views.contains(koid)) {
    registered_view_seen_ = true;
  } else if (registered_view_seen_) {
    registered_view_koid_.reset();
    registered_view_seen_ = false;
  }
}

bool InputOwnershipEvaluator::IsSingleViewFullscreen(
    const Snapshot& snapshot, std::optional<zx_koid_t> registered_view_koid) {
  TRACE_DURATION("gfx", "InputOwnershipEvaluator::IsSingleViewFullscreen");
  if (!registered_view_koid.has_value()) {
    return false;
  }

  if (snapshot.root == ZX_KOID_INVALID || snapshot.view_tree.empty()) {
    return false;
  }

  auto root_it = snapshot.view_tree.find(snapshot.root);
  if (root_it == snapshot.view_tree.end()) {
    return false;
  }
  const auto& root_node = root_it->second;
  const float root_width = root_node.bounding_box.max[0] - root_node.bounding_box.min[0];
  const float root_height = root_node.bounding_box.max[1] - root_node.bounding_box.min[1];
  if (root_width <= 0.f || root_height <= 0.f) {
    return false;
  }

  const zx_koid_t target_koid = *registered_view_koid;
  auto target_it = snapshot.view_tree.find(target_koid);
  if (target_it == snapshot.view_tree.end()) {
    return false;
  }
  const auto* target_node = &target_it->second;

  // Single view check:
  // Target must be a leaf view (no child views).
  if (!target_node->children.empty()) {
    return false;
  }

  const float target_width = target_node->bounding_box.max[0] - target_node->bounding_box.min[0];
  const float target_height = target_node->bounding_box.max[1] - target_node->bounding_box.min[1];
  if (target_width <= 0.f || target_height <= 0.f) {
    return false;
  }

  // If the target view is the root view itself, it must be the sole view in the tree.
  if (target_koid == snapshot.root) {
    return snapshot.view_tree.size() == 1;
  }

  // Helper lambda to check if a node's bounding box covers or exceeds root bounds in root space.
  auto covers_root = [&](zx_koid_t koid, const ViewNode& node) -> bool {
    const auto root_from_node =
        snapshot.GetDestinationViewFromSourceViewTransform(koid, snapshot.root);
    if (!root_from_node.has_value()) {
      return false;
    }

    const std::array<glm::vec2, 4> local_corners = {
        glm::vec2(node.bounding_box.min[0], node.bounding_box.min[1]),
        glm::vec2(node.bounding_box.max[0], node.bounding_box.min[1]),
        glm::vec2(node.bounding_box.min[0], node.bounding_box.max[1]),
        glm::vec2(node.bounding_box.max[0], node.bounding_box.max[1]),
    };

    float root_space_min_x = std::numeric_limits<float>::infinity();
    float root_space_min_y = std::numeric_limits<float>::infinity();
    float root_space_max_x = -std::numeric_limits<float>::infinity();
    float root_space_max_y = -std::numeric_limits<float>::infinity();

    for (const auto& corner : local_corners) {
      const glm::vec2 p = utils::TransformPointerCoords(corner, *root_from_node);
      root_space_min_x = std::min(root_space_min_x, p.x);
      root_space_min_y = std::min(root_space_min_y, p.y);
      root_space_max_x = std::max(root_space_max_x, p.x);
      root_space_max_y = std::max(root_space_max_y, p.y);
    }

    // kEpsilon allows tolerance for floating-point rounding from matrix inversion.
    constexpr float kEpsilon = 1e-3f;
    return (root_space_min_x <= root_node.bounding_box.min[0] + kEpsilon &&
            root_space_min_y <= root_node.bounding_box.min[1] + kEpsilon &&
            root_space_max_x >= root_node.bounding_box.max[0] - kEpsilon &&
            root_space_max_y >= root_node.bounding_box.max[1] - kEpsilon);
  };

  // Fullscreen check for the target view:
  if (!covers_root(target_koid, *target_node)) {
    return false;
  }

  // Verify ancestor chain and intermediate viewport clipping:
  // Walk parent pointers directly up to root without allocating an ancestor vector.
  // Intermediate ancestors must also cover the root viewport so the target view is not
  // clipped on display.
  size_t ancestor_count = 0;
  zx_koid_t curr = target_node->parent;
  while (curr != ZX_KOID_INVALID) {
    auto ancestor_it = snapshot.view_tree.find(curr);
    if (ancestor_it == snapshot.view_tree.end()) {
      return false;
    }
    ++ancestor_count;
    if (ancestor_it->second.children.size() != 1) {
      return false;
    }
    if (curr == snapshot.root) {
      break;
    }
    if (!covers_root(curr, ancestor_it->second)) {
      return false;
    }
    curr = ancestor_it->second.parent;
  }

  if (curr != snapshot.root) {
    return false;
  }

  // All nodes in the view tree must be either the target view or an ancestor of the target view.
  // This guarantees there are no competing/occluding views, sibling views, or overlay views.
  // Note: Only views with ViewRefs in the ViewTree topology are evaluated. Anonymous views
  // or ancestor-level transform content (such as mouse cursor transforms) without separate
  // ViewRef nodes in snapshot.view_tree are intentionally ignored.
  if (snapshot.view_tree.size() != ancestor_count + 1) {
    return false;
  }

  return true;
}

}  // namespace view_tree
