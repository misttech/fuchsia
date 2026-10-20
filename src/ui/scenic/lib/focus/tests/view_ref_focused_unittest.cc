// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.ui.views/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/zx/time.h>

#include <optional>

#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/test_loop_fixture.h"
#include "src/ui/scenic/lib/focus/focus_manager.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/view_tree/snapshot_types.h"

// This test exercises the implementation of the fuchsia.ui.views.ViewRefFocused protocol, which
// allows clients to listen to view-focus events.
//
// Visual geometry is not important in this test. We use the following two-node (or three-node) tree
// topology:
//   A     A
//   |     |
//   B     B
//         |
//         C
namespace {

enum : zx_koid_t { kNodeA = 1, kNodeB, kNodeC };

// Class fixture for TEST_F.
class ViewRefFocusedTest : public gtest::TestLoopFixture {
 protected:
  // Creates a snapshot with the following two-node topology:
  //     A
  //     |
  //     B
  std::shared_ptr<const view_tree::Snapshot> TwoNodeSnapshot() {
    auto snapshot = std::make_shared<view_tree::Snapshot>();
    snapshot->sequence_number = next_sequence_number_++;

    snapshot->root = kNodeA;
    auto& view_tree = snapshot->view_tree;
    view_tree[kNodeA] = view_tree::ViewNode{.parent = ZX_KOID_INVALID, .children = {kNodeB}};
    view_tree[kNodeB] = view_tree::ViewNode{.parent = kNodeA};

    return snapshot;
  }

  // Creates a snapshot with the following three-node topology:
  //     A
  //     |
  //     B
  //     |
  //     C
  std::shared_ptr<const view_tree::Snapshot> ThreeNodeSnapshot() {
    auto snapshot = std::make_shared<view_tree::Snapshot>(*TwoNodeSnapshot());
    snapshot->view_tree[kNodeB].children = {kNodeC};
    snapshot->view_tree[kNodeC] = view_tree::ViewNode{.parent = kNodeB};
    return snapshot;
  }

  ViewRefFocusedTest()
      : snapshot_holder_(std::make_shared<view_tree::SnapshotHolder>()),
        focus_manager_(dispatcher(), snapshot_holder_) {}

  void SetUp() override {
    gtest::TestLoopFixture::SetUp();
    dispatcher_setter_ =
        std::make_unique<utils::ScopedThreadDispatcherSetter>(dispatcher(), dispatcher());
    auto [client_a, server_a] = fidl::Endpoints<fuchsia_ui_views::ViewRefFocused>::Create();
    node_a_focused_.Bind(std::move(client_a), dispatcher());
    focus_manager_.RegisterViewRefFocused(kNodeA, std::move(server_a));

    auto [client_b, server_b] = fidl::Endpoints<fuchsia_ui_views::ViewRefFocused>::Create();
    node_b_focused_.Bind(std::move(client_b), dispatcher());
    focus_manager_.RegisterViewRefFocused(kNodeB, std::move(server_b));

    FX_CHECK(node_a_focused_.is_valid());
    FX_CHECK(node_b_focused_.is_valid());
  }

  void TearDown() override {
    dispatcher_setter_.reset();
    gtest::TestLoopFixture::TearDown();
  }

  std::unique_ptr<utils::ScopedThreadDispatcherSetter> dispatcher_setter_;
  std::shared_ptr<view_tree::SnapshotHolder> snapshot_holder_;
  focus::FocusManager focus_manager_;
  fidl::Client<fuchsia_ui_views::ViewRefFocused> node_a_focused_;
  fidl::Client<fuchsia_ui_views::ViewRefFocused> node_b_focused_;

