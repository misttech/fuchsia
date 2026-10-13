// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/view_tree/input_ownership_evaluator.h"

#include <fidl/fuchsia.ui.input.internal/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/event.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/view_tree/snapshot_types.h"

#include <glm/gtc/matrix_transform.hpp>

namespace view_tree::test {

namespace {

using fuchsia_ui_input_internal::InputOwnershipSignal;

enum : zx_koid_t {
  kRoot1A = 1,
  kNode2,
  kNode3,
  kRoot4B,
  kNode5,
  kRoot6C,
  kNode7,
  kNode8,
  kNode9,
  kNode10,
  kNode11,
};

ViewNode NewViewNodeWithBounds(zx_koid_t parent, std::unordered_set<zx_koid_t> children,
                               float width, float height,
                               const glm::mat4& local_from_world = glm::mat4(1.f)) {
  return ViewNode{
      .parent = parent,
      .children = std::move(children),
      .bounding_box = {.min = {0.f, 0.f}, .max = {width, height}},
      .local_from_world_transform = local_from_world,
      .view_ref = std::make_shared<const types::ViewRef>(fuchsia_ui_views::ViewRef{}),
  };
}

zx::event CreateEvent(uint32_t initial_signals = 0) {
  zx::event event;
  FX_CHECK(zx::event::create(0, &event) == ZX_OK);
  if (initial_signals != 0) {
    FX_CHECK(event.signal(0, initial_signals) == ZX_OK);
  }
  return event;
}

Snapshot CreateTwoNodeSnapshot(float root_w, float root_h, float node_w, float node_h,
                               const glm::mat4& root_local_from_world = glm::mat4(1.f),
                               const glm::mat4& node_local_from_world = glm::mat4(1.f)) {
  Snapshot snapshot;
  snapshot.root = kRoot1A;
  snapshot.view_tree[kRoot1A] =
      NewViewNodeWithBounds(ZX_KOID_INVALID, {kNode2}, root_w, root_h, root_local_from_world);
  snapshot.view_tree[kNode2] =
      NewViewNodeWithBounds(kRoot1A, {}, node_w, node_h, node_local_from_world);
  return snapshot;
}

void ExpectSignals(const zx::event& event, uint32_t expected_set, uint32_t expected_cleared) {
  zx_signals_t pending = 0;
  zx_status_t status = event.wait_one(0, zx::time::infinite_past(), &pending);
  ASSERT_TRUE(status == ZX_OK || status == ZX_ERR_TIMED_OUT);
  if (expected_set != 0) {
    EXPECT_EQ(pending & expected_set, expected_set);
  }
  if (expected_cleared != 0) {
    EXPECT_EQ(pending & expected_cleared, 0u);
  }
}

}  // namespace

TEST(InputOwnershipEvaluatorTest, EmptyTree) {
  Snapshot snapshot;
  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);

  // Initialize with kInputClientOwned set to verify evaluator actively transitions to
  // kInputPlatformOwned.
  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputClientOwned));
  evaluator.SetOwnershipEvent(event);

  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event,
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned |
                                      InputOwnershipSignal::kDisplayPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned |
                                      InputOwnershipSignal::kDisplayUnowned));
}

TEST(InputOwnershipEvaluatorTest, SingleRegisteredFullscreenView) {
  Snapshot snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);

  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);

  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputPlatformOwned));
  evaluator.SetOwnershipEvent(event);

  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event,
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned |
                                      InputOwnershipSignal::kDisplayPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned |
                                      InputOwnershipSignal::kDisplayUnowned));
}

TEST(InputOwnershipEvaluatorTest, UnregisteredView) {
  Snapshot snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);

  InputOwnershipEvaluator evaluator;
  // kNode2 is not registered; register another view kNode5 instead.
  evaluator.RegisterViewRef(kNode5);

  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputClientOwned));
  evaluator.SetOwnershipEvent(event);

  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
}

TEST(InputOwnershipEvaluatorTest, MultipleViewsOrOverlayAdded) {
  Snapshot snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);

  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);

  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputPlatformOwned));
  evaluator.SetOwnershipEvent(event);

  // Single fullscreen view -> true
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));

  // Add an overlay sibling to kNode2 attached to root:
  snapshot.view_tree[kRoot1A].children.insert(kNode3);
  snapshot.view_tree[kNode3] =
      NewViewNodeWithBounds(kRoot1A, {}, 200.f, 200.f);  // e.g. system overlay
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));

  // Remove overlay -> back to true
  snapshot.view_tree[kRoot1A].children.erase(kNode3);
  snapshot.view_tree.erase(kNode3);
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
}

