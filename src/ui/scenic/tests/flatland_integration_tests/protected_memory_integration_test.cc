// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/sysmem/cpp/fidl.h>
#include <fuchsia/ui/composition/cpp/fidl.h>
#include <lib/syslog/cpp/macros.h>
#include <lib/ui/scenic/cpp/view_creation_tokens.h>
#include <lib/ui/scenic/cpp/view_identity.h>

#include <zxtest/zxtest.h>

#include "src/ui/scenic/lib/allocation/buffer_collection_import_export_tokens.h"
#include "src/ui/scenic/lib/utils/helpers.h"
#include "src/ui/scenic/tests/utils/blocking_present.h"
#include "src/ui/scenic/tests/utils/scenic_ctf_test_base.h"
#include "src/ui/scenic/tests/utils/utils.h"

namespace integration_tests {

using fuchsia::ui::composition::ChildViewWatcher;
using fuchsia::ui::composition::ContentId;
using fuchsia::ui::composition::FlatlandPtr;
using fuchsia::ui::composition::ParentViewportWatcher;
using fuchsia::ui::composition::TransformId;

class ProtectedMemoryIntegrationTest : public ScenicCtfTest {
 public:
  void SetUp() override {
    ScenicCtfTest::SetUp();

    LocalServiceDirectory()->Connect(sysmem_allocator_.NewRequest());

    flatland_display_ = ConnectSyncIntoRealm<fuchsia::ui::composition::FlatlandDisplay>();

    flatland_allocator_ = ConnectSyncIntoRealm<fuchsia::ui::composition::Allocator>();

    root_flatland_ = ConnectAsyncIntoRealm<fuchsia::ui::composition::Flatland>();
    root_flatland_.set_error_handler([](zx_status_t status) {
      FX_LOGS(INFO) << "Lost connection to Scenic: " << zx_status_get_string(status);
    });

    // Attach |root_flatland_| as the only Flatland under |flatland_display_|.
    auto [child_token, parent_token] = scenic::ViewCreationTokenPair::New();
    fidl::InterfacePtr<ChildViewWatcher> child_view_watcher;
    flatland_display_->SetContent(std::move(parent_token), child_view_watcher.NewRequest());
    fidl::InterfacePtr<ParentViewportWatcher> parent_viewport_watcher;
    root_flatland_->CreateView2(std::move(child_token), scenic::NewViewIdentityOnCreation(), {},
                                parent_viewport_watcher.NewRequest());

    // Get the display's width and height. Since there is no Present in FlatlandDisplay, receiving
    // this callback ensures that all |flatland_display_| calls are processed.
    std::optional<fuchsia::ui::composition::LayoutInfo> info;
    parent_viewport_watcher->GetLayout([&info](auto result) { info = std::move(result); });
    RunLoopUntil([&info] { return info.has_value(); });
    display_width_ = info->logical_size().width;
    display_height_ = info->logical_size().height;

    screenshotter_ = ConnectSyncIntoRealm<fuchsia::ui::composition::Screenshot>();
  }

 protected:
  void SetConstraintsAndAllocateBuffer(fuchsia::sysmem2::BufferCollectionTokenSyncPtr token,
                                       bool use_protected_memory) {
    fuchsia::sysmem2::BufferCollectionSyncPtr buffer_collection;
    fuchsia::sysmem2::AllocatorBindSharedCollectionRequest bind_shared_request;
    bind_shared_request.set_token(std::move(token));
    bind_shared_request.set_buffer_collection_request(buffer_collection.NewRequest());
    auto status = sysmem_allocator_->BindSharedCollection(std::move(bind_shared_request));
    ASSERT_EQ(status, ZX_OK);
    fuchsia::sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
    auto& constraints = *set_constraints_request.mutable_constraints();
    if (use_protected_memory) {
      auto& bmc = *constraints.mutable_buffer_memory_constraints();
      bmc.set_secure_required(true);
      bmc.set_inaccessible_domain_supported(true);
      bmc.set_cpu_domain_supported(false);
      bmc.set_ram_domain_supported(false);
    }
    constraints.mutable_usage()->set_none(fuchsia::sysmem2::NONE_USAGE);
    constraints.set_min_buffer_count(1);
    uint32_t constraints_min_buffer_count = constraints.min_buffer_count();
    auto& image_constraints = constraints.mutable_image_format_constraints()->emplace_back();
    image_constraints.set_pixel_format(fuchsia::images2::PixelFormat::B8G8R8A8);
    image_constraints.mutable_color_spaces()->emplace_back(fuchsia::images2::ColorSpace::SRGB);
    image_constraints.set_required_min_size(
        fuchsia::math::SizeU{.width = display_width_, .height = display_height_});
    image_constraints.set_required_max_size(
        fuchsia::math::SizeU{.width = display_width_, .height = display_height_});
    status = buffer_collection->SetConstraints(std::move(set_constraints_request));
    ASSERT_EQ(status, ZX_OK);

    fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result wait_result;
    status = buffer_collection->WaitForAllBuffersAllocated(&wait_result);
    ASSERT_EQ(ZX_OK, status);
    ASSERT_TRUE(!wait_result.is_framework_err());
    ASSERT_TRUE(!wait_result.is_err());
    ASSERT_TRUE(wait_result.is_response());
    auto buffer_collection_info =
        std::move(*wait_result.response().mutable_buffer_collection_info());
    EXPECT_EQ(constraints_min_buffer_count, buffer_collection_info.buffers().size());
    ASSERT_EQ(ZX_OK, buffer_collection->Release());
  }

