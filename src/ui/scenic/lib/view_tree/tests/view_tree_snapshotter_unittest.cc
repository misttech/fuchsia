// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/view_tree/view_tree_snapshotter.h"

#include <fidl/fuchsia.ui.input.internal/cpp/fidl.h>
#include <lib/async-testing/test_loop.h>
#include <lib/async/default.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/event.h>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/utils/helpers.h"
#include "src/ui/scenic/lib/view_tree/input_ownership_evaluator.h"
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

ViewNode NewViewNode(zx_koid_t parent, std::unordered_set<zx_koid_t> children) {
  return ViewNode{
      .parent = parent,
      .children = children,
      .view_ref = std::make_shared<const types::ViewRef>(fuchsia_ui_views::ViewRef{}),
  };
}

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

bool IsSignalled(const zx::event& event, InputOwnershipSignal signal) {
  return utils::IsEventSignalled(event, static_cast<uint32_t>(signal));
}

// Generates a valid tree out of three subtrees: A, B and C
//  ViewTrees:           Unconnected nodes:
// -------------         -----------------------
// | A   1     |         | A 8 | B 9 | C 10 11 |
// |   /   \   |         -----------------------
// |  2     3  |
// |  |     |  |
// -------------
// |B 4  |C 6  |
// |  |  |  |  |
// |  5  |  7  |
// ------ -----
std::vector<SubtreeSnapshotGenerator> BasicTree() {
  std::vector<SubtreeSnapshotGenerator> subtree_generators;

  // A
  subtree_generators.emplace_back([] {
    auto subtree = std::make_unique<SubtreeSnapshot>();
    auto& [root, view_tree, unconnected_views, hit_tester, tree_boundaries] = *subtree;

    root = kRoot1A;
    view_tree[kRoot1A] = NewViewNode(ZX_KOID_INVALID, {kNode2, kNode3});
    view_tree[kNode2] = NewViewNode(kRoot1A, {kRoot4B});
    view_tree[kNode3] = NewViewNode(kRoot1A, {kRoot6C});
    unconnected_views = {kNode8};

    tree_boundaries.emplace(kNode2, kRoot4B);
    tree_boundaries.emplace(kNode3, kRoot6C);
    return subtree;
  });

  // B
  subtree_generators.emplace_back([] {
    auto subtree = std::make_unique<SubtreeSnapshot>();
    auto& [root, view_tree, unconnected_views, hit_tester, tree_boundaries] = *subtree;

    root = kRoot4B;
    view_tree[kRoot4B] = NewViewNode(ZX_KOID_INVALID, {kNode5});
    view_tree[kNode5] = NewViewNode(kRoot4B, {});
    unconnected_views = {kNode9};
    return subtree;
  });

  // C
  subtree_generators.emplace_back([] {
    auto subtree = std::make_unique<SubtreeSnapshot>();
    auto& [root, view_tree, unconnected_views, hit_tester, tree_boundaries] = *subtree;

    root = kRoot6C;
    view_tree[kRoot6C] = NewViewNode(ZX_KOID_INVALID, {kNode7});
    view_tree[kNode7] = NewViewNode(kRoot6C, {});
    unconnected_views = {kNode10, kNode11};
    return subtree;
  });

  return subtree_generators;
}

// Expected combined Snapshot from BasicTree() above.
Snapshot BasicTreeSnapshot() {
  Snapshot snapshot;
  snapshot.root = kRoot1A;

  {
    auto& view_tree = snapshot.view_tree;
    view_tree[kRoot1A] = NewViewNode(ZX_KOID_INVALID, {kNode2, kNode3});
    view_tree[kNode2] = NewViewNode(kRoot1A, {kRoot4B});
    view_tree[kNode3] = NewViewNode(kRoot1A, {kRoot6C});
    view_tree[kRoot4B] = NewViewNode(kNode2, {kNode5});
    view_tree[kNode5] = NewViewNode(kRoot4B, {});
    view_tree[kRoot6C] = NewViewNode(kNode3, {kNode7});
    view_tree[kNode7] = NewViewNode(kRoot6C, {});
  }

  snapshot.unconnected_views = {kNode8, kNode9, kNode10, kNode11};

  return snapshot;
}

}  // namespace

// Checks that BasicTree() gets combined to the correct Snapshot, and that the snapshot is
// correctly delivered to a subscriber.
TEST(ViewTreeSnapshotterTest, BasicTreeTest) {
  std::vector<ViewTreeSnapshotter::Subscriber> subscribers;
  bool callback_fired = false;
  subscribers.push_back(
      {.on_new_view_tree = [&callback_fired](std::shared_ptr<const Snapshot> snapshot) {
        callback_fired = true;
        const bool conversion_correct = *snapshot == BasicTreeSnapshot();
        EXPECT_TRUE(conversion_correct);
        if (!conversion_correct) {
          FX_LOGS(ERROR) << "Generated snapshot:\n"
                         << (*snapshot) << "\ndid not match expected:\n\n"
                         << BasicTreeSnapshot();
        }
      }});

  ViewTreeSnapshotter tree(BasicTree(), std::move(subscribers));

  tree.UpdateSnapshot();
  EXPECT_TRUE(callback_fired);
}