TEST(InputOwnershipEvaluatorTest, NonFullscreenView) {
  // Target node size 500x500 is smaller than root 1000x1000
  Snapshot snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 500.f, 500.f);

  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);

  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputClientOwned));
  evaluator.SetOwnershipEvent(event);

  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
}

TEST(InputOwnershipEvaluatorTest, PreservingDisplayOwnershipBits) {
  Snapshot fullscreen_snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);
  Snapshot non_fullscreen_snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 500.f, 500.f);

  // Case 1: Virtcon display ownership (DISPLAY_UNOWNED).
  {
    InputOwnershipEvaluator evaluator;
    evaluator.RegisterViewRef(kNode2);

    zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayUnowned |
                                                        InputOwnershipSignal::kInputPlatformOwned));
    evaluator.SetOwnershipEvent(event);

    // Fullscreen -> sets INPUT_CLIENT_OWNED.
    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(fullscreen_snapshot));
    ExpectSignals(event,
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayUnowned |
                                        InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                        InputOwnershipSignal::kInputPlatformOwned));

    // Non-fullscreen -> sets INPUT_PLATFORM_OWNED.
    EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(non_fullscreen_snapshot));
    ExpectSignals(event,
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayUnowned |
                                        InputOwnershipSignal::kInputPlatformOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                        InputOwnershipSignal::kInputClientOwned));
  }

  // Case 2: Scenic display ownership (DISPLAY_PLATFORM_OWNED).
  {
    InputOwnershipEvaluator evaluator;
    evaluator.RegisterViewRef(kNode2);

    zx::event event = CreateEvent(static_cast<uint32_t>(
        InputOwnershipSignal::kDisplayPlatformOwned | InputOwnershipSignal::kInputPlatformOwned));
    evaluator.SetOwnershipEvent(event);

    // Fullscreen -> sets INPUT_CLIENT_OWNED.
    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(fullscreen_snapshot));
    ExpectSignals(event,
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                        InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayUnowned |
                                        InputOwnershipSignal::kInputPlatformOwned));

    // Non-fullscreen -> sets INPUT_PLATFORM_OWNED.
    EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(non_fullscreen_snapshot));
    ExpectSignals(event,
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                        InputOwnershipSignal::kInputPlatformOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kDisplayUnowned |
                                        InputOwnershipSignal::kInputClientOwned));
  }
}

TEST(InputOwnershipEvaluatorTest, StaleKoidCleanup) {
  Snapshot connected_snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);

  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);
  EXPECT_EQ(evaluator.registered_view_for_test(), kNode2);

  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputPlatformOwned));
  evaluator.SetOwnershipEvent(event);

  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(connected_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));

  // Step 1: Disconnected snapshot: kNode2 is now in unconnected_views.
  Snapshot disconnected_snapshot;
  disconnected_snapshot.root = kRoot1A;
  disconnected_snapshot.view_tree[kRoot1A] =
      NewViewNodeWithBounds(ZX_KOID_INVALID, {}, 1000.f, 1000.f);
  disconnected_snapshot.unconnected_views = {kNode2};

  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(disconnected_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));

  // Verify kNode2 remains registered while present in unconnected_views.
  EXPECT_EQ(evaluator.registered_view_for_test(), kNode2);

  // Step 2: Reconnect kNode2 fullscreen: asserts kInputClientOwned again.
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(connected_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
  EXPECT_EQ(evaluator.registered_view_for_test(), kNode2);

  // Step 2b: Unrooted snapshot (root = ZX_KOID_INVALID): verify kNode2 is not pruned.
  Snapshot unrooted_snapshot;
  unrooted_snapshot.root = ZX_KOID_INVALID;
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(unrooted_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
  EXPECT_EQ(evaluator.registered_view_for_test(), kNode2);

  // Re-evaluate connected snapshot: regains kInputClientOwned.
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(connected_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
  EXPECT_EQ(evaluator.registered_view_for_test(), kNode2);

  // Step 3: Remove kNode2 from both view_tree and unconnected_views.
  Snapshot removed_snapshot;
  removed_snapshot.root = kRoot1A;
  removed_snapshot.view_tree[kRoot1A] = NewViewNodeWithBounds(ZX_KOID_INVALID, {}, 1000.f, 1000.f);

  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(removed_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));

  // Verify kNode2 was cleaned up from registered views.
  EXPECT_EQ(evaluator.registered_view_for_test(), std::nullopt);
}

// Production code can't do this: only Scenic holds ZX_RIGHT_SIGNAL on the ownership
// event (clients get ZX_RIGHTS_BASIC), so these bits never change behind the evaluator.
// We flip them anyway because a stale bit is the only observable sign that Evaluate()
// skipped a redundant signal. This pins deduplication, not a policy of leaving wrong
// state alone, and it cannot count syscalls directly.
TEST(InputOwnershipEvaluatorTest, RedundantSignalingAvoided) {
  Snapshot snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);
  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);

  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputPlatformOwned));
  evaluator.SetOwnershipEvent(event);
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));

  // Manually clear kInputClientOwned and set kInputPlatformOwned behind the evaluator's back.
  event.signal(static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
               static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));

  // Re-evaluating with unchanged snapshot and eligibility must NOT re-issue signal syscall.
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
}