 private:
  uint64_t next_sequence_number_ = 1;
};

TEST_F(ViewRefFocusedTest, NoFocus_NoResponse) {
  // No snapshots declared yet, "empty scene".

  bool called_node_a = false;
  node_a_focused_->Watch().Then([&called_node_a](auto&) { called_node_a = true; });

  bool called_node_b = false;
  node_b_focused_->Watch().Then([&called_node_b](auto&) { called_node_b = true; });

  RunLoopUntilIdle();
  EXPECT_FALSE(called_node_a);
  EXPECT_FALSE(called_node_b);
}

TEST_F(ViewRefFocusedTest, BasicTree_ParentGetsFocus) {
  snapshot_holder_->SetSnapshot(TwoNodeSnapshot());
  (void)focus_manager_.GetFocusChainForTest();  // Trigger lazy update

  std::optional<bool> node_a_focus;
  node_a_focused_->Watch().Then(
      [&node_a_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_a_focus = result->state().focused().value();
      });

  bool called_node_b = false;
  node_b_focused_->Watch().Then([&called_node_b](auto&) { called_node_b = true; });

  RunLoopUntilIdle();
  ASSERT_TRUE(node_a_focus.has_value());  // received a focus event
  EXPECT_TRUE(node_a_focus.value());      // node A has focus
  EXPECT_FALSE(called_node_b);
}

TEST_F(ViewRefFocusedTest, ChildFocus_FalseToTrue) {
  snapshot_holder_->SetSnapshot(TwoNodeSnapshot());

  // Poll after node B gains focus.
  std::optional<bool> node_b_focus;
  node_b_focused_->Watch().Then(
      [&node_b_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_b_focus = result->state().focused().value();
      });

  RunLoopUntilIdle();
  EXPECT_FALSE(node_b_focus.has_value());

  focus_manager_.RequestFocusForTest(kNodeA, kNodeB);

  RunLoopUntilIdle();
  ASSERT_TRUE(node_b_focus.has_value());
  EXPECT_TRUE(node_b_focus.value());
}

TEST_F(ViewRefFocusedTest, ChildFocus_FalseToFalse) {
  snapshot_holder_->SetSnapshot(TwoNodeSnapshot());
  focus_manager_.RequestFocusForTest(kNodeA, kNodeB);
  focus_manager_.RequestFocusForTest(kNodeA, kNodeA);

  // Poll after node B gains then loses focus.
  std::optional<bool> node_b_focus;
  node_b_focused_->Watch().Then(
      [&node_b_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_b_focus = result->state().focused().value();
      });

  RunLoopUntilIdle();
  ASSERT_TRUE(node_b_focus.has_value());
  EXPECT_FALSE(node_b_focus.value());
}

TEST_F(ViewRefFocusedTest, ChildFocus_TrueToFalse) {
  snapshot_holder_->SetSnapshot(TwoNodeSnapshot());
  focus_manager_.RequestFocusForTest(kNodeA, kNodeB);

  // First poll by node B sees focus gained.
  std::optional<bool> node_b_focus;
  node_b_focused_->Watch().Then(
      [&node_b_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_b_focus = result->state().focused().value();
      });

  RunLoopUntilIdle();
  ASSERT_TRUE(node_b_focus.has_value());
  EXPECT_TRUE(node_b_focus.value());

  // Second poll by node B sees focus lost.
  node_b_focus.reset();
  node_b_focused_->Watch().Then(
      [&node_b_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_b_focus = result->state().focused().value();
      });

  focus_manager_.RequestFocusForTest(kNodeA, kNodeA);

  RunLoopUntilIdle();
  ASSERT_TRUE(node_b_focus.has_value());
  EXPECT_FALSE(node_b_focus.value());
}

TEST_F(ViewRefFocusedTest, ChildFocus_TrueToTrue) {
  snapshot_holder_->SetSnapshot(TwoNodeSnapshot());
  focus_manager_.RequestFocusForTest(kNodeA, kNodeB);

  // First poll by node B sees focus gained.
  std::optional<bool> node_b_focus;
  node_b_focused_->Watch().Then(
      [&node_b_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_b_focus = result->state().focused().value();
      });

  RunLoopUntilIdle();
  ASSERT_TRUE(node_b_focus.has_value());
  EXPECT_TRUE(node_b_focus.value());

  // Second poll by node B sees focus lost then gained.
  node_b_focus.reset();
  node_b_focused_->Watch().Then(
      [&node_b_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_b_focus = result->state().focused().value();
      });

  focus_manager_.RequestFocusForTest(kNodeA, kNodeA);
  focus_manager_.RequestFocusForTest(kNodeA, kNodeB);

  RunLoopUntilIdle();
  ASSERT_TRUE(node_b_focus.has_value());
  EXPECT_TRUE(node_b_focus.value());
}

TEST_F(ViewRefFocusedTest, RegisterAfterSnapshot_FocusedView) {
  snapshot_holder_->SetSnapshot(ThreeNodeSnapshot());

  // Move focus to kNodeC before kNodeC registers its ViewRefFocused endpoint.
  focus_manager_.OnNewViewTreeSnapshot();
  EXPECT_EQ(focus_manager_.RequestFocusForTest(kNodeA, kNodeC), focus::FocusChangeStatus::kAccept);

  auto [client_c, server_c] = fidl::Endpoints<fuchsia_ui_views::ViewRefFocused>::Create();
  fidl::Client<fuchsia_ui_views::ViewRefFocused> node_c_focused(std::move(client_c), dispatcher());
  focus_manager_.RegisterViewRefFocused(kNodeC, std::move(server_c));

  std::optional<bool> node_c_focus;
  node_c_focused->Watch().Then(
      [&node_c_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_c_focus = result->state().focused().value();
      });

  RunLoopUntilIdle();
  ASSERT_TRUE(node_c_focus.has_value());
  EXPECT_TRUE(node_c_focus.value());
}

TEST_F(ViewRefFocusedTest, RegisterAfterSetSnapshot_UsesUpdatedFocus) {
  // 1. A view that gains focus in a newly published snapshot (via auto-focus) and registers
  // after `SetSnapshot()` but before `OnNewViewTreeSnapshot()` runs should observe `focused = true`
  // from the new snapshot.
  snapshot_holder_->SetSnapshot(TwoNodeSnapshot());
  focus_manager_.OnNewViewTreeSnapshot();
  focus_manager_.SetAutoFocusForTest(kNodeA, kNodeC);

  snapshot_holder_->SetSnapshot(ThreeNodeSnapshot());
  auto [client_c, server_c] = fidl::Endpoints<fuchsia_ui_views::ViewRefFocused>::Create();
  fidl::Client<fuchsia_ui_views::ViewRefFocused> node_c_focused(std::move(client_c), dispatcher());
  focus_manager_.RegisterViewRefFocused(kNodeC, std::move(server_c));

  std::optional<bool> node_c_focus;
  node_c_focused->Watch().Then(
      [&node_c_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_c_focus = result->state().focused().value();
      });

  RunLoopUntilIdle();
  ASSERT_TRUE(node_c_focus.has_value());
  EXPECT_TRUE(node_c_focus.value());
  focus_manager_.OnNewViewTreeSnapshot();

  // 2. A view (`kNodeD`) that was focused in the previous snapshot but loses focus in the new
  // snapshot (`is_focusable = false`) and registers after `SetSnapshot()` before
  // `OnNewViewTreeSnapshot()` runs should not be bound as focused from the stale `focus_chain_`.
  focus_manager_.SetAutoFocusForTest(kNodeA, ZX_KOID_INVALID);

  constexpr zx_koid_t kNodeD = 4;
  auto snapshot_with_d = std::make_shared<view_tree::Snapshot>(*TwoNodeSnapshot());
  snapshot_with_d->view_tree[kNodeB].children = {kNodeD};
  snapshot_with_d->view_tree[kNodeD] = view_tree::ViewNode{.parent = kNodeB};
  snapshot_holder_->SetSnapshot(snapshot_with_d);
  focus_manager_.OnNewViewTreeSnapshot();
  EXPECT_EQ(focus_manager_.RequestFocusForTest(kNodeA, kNodeD), focus::FocusChangeStatus::kAccept);

  auto unfocusable_d_snapshot = std::make_shared<view_tree::Snapshot>(*TwoNodeSnapshot());
  unfocusable_d_snapshot->view_tree[kNodeB].children = {kNodeD};
  unfocusable_d_snapshot->view_tree[kNodeD] =
      view_tree::ViewNode{.parent = kNodeB, .is_focusable = false};
  snapshot_holder_->SetSnapshot(unfocusable_d_snapshot);

  auto [client_d, server_d] = fidl::Endpoints<fuchsia_ui_views::ViewRefFocused>::Create();
  fidl::Client<fuchsia_ui_views::ViewRefFocused> node_d_focused(std::move(client_d), dispatcher());
  focus_manager_.RegisterViewRefFocused(kNodeD, std::move(server_d));

  std::optional<bool> node_d_focus;
  node_d_focused->Watch().Then(
      [&node_d_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_d_focus = result->state().focused().value();
      });

  RunLoopUntilIdle();
  EXPECT_FALSE(node_d_focus.has_value());

  focus_manager_.OnNewViewTreeSnapshot();
  RunLoopUntilIdle();
  EXPECT_FALSE(node_d_focus.has_value());
}

TEST_F(ViewRefFocusedTest, RegisterAfterSnapshot_UnfocusedView) {
  snapshot_holder_->SetSnapshot(ThreeNodeSnapshot());

  // Process the snapshot (focus is on kNodeA) before kNodeC registers its ViewRefFocused endpoint.
  focus_manager_.OnNewViewTreeSnapshot();

  auto [client_c, server_c] = fidl::Endpoints<fuchsia_ui_views::ViewRefFocused>::Create();
  fidl::Client<fuchsia_ui_views::ViewRefFocused> node_c_focused(std::move(client_c), dispatcher());
  focus_manager_.RegisterViewRefFocused(kNodeC, std::move(server_c));

  std::optional<bool> node_c_focus;
  node_c_focused->Watch().Then(
      [&node_c_focus](fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
        ASSERT_TRUE(result.is_ok());
        ASSERT_TRUE(result->state().focused().has_value());
        node_c_focus = result->state().focused().value();
      });

  // Watch() should hang because kNodeC has never been focused.
  RunLoopUntilIdle();
  EXPECT_FALSE(node_c_focus.has_value());

  // Focusing kNodeC now should resolve the pending Watch().
  EXPECT_EQ(focus_manager_.RequestFocusForTest(kNodeA, kNodeC), focus::FocusChangeStatus::kAccept);
  RunLoopUntilIdle();
  ASSERT_TRUE(node_c_focus.has_value());
  EXPECT_TRUE(node_c_focus.value());
}

TEST_F(ViewRefFocusedTest, RegisterBeforeSnapshot_UnknownViewStaysPendingUntilInSnapshot) {
  // Register an endpoint for kNodeC before it appears in any snapshot.
  auto [client_c, server_c] = fidl::Endpoints<fuchsia_ui_views::ViewRefFocused>::Create();
  fidl::Client<fuchsia_ui_views::ViewRefFocused> node_c_focused(std::move(client_c), dispatcher());
  focus_manager_.RegisterViewRefFocused(kNodeC, std::move(server_c));

  bool watch_completed = false;
  std::optional<bool> node_c_focus;
  node_c_focused->Watch().Then([&watch_completed, &node_c_focus](
                                   fidl::Result<fuchsia_ui_views::ViewRefFocused::Watch>& result) {
    watch_completed = true;
    ASSERT_TRUE(result.is_ok());
    ASSERT_TRUE(result->state().focused().has_value());
    node_c_focus = result->state().focused().value();
  });

  // Apply a snapshot without kNodeC; the pending endpoint should remain open and Watch() should not
  // error or complete.
  snapshot_holder_->SetSnapshot(TwoNodeSnapshot());
  focus_manager_.OnNewViewTreeSnapshot();
  RunLoopUntilIdle();
  EXPECT_FALSE(watch_completed);

  // Apply a snapshot containing kNodeC, focus it, and verify Watch() resolves.
  snapshot_holder_->SetSnapshot(ThreeNodeSnapshot());
  focus_manager_.OnNewViewTreeSnapshot();
  EXPECT_EQ(focus_manager_.RequestFocusForTest(kNodeA, kNodeC), focus::FocusChangeStatus::kAccept);
  RunLoopUntilIdle();
  EXPECT_TRUE(watch_completed);
  ASSERT_TRUE(node_c_focus.has_value());
  EXPECT_TRUE(node_c_focus.value());
}

}  // namespace