TEST(ViewTreeSnapshotterTest, MultipleSubscribers) {
  std::vector<ViewTreeSnapshotter::Subscriber> subscribers;

  std::shared_ptr<const Snapshot> snapshot1;
  subscribers.push_back(
      {.on_new_view_tree = [&snapshot1](auto snapshot) { snapshot1 = snapshot; }});
  std::shared_ptr<const Snapshot> snapshot2;
  subscribers.push_back(
      {.on_new_view_tree = [&snapshot2](auto snapshot) { snapshot2 = snapshot; }});
  std::shared_ptr<const Snapshot> snapshot3;
  subscribers.push_back(
      {.on_new_view_tree = [&snapshot3](auto snapshot) { snapshot3 = snapshot; }});

  ViewTreeSnapshotter tree(BasicTree(), std::move(subscribers));

  tree.UpdateSnapshot();
  EXPECT_TRUE(snapshot1);
  EXPECT_TRUE(snapshot2);
  EXPECT_TRUE(snapshot3);

  // Should all be pointing to the same snapshot.
  EXPECT_EQ(snapshot1, snapshot2);
  EXPECT_EQ(snapshot1, snapshot3);
}

// Check that multiple calls to UpdateSessions() are handled correctly.
TEST(ViewTreeSnapshotterTest, MultipleUpdateSessionsCalls) {
  std::vector<SubtreeSnapshotGenerator> subtrees;
  bool first_call = true;
  subtrees.emplace_back([&first_call] {
    auto subtree = std::make_unique<SubtreeSnapshot>();
    if (first_call) {
      subtree->root = kRoot1A;
      subtree->view_tree[kRoot1A] = NewViewNode(ZX_KOID_INVALID, {});
    } else {
      subtree->root = kRoot4B;
      subtree->view_tree[kRoot4B] = NewViewNode(ZX_KOID_INVALID, {});
    }
    first_call = false;
    return subtree;
  });

  std::vector<ViewTreeSnapshotter::Subscriber> subscribers;
  std::shared_ptr<const Snapshot> snapshot1;
  subscribers.push_back(
      {.on_new_view_tree = [&snapshot1](auto snapshot) { snapshot1 = snapshot; }});

  ViewTreeSnapshotter tree(std::move(subtrees), std::move(subscribers));

  tree.UpdateSnapshot();
  ASSERT_TRUE(snapshot1);
  EXPECT_EQ(snapshot1->root, kRoot1A);

  std::shared_ptr<const Snapshot> snapshot1_copy = snapshot1;
  EXPECT_EQ(snapshot1_copy, snapshot1);

  tree.UpdateSnapshot();
  EXPECT_NE(snapshot1_copy, snapshot1);
  EXPECT_EQ(snapshot1->root, kRoot4B);
}

// Test that a callback queued on a subscriber thread survives the death of ViewTreeSnapshotter.
TEST(ViewTreeSnapshotterTest, SubscriberCallbackLifetime) {
  std::vector<SubtreeSnapshotGenerator> subtrees;
  subtrees.emplace_back([] {
    auto subtree = std::make_unique<SubtreeSnapshot>();
    subtree->root = kRoot1A;
    subtree->view_tree[kRoot1A] = NewViewNode(ZX_KOID_INVALID, {});
    return subtree;
  });

  std::vector<ViewTreeSnapshotter::Subscriber> subscribers;
  std::shared_ptr<const Snapshot> snapshot1;
  int called_count = 0;
  subscribers.push_back({.on_new_view_tree = [&snapshot1, &called_count](auto snapshot) {
    snapshot1 = snapshot;
    ++called_count;
  }});

  auto tree = std::make_unique<ViewTreeSnapshotter>(std::move(subtrees), std::move(subscribers));

  tree->UpdateSnapshot();
  tree->UpdateSnapshot();
  tree.reset();

  EXPECT_EQ(called_count, 2);
  ASSERT_TRUE(snapshot1);
  EXPECT_EQ(snapshot1->root, kRoot1A);
}