TEST(InputOwnershipEvaluatorTest, DevicePixelRatioScaling) {
  // Root has 2x DPR (local_from_world scales by 0.5x, meaning world_from_root is 2x).
  const glm::mat4 dpr_local_from_world = glm::scale(glm::mat4(1.f), glm::vec3(0.5f, 0.5f, 1.f));
  // Root bounds are 400x300 in root local coordinates.
  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);

  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
  evaluator.SetOwnershipEvent(event);

  // Quarter-screen view (200x150) with same DPR scale returns false.
  Snapshot quarter_snapshot =
      CreateTwoNodeSnapshot(400.f, 300.f, 200.f, 150.f, dpr_local_from_world, dpr_local_from_world);
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(quarter_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));

  // Full logical-size view (400x300) with same DPR scale returns true.
  Snapshot full_snapshot =
      CreateTwoNodeSnapshot(400.f, 300.f, 400.f, 300.f, dpr_local_from_world, dpr_local_from_world);
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(full_snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
}

TEST(InputOwnershipEvaluatorTest, ClippedIntermediateAncestorReturnsFalse) {
  // root (1000x1000) -> container (500x500) -> target (1000x1000).
  // Even though target has 1000x1000, container clips the viewport.
  Snapshot snapshot;
  snapshot.root = kRoot1A;
  snapshot.view_tree[kRoot1A] = NewViewNodeWithBounds(ZX_KOID_INVALID, {kNode2}, 1000.f, 1000.f);
  snapshot.view_tree[kNode2] = NewViewNodeWithBounds(kRoot1A, {kNode3}, 500.f, 500.f);
  snapshot.view_tree[kNode3] = NewViewNodeWithBounds(kNode2, {}, 1000.f, 1000.f);

  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode3);
  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
  evaluator.SetOwnershipEvent(event);

  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));
  ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
}

