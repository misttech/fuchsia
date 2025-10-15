// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.display.singleton/cpp/fidl.h>
#include <lib/async/cpp/task.h>
#include <lib/fit/function.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_creation_tokens.h>
#include <lib/ui/scenic/cpp/view_identity.h>

#include <zxtest/zxtest.h>

#include "src/ui/scenic/tests/utils/blocking_present.h"
#include "src/ui/scenic/tests/utils/scenic_ctf_test_base.h"

namespace integration_tests {

namespace {

using fuds_VsyncSource = fuchsia_ui_display_singleton::VsyncSource;

class VsyncSourceIntegrationTest
    : public ScenicCtfTest,
      public fidl::AsyncEventHandler<fuchsia_ui_display_singleton::VsyncSource> {
 public:
  VsyncSourceIntegrationTest() = default;

  void SetUp() override {
    ScenicCtfTest::SetUp();

    // Fake display only triggers vsyncs after a config is applied. Drawing a solid fill here to
    // trigger vsyncs.
    {
      fidl::SyncClient<fuchsia_ui_composition::FlatlandDisplay> flatland_display =
          ConnectSyncIntoRealm<fuchsia_ui_composition::FlatlandDisplay>();
      FlatlandClientWithEventHandler root_flatland(
          ConnectIntoRealm<fuchsia_ui_composition::Flatland>(), dispatcher());
      auto [child_token, parent_token] = scenic::cpp::ViewCreationTokenPair::New();
      auto child_view_watcher_endpoints =
          fidl::CreateEndpoints<fuchsia_ui_composition::ChildViewWatcher>();
      auto res = flatland_display->SetContent(
          {{.token = std::move(parent_token),
            .child_view_watcher = std::move(child_view_watcher_endpoints->server)}});
      ASSERT_TRUE(res.is_ok());
      auto parent_viewport_watcher_endpoints =
          fidl::CreateEndpoints<fuchsia_ui_composition::ParentViewportWatcher>();
      fuchsia::ui::composition::FlatlandCreateView2Request request;
      res = root_flatland->CreateView2(
          {{.token = std::move(child_token),
            .view_identity = scenic::cpp::NewViewIdentityOnCreation(),
            .parent_viewport_watcher = std::move(parent_viewport_watcher_endpoints->server)}});
      ASSERT_TRUE(res.is_ok());
      const fuchsia_ui_composition::TransformId kRootTransform = {1};
      res = root_flatland->CreateTransform(kRootTransform);
      ASSERT_TRUE(res.is_ok());
      res = root_flatland->SetRootTransform(kRootTransform);
      ASSERT_TRUE(res.is_ok());
      const fuchsia_ui_composition::ContentId kFilledRectContentId = {1};
      res = root_flatland->CreateFilledRect(kFilledRectContentId);
      ASSERT_TRUE(res.is_ok());
      res = root_flatland->SetSolidFill(
          {{.rect_id = kFilledRectContentId,
            .color = {{.red = 1.f, .green = 0.f, .blue = 0.f, .alpha = 1.f}},
            .size = {{.width = 100, .height = 100}}}});
      ASSERT_TRUE(res.is_ok());
      res = root_flatland->SetContent(
          {{.transform_id = kRootTransform, .content_id = kFilledRectContentId}});
      ASSERT_TRUE(res.is_ok());
      BlockingPresent(this, root_flatland);
      ASSERT_TRUE(res.is_ok());
    }
    vsync_source_.Bind(ConnectIntoRealm<fuds_VsyncSource>(), dispatcher(), this);
  }

  // fuchsia_ui_display_singleton::VsyncSource
  void OnVsync(fidl::Event<fuds_VsyncSource::OnVsync>& event) override {
    on_vsync_called_ = true;
    on_vsync_timestamp_ = event.timestamp();
  }

  fidl::Client<fuds_VsyncSource> vsync_source_;
  bool on_vsync_called_ = false;
  bool on_vsync_timestamp_ = 0;
};

TEST_F(VsyncSourceIntegrationTest, OnVsyncAfterEnabled) {
  auto res = vsync_source_->SetVsyncEnabled(true);
  EXPECT_TRUE(res.is_ok());

  RunLoopUntil([this] { return on_vsync_called_; });
  EXPECT_GT(on_vsync_timestamp_, 0);

  res = vsync_source_->SetVsyncEnabled(false);
  EXPECT_TRUE(res.is_ok());
}

TEST_F(VsyncSourceIntegrationTest, NoOnVsyncWhenDisabled) {
  // Run the loop for a few frames and check that we don't receive OnVsync.
  // A bit racy, but there is no easy way to test for absence of events.
  RunLoopWithTimeout(zx::msec(200));
  EXPECT_FALSE(on_vsync_called_);
}

}  // namespace

}  // namespace integration_tests
