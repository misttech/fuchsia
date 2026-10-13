// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_VIEW_TREE_INPUT_OWNERSHIP_EVALUATOR_H_
#define SRC_UI_SCENIC_LIB_VIEW_TREE_INPUT_OWNERSHIP_EVALUATOR_H_

#include <lib/zx/event.h>
#include <zircon/types.h>

#include <memory>
#include <optional>

#include "src/ui/scenic/lib/view_tree/snapshot_types.h"

namespace view_tree {

// Evaluates ViewTree snapshots to determine whether a registered client view
// covers full screen as the sole active input view. Dynamically asserts and
// clears orthogonal input ownership signals on the shared `ownership_event`:
// - Assert ZX_USER_SIGNAL_3 (`kInputClientOwned`) and clear ZX_USER_SIGNAL_4
//   (`kInputPlatformOwned`) when single-view fullscreen criteria are met.
// - Assert ZX_USER_SIGNAL_4 and clear ZX_USER_SIGNAL_3 when conditions are not met.
// - ZX_USER_SIGNAL_0..2 (`kDisplayUnowned`, `kDisplayPlatformOwned`) are never modified.
//
// Threading Model:
// All member functions must be called from the same thread (main/Scenic dispatcher thread).
//
// Direct-Client Model:
// Follows the singleton direct-client replacement contract: registering a new client ViewRef
// replaces the prior one and re-evaluates against the new view. If the registered view is
// in the tree as a fullscreen sole leaf without competing views or overlays, direct input ownership
// is granted (asserting ZX_USER_SIGNAL_3). Ownership goes to the platform unless the new view
// is in the tree as a fullscreen sole leaf, and when the registered view disconnects.
//
// View Hierarchy Scope:
// All overlays revoking `kInputClientOwned` must have `ViewRef`s in `snapshot.view_tree`.
// Anonymous views and ancestor-level transform content (such as mouse cursor nodes attached
// directly to ancestor transforms) without independent `ViewRef` entries are intentionally
// excluded from overlay evaluation. Intermediate ancestors must cover or exceed root bounds to
// ensure the client view is not clipped on screen.
class InputOwnershipEvaluator {
 public:
  InputOwnershipEvaluator() = default;
  ~InputOwnershipEvaluator() = default;

  // Registers a view ref koid requesting direct input ownership.
  // Replaces any previously registered view ref.
  // Triggers evaluation if a snapshot is already present.
  void RegisterViewRef(zx_koid_t koid);

  // Sets or updates the display ownership event handle.
  // Duplicates the event handle to maintain ownership.
  // Triggers evaluation if a snapshot is already present.
  void SetOwnershipEvent(const zx::event& ownership_event);

  // Updates the latest snapshot and re-evaluates input ownership.
  // Returns true if single-view fullscreen criteria were met.
  bool OnNewViewTreeSnapshot(std::shared_ptr<const Snapshot> snapshot);

  // Convenience overload accepting a const reference for callers and unit tests.
  bool OnNewViewTreeSnapshot(const Snapshot& snapshot) {
    return OnNewViewTreeSnapshot(std::make_shared<const Snapshot>(snapshot));
  }

  std::optional<zx_koid_t> registered_view_for_test() const { return registered_view_koid_; }

 private:
  // Re-evaluates input ownership using `latest_snapshot_` and updates
  // signals on `ownership_event_` if valid.
  bool Evaluate();

  // Prunes stale view ref from `registered_view_koid_`.
  // The KOID is marked as seen/connected when it appears in `snapshot.view_tree`
  // or `snapshot.unconnected_views`. It is erased if it was previously
  // seen/connected and is no longer present in either `view_tree` or `unconnected_views`.
  void PruneRegisteredView(const Snapshot& snapshot);

  // Checks whether the snapshot meets the single-view fullscreen criteria for
  // the registered view. Intermediate ancestors must cover or exceed root bounds.
  static bool IsSingleViewFullscreen(const Snapshot& snapshot,
                                     std::optional<zx_koid_t> registered_view_koid);

  zx::event ownership_event_;
  std::shared_ptr<const Snapshot> latest_snapshot_;

  std::optional<bool> last_is_eligible_;

  std::optional<zx_koid_t> registered_view_koid_;
  bool registered_view_seen_ = false;
};

}  // namespace view_tree

#endif  // SRC_UI_SCENIC_LIB_VIEW_TREE_INPUT_OWNERSHIP_EVALUATOR_H_