TEST(InputOwnershipEvaluatorTest, ScaledAndRotatedViewsAndBoundaryChecks) {
  InputOwnershipEvaluator evaluator;
  evaluator.RegisterViewRef(kNode2);
  zx::event event = CreateEvent();
  evaluator.SetOwnershipEvent(event);

  // 1. 500x500 scaled 2x in root space covering 1000x1000 -> true.
  const glm::mat4 scale_2x_world_from_node = glm::scale(glm::mat4(1.f), glm::vec3(2.f, 2.f, 1.f));
  const glm::mat4 scale_2x_local_from_world = glm::inverse(scale_2x_world_from_node);
  Snapshot scaled_snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 500.f, 500.f, glm::mat4(1.f),
                                                   scale_2x_local_from_world);
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(scaled_snapshot));

  // 2. 500x1000 rotated 90 deg with translation covering 1000x500 root -> true.
  const glm::mat4 rot90_world_from_node =
      glm::translate(glm::mat4(1.f), glm::vec3(1000.f, 0.f, 0.f)) *
      glm::rotate(glm::mat4(1.f), glm::radians(90.f), glm::vec3(0.f, 0.f, 1.f));
  const glm::mat4 rot90_local_from_world = glm::inverse(rot90_world_from_node);
  Snapshot rotated_snapshot =
      CreateTwoNodeSnapshot(1000.f, 500.f, 500.f, 1000.f, glm::mat4(1.f), rot90_local_from_world);
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(rotated_snapshot));

  // 3. Isolated boundary checks:
  // Translated right by 1.f (min_x > kEpsilon fails) -> false.
  const glm::mat4 trans_right =
      glm::inverse(glm::translate(glm::mat4(1.f), glm::vec3(1.f, 0.f, 0.f)));
  Snapshot trans_right_snapshot =
      CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f, glm::mat4(1.f), trans_right);
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(trans_right_snapshot));

  // Translated down by 1.f (min_y > kEpsilon fails) -> false.
  const glm::mat4 trans_down =
      glm::inverse(glm::translate(glm::mat4(1.f), glm::vec3(0.f, 1.f, 0.f)));
  Snapshot trans_down_snapshot =
      CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f, glm::mat4(1.f), trans_down);
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(trans_down_snapshot));

  // 999x1000 (max_x < 1000 - kEpsilon fails) -> false.
  Snapshot narrow_snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 999.f, 1000.f);
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(narrow_snapshot));

  // 1000x999 (max_y < 1000 - kEpsilon fails) -> false.
  Snapshot short_snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 999.f);
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(short_snapshot));

  // Boundary within kEpsilon (1e-3f): 999.999x999.999 translated by (+0.0005f, +0.0005f) -> true.
  // min_x = 0.0005f <= 0 + 1e-3, max_x = 999.9995f >= 1000 - 1e-3.
  const glm::mat4 trans_epsilon =
      glm::inverse(glm::translate(glm::mat4(1.f), glm::vec3(0.0005f, 0.0005f, 0.f)));
  Snapshot within_eps_snapshot =
      CreateTwoNodeSnapshot(1000.f, 1000.f, 999.999f, 999.999f, glm::mat4(1.f), trans_epsilon);
  EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(within_eps_snapshot));
}

TEST(InputOwnershipEvaluatorTest, PreAndPostRegistrationEvaluation) {
  Snapshot snapshot = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);

  // Case 1: Pre-register view, evaluate unlinked snapshot, then evaluate connected snapshot.
  // Also exercises SetOwnershipEvent() called after OnNewViewTreeSnapshot().
  {
    InputOwnershipEvaluator evaluator;
    evaluator.RegisterViewRef(kNode2);

    Snapshot root_only_snapshot;
    root_only_snapshot.root = kRoot1A;
    root_only_snapshot.view_tree[kRoot1A] =
        NewViewNodeWithBounds(ZX_KOID_INVALID, {}, 1000.f, 1000.f);
    EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(root_only_snapshot));
    EXPECT_EQ(evaluator.registered_view_for_test(), kNode2);

    // Call SetOwnershipEvent after snapshot has already been submitted:
    // Create the event with neither bit set to confirm SetOwnershipEvent() actively evaluates
    // and sets kInputPlatformOwned.
    zx::event event = CreateEvent();
    evaluator.SetOwnershipEvent(event);
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));

    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));

    // Calling SetOwnershipEvent() a second time on the same evaluator resets last_is_eligible_
    // and immediately evaluates against the new unsignaled event.
    zx::event event2 = CreateEvent();
    evaluator.SetOwnershipEvent(event2);
    ExpectSignals(event2, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
  }

  // Case 2: Evaluate connected snapshot before registration -> false, then register -> true.
  {
    InputOwnershipEvaluator evaluator;
    zx::event event = CreateEvent();
    evaluator.SetOwnershipEvent(event);
    EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));

    evaluator.RegisterViewRef(kNode2);
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
  }
}