TEST(ViewTreeSnapshotterTest, NoDiff) {
  auto subtree_generators = BasicTree();

  bool nodiffA = false;
  bool nodiffB = false;
  bool nodiffC = false;

  // Wrap generators so that we can control whether they produce a subtree snapshot, or "no diff".
  auto wrapper = [](SubtreeSnapshotGenerator& gen, bool nodiff) -> GeneratedSubtreeSnapshot {
    if (nodiff) {
      return SubtreeSnapshotNoDiff();
    }
    return gen();
  };
  subtree_generators[0] = [gen = std::move(subtree_generators[0]), &nodiffA, &wrapper]() mutable {
    return wrapper(gen, nodiffA);
  };
  subtree_generators[1] = [gen = std::move(subtree_generators[1]), &nodiffB, &wrapper]() mutable {
    return wrapper(gen, nodiffB);
  };
  subtree_generators[2] = [gen = std::move(subtree_generators[2]), &nodiffC, &wrapper]() mutable {
    return wrapper(gen, nodiffC);
  };

  std::vector<ViewTreeSnapshotter::Subscriber> subscribers;

  std::vector<std::shared_ptr<const Snapshot>> snapshots;
  subscribers.push_back({.on_new_view_tree = [&snapshots](auto snapshot) {
    snapshots.push_back(std::move(snapshot));
  }});

  auto tree =
      std::make_unique<ViewTreeSnapshotter>(std::move(subtree_generators), std::move(subscribers));

  tree->UpdateSnapshot();

  EXPECT_EQ(snapshots.size(), 1U);

  // If even one generator doesn't return "no diff", then a new snapshot will be generated.
  nodiffB = nodiffC = true;
  tree->UpdateSnapshot();

  EXPECT_EQ(snapshots.size(), 2U);
  EXPECT_EQ(*snapshots[0], *snapshots[1]);

  // If all generators return "no diff", then no new snapshot will be generatated.
  nodiffA = true;
  tree->UpdateSnapshot();

  EXPECT_EQ(snapshots.size(), 2U);
}

TEST(ViewTreeSnapshotterTest, ViewTreeSnapshotterWithInputOwnershipSubscriber) {
  zx::event event = CreateEvent(static_cast<uint32_t>(InputOwnershipSignal::kDisplayPlatformOwned |
                                                      InputOwnershipSignal::kInputPlatformOwned));

  InputOwnershipEvaluator evaluator;
  evaluator.SetOwnershipEvent(event);
  evaluator.RegisterViewRef(kNode2);

  int frame_index = 0;
  std::vector<SubtreeSnapshotGenerator> subtrees;
  subtrees.emplace_back([&frame_index] {
    auto subtree = std::make_unique<SubtreeSnapshot>();
    subtree->root = kRoot1A;
    if (frame_index == 0) {
      // Frame 0: single fullscreen view
      subtree->view_tree[kRoot1A] =
          NewViewNodeWithBounds(ZX_KOID_INVALID, {kNode2}, 1000.f, 1000.f);
      subtree->view_tree[kNode2] = NewViewNodeWithBounds(kRoot1A, {}, 1000.f, 1000.f);
    } else if (frame_index == 1) {
      // Frame 1: overlay added
      subtree->view_tree[kRoot1A] =
          NewViewNodeWithBounds(ZX_KOID_INVALID, {kNode2, kNode3}, 1000.f, 1000.f);
      subtree->view_tree[kNode2] = NewViewNodeWithBounds(kRoot1A, {}, 1000.f, 1000.f);
      subtree->view_tree[kNode3] = NewViewNodeWithBounds(kRoot1A, {}, 200.f, 200.f);
    } else {
      // Frame 2: overlay removed
      subtree->view_tree[kRoot1A] =
          NewViewNodeWithBounds(ZX_KOID_INVALID, {kNode2}, 1000.f, 1000.f);
      subtree->view_tree[kNode2] = NewViewNodeWithBounds(kRoot1A, {}, 1000.f, 1000.f);
    }
    return subtree;
  });

  std::vector<ViewTreeSnapshotter::Subscriber> subscribers;
  subscribers.push_back({.on_new_view_tree = [&evaluator](auto snapshot) {
    evaluator.OnNewViewTreeSnapshot(snapshot);
  }});

  ViewTreeSnapshotter tree(std::move(subtrees), std::move(subscribers));

  // Frame 0
  tree.UpdateSnapshot();
  EXPECT_TRUE(IsSignalled(event, InputOwnershipSignal::kInputClientOwned));
  EXPECT_FALSE(IsSignalled(event, InputOwnershipSignal::kInputPlatformOwned));

  // Frame 1
  frame_index = 1;
  tree.UpdateSnapshot();
  EXPECT_TRUE(IsSignalled(event, InputOwnershipSignal::kInputPlatformOwned));
  EXPECT_FALSE(IsSignalled(event, InputOwnershipSignal::kInputClientOwned));

  // Frame 2
  frame_index = 2;
  tree.UpdateSnapshot();
  EXPECT_TRUE(IsSignalled(event, InputOwnershipSignal::kInputClientOwned));
  EXPECT_FALSE(IsSignalled(event, InputOwnershipSignal::kInputPlatformOwned));
}

}  // namespace view_tree::test