  const TransformId kRootTransform{.value = 1};
  uint32_t display_width_ = 0;
  uint32_t display_height_ = 0;

  fuchsia::sysmem2::AllocatorSyncPtr sysmem_allocator_;
  fuchsia::ui::composition::AllocatorSyncPtr flatland_allocator_;
  FlatlandPtr root_flatland_;
  fuchsia::ui::composition::ScreenshotSyncPtr screenshotter_;

 private:
  fuchsia::ui::composition::FlatlandDisplaySyncPtr flatland_display_;
};

TEST_F(ProtectedMemoryIntegrationTest, RendersProtectedImage) {
  auto [local_token, scenic_token] = utils::CreateSysmemTokens(sysmem_allocator_.get());

  // Send one token to Flatland Allocator.
  allocation::BufferCollectionImportExportTokens bc_tokens =
      allocation::BufferCollectionImportExportTokens::New();
  fuchsia::ui::composition::RegisterBufferCollectionArgs rbc_args = {};
  rbc_args.set_export_token(std::move(bc_tokens.export_token));
  rbc_args.set_buffer_collection_token2(std::move(scenic_token));
  fuchsia::ui::composition::Allocator_RegisterBufferCollection_Result result;
  flatland_allocator_->RegisterBufferCollection(std::move(rbc_args), &result);
  ASSERT_FALSE(result.is_err());

  // Use the local token to allocate a protected buffer.
  SetConstraintsAndAllocateBuffer(std::move(local_token), /*use_protected_memory=*/true);

  // Create the image in the Flatland instance.
  fuchsia::ui::composition::ImageProperties image_properties = {};
  image_properties.set_size({display_width_, display_height_});
  const ContentId kImageContentId{.value = 1};
  root_flatland_->CreateImage(kImageContentId, std::move(bc_tokens.import_token),
                              /*buffer_collection_index=*/0, std::move(image_properties));
  BlockingPresent(this, root_flatland_);

  // Present the created Image.
  root_flatland_->CreateTransform(kRootTransform);
  root_flatland_->SetRootTransform(kRootTransform);
  root_flatland_->SetContent(kRootTransform, kImageContentId);
  BlockingPresent(this, root_flatland_);

  // Verify that render happened without any errors.
}

TEST_F(ProtectedMemoryIntegrationTest, ScreenshotReplacesProtectedImage) {
  auto [local_token, scenic_token] = utils::CreateSysmemTokens(sysmem_allocator_.get());

  // Send one token to Flatland Allocator.
  allocation::BufferCollectionImportExportTokens bc_tokens =
      allocation::BufferCollectionImportExportTokens::New();
  fuchsia::ui::composition::RegisterBufferCollectionArgs rbc_args = {};
  rbc_args.set_export_token(std::move(bc_tokens.export_token));
  rbc_args.set_buffer_collection_token2(std::move(scenic_token));
  fuchsia::ui::composition::Allocator_RegisterBufferCollection_Result result;
  flatland_allocator_->RegisterBufferCollection(std::move(rbc_args), &result);
  ASSERT_FALSE(result.is_err());

  // Use the local token to allocate a protected buffer.
  SetConstraintsAndAllocateBuffer(std::move(local_token), /*use_protected_memory=*/true);

  // Create the image in the Flatland instance.
  fuchsia::ui::composition::ImageProperties image_properties = {};
  image_properties.set_size({display_width_, display_height_});
  const ContentId kImageContentId{.value = 1};
  root_flatland_->CreateImage(kImageContentId, std::move(bc_tokens.import_token),
                              /*buffer_collection_index=*/0, std::move(image_properties));
  BlockingPresent(this, root_flatland_);

  // Present the created Image.
  root_flatland_->CreateTransform(kRootTransform);
  root_flatland_->SetRootTransform(kRootTransform);
  root_flatland_->SetContent(kRootTransform, kImageContentId);
  BlockingPresent(this, root_flatland_);

  // Verify that screenshot works and replaced the content with black.
  auto screenshot = TakeScreenshot(screenshotter_, display_width_, display_height_);
  EXPECT_EQ(screenshot.Histogram()[utils::kBlack], screenshot.width() * screenshot.height());
}

}  // namespace integration_tests