TEST(InputOwnershipEvaluatorTest, MultiHopHierarchyAndTargetTopology) {
  // 3-level chain: root -> node2 -> node3
  Snapshot snapshot;
  snapshot.root = kRoot1A;
  snapshot.view_tree[kRoot1A] = NewViewNodeWithBounds(ZX_KOID_INVALID, {kNode2}, 1000.f, 1000.f);
  snapshot.view_tree[kNode2] = NewViewNodeWithBounds(kRoot1A, {kNode3}, 1000.f, 1000.f);
  snapshot.view_tree[kNode3] = NewViewNodeWithBounds(kNode2, {}, 1000.f, 1000.f);

  zx::event event = CreateEvent();

  // 1. node3 registered (leaf) -> true.
  {
    InputOwnershipEvaluator evaluator;
    evaluator.SetOwnershipEvent(event);
    evaluator.RegisterViewRef(kNode3);
    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
  }

  // 2. non-leaf node2 registered -> false.
  {
    InputOwnershipEvaluator evaluator;
    evaluator.SetOwnershipEvent(event);
    evaluator.RegisterViewRef(kNode2);
    EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
  }

  // 3. Registering node2 after node3 replaces node3.
  {
    InputOwnershipEvaluator evaluator;
    evaluator.SetOwnershipEvent(event);
    evaluator.RegisterViewRef(kNode3);
    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(snapshot));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));

    // Register node2: replaces node3. Since node2 is non-leaf in this tree, ownership is revoked.
    evaluator.RegisterViewRef(kNode2);
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));
    EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(snapshot));

    // In a 2-node tree, node2 is fullscreen leaf:
    Snapshot two_node = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);
    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(two_node));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));

    // Register unconnected view: ownership immediately drops to platform.
    constexpr zx_koid_t kUnconnectedView = 105;
    evaluator.RegisterViewRef(kUnconnectedView);
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned));

    // Once unconnected view connects fullscreen, ownership transitions back to client.
    Snapshot unconnected_connected;
    unconnected_connected.root = kRoot1A;
    unconnected_connected.view_tree[kRoot1A] =
        NewViewNodeWithBounds(ZX_KOID_INVALID, {kUnconnectedView}, 1000.f, 1000.f);
    unconnected_connected.view_tree[kUnconnectedView] =
        NewViewNodeWithBounds(kRoot1A, {}, 1000.f, 1000.f);
    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(unconnected_connected));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
  }

  // 4. 1-node tree where root is registered -> true.
  {
    Snapshot single_node_snapshot;
    single_node_snapshot.root = kRoot1A;
    single_node_snapshot.view_tree[kRoot1A] =
        NewViewNodeWithBounds(ZX_KOID_INVALID, {}, 1000.f, 1000.f);

    InputOwnershipEvaluator evaluator;
    evaluator.SetOwnershipEvent(event);
    evaluator.RegisterViewRef(kRoot1A);
    EXPECT_TRUE(evaluator.OnNewViewTreeSnapshot(single_node_snapshot));
    ExpectSignals(event, static_cast<uint32_t>(InputOwnershipSignal::kInputClientOwned),
                  static_cast<uint32_t>(InputOwnershipSignal::kInputPlatformOwned));
  }
}

TEST(InputOwnershipEvaluatorTest, ZeroAreaAndInvalidKoidGuards) {
  InputOwnershipEvaluator evaluator;
  zx::event event = CreateEvent();
  evaluator.SetOwnershipEvent(event);

  // RegisterViewRef(ZX_KOID_INVALID) leaves registered_view_for_test() nullopt.
  evaluator.RegisterViewRef(ZX_KOID_INVALID);
  EXPECT_EQ(evaluator.registered_view_for_test(), std::nullopt);

  evaluator.RegisterViewRef(kNode2);

  // root 0x0 -> false.
  Snapshot zero_root = CreateTwoNodeSnapshot(0.f, 0.f, 1000.f, 1000.f);
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(zero_root));

  // target 0x0 -> false.
  Snapshot zero_target = CreateTwoNodeSnapshot(1000.f, 1000.f, 0.f, 0.f);
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(zero_target));

  // snapshot.root = ZX_KOID_INVALID -> false.
  // Production never sends this snapshot: ValidateSnapshot() DCHECKs that a non-empty
  // view_tree has a valid root. This case only exercises the evaluator's defensive guard.
  // The real unrooted path is an empty snapshot, covered by EmptyTree and the unrooted
  // step of StaleKoidCleanup.
  Snapshot invalid_root = CreateTwoNodeSnapshot(1000.f, 1000.f, 1000.f, 1000.f);
  invalid_root.root = ZX_KOID_INVALID;
  EXPECT_FALSE(evaluator.OnNewViewTreeSnapshot(invalid_root));
}

}  // namespace view_tree::test
