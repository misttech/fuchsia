// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <fuchsia/sysmem/cpp/fidl.h>
#include <fuchsia/ui/composition/cpp/fidl.h>
#include <lib/async-testing/test_loop.h>
#include <lib/syslog/cpp/macros.h>

#include <cstddef>

#include <gtest/gtest.h>

#include "../screen_capture_buffer_collection_importer.h"
#include "src/ui/lib/escher/test/common/gtest_vulkan.h"
#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/flatland/renderer/tests/common.h"
#include "src/ui/scenic/lib/flatland/renderer/vk_renderer.h"
#include "src/ui/scenic/lib/utils/helpers.h"

namespace screen_capture::test {

using allocation::BufferCollectionUsage;
using fuchsia::images2::PixelFormat;

class ScreenCaptureBufferCollectionTest : public flatland::RendererTest {
 public:
  void SetUp() {
    RendererTest::SetUp();
    renderer_ = std::make_shared<flatland::VkRenderer>(escher::test::GetEscher()->GetWeakPtr());
    importer_ = std::make_unique<ScreenCaptureBufferCollectionImporter>(
        utils::CreateSysmemAllocatorClient(loop_.dispatcher(), "SCBCTest::Setup"), renderer_);
  }

  fuchsia::sysmem2::BufferCollectionInfo CreateBufferCollectionInfoWithConstraints(
      fuchsia::sysmem2::BufferCollectionConstraints constraints,
      allocation::GlobalBufferCollectionId collection_id) {
    fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator =
        utils::CreateSysmemAllocatorClient(loop_.dispatcher(), "CreateBCInfo2WithConstraints");
    // Create Sysmem tokens.

    auto [local_token, dup_token] = utils::CreateSysmemTokensHlcpp(sysmem_allocator);

    // Import into ScreenCaptureBufferCollectionImporter.
    bool success =
        importer_->ImportBufferCollection(collection_id, sysmem_allocator, std::move(dup_token),
                                          BufferCollectionUsage::kRenderTarget, std::nullopt);
    EXPECT_TRUE(success);

    fuchsia::sysmem2::BufferCollectionSyncPtr buffer_collection;
    fidl::Arena arena;
    fidl::OneWayStatus result = sysmem_allocator->BindSharedCollection(
        fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
            .token(fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>(
                local_token.Unbind().TakeChannel()))
            .buffer_collection_request(fidl::ServerEnd<fuchsia_sysmem2::BufferCollection>(
                buffer_collection.NewRequest().TakeChannel()))
            .Build());
    EXPECT_TRUE(result.ok());

    fuchsia::sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
    set_constraints_request.set_constraints(std::move(constraints));
    zx_status_t status = buffer_collection->SetConstraints(std::move(set_constraints_request));
    EXPECT_EQ(status, ZX_OK);

    // Wait for allocation.
    fuchsia::sysmem2::BufferCollection_WaitForAllBuffersAllocated_Result wait_result;
    status = buffer_collection->WaitForAllBuffersAllocated(&wait_result);
    EXPECT_EQ(status, ZX_OK);
    EXPECT_TRUE(wait_result.is_response());
    status = buffer_collection->Release();
    EXPECT_EQ(status, ZX_OK);
    return std::move(*wait_result.response().mutable_buffer_collection_info());
  }

 protected:
  async::TestLoop loop_;
  std::shared_ptr<flatland::VkRenderer> renderer_;
  std::shared_ptr<ScreenCaptureBufferCollectionImporter> importer_;
};

class ScreenCaptureBCTestParameterized : public ScreenCaptureBufferCollectionTest,
                                         public testing::WithParamInterface<PixelFormat> {};

// TODO(https://fxbug.dev/42158284): we don't want to "warm up" render passes and pipelines for
// multiple framebuffer formats, so we allow only BGRA framebuffers.  This is supported by all
// current platforms, including the emulator.
INSTANTIATE_TEST_SUITE_P(, ScreenCaptureBCTestParameterized,
                         testing::Values(PixelFormat::B8G8R8A8));

VK_TEST_F(ScreenCaptureBufferCollectionTest, ImportAndReleaseBufferCollection) {
  // Create Sysmem tokens.
  zx_status_t status;
  fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator =
      utils::CreateSysmemAllocatorClient(loop_.dispatcher(), "SCBCTest-ImportAndReleaseBC");
  // Create Sysmem tokens.

  auto [local_token, dup_token] = utils::CreateSysmemTokensHlcpp(sysmem_allocator);

  // Import into ScreenCaptureBufferCollectionImporter.
  auto collection_id = allocation::GenerateUniqueBufferCollectionId();
  bool result =
      importer_->ImportBufferCollection(collection_id, sysmem_allocator, std::move(dup_token),
                                        BufferCollectionUsage::kRenderTarget, std::nullopt);

  EXPECT_TRUE(result);

  // Cleanup.
  importer_->ReleaseBufferCollection(collection_id, BufferCollectionUsage::kRenderTarget);
}

VK_TEST_P(ScreenCaptureBCTestParameterized, ImportBufferImage) {
  auto collection_id = allocation::GenerateUniqueBufferCollectionId();
  // Set constraints.
  const auto pixel_format = GetParam();
  const uint32_t kWidth = 32;
  const uint32_t kHeight = 32;
  const uint32_t buffer_count = 2;
  fuchsia::sysmem2::BufferCollectionConstraints constraints =
      utils::CreateDefaultConstraints(buffer_count, kWidth, kHeight);
  constraints.mutable_image_format_constraints()->at(0).set_pixel_format(pixel_format);

  CreateBufferCollectionInfoWithConstraints(std::move(constraints), collection_id);
  // Extract image into the first Session.
  allocation::ImageMetadata metadata;
  metadata.width = kWidth;
  metadata.height = kHeight;
  metadata.vmo_index = 0;
  metadata.collection_id = collection_id;
  metadata.identifier = display::ImageId(1);

  // Verify image has been imported correctly.
  bool success = importer_->ImportBufferImage(metadata, BufferCollectionUsage::kRenderTarget);
  EXPECT_TRUE(success);

  // Cleanup.
  importer_->ReleaseBufferCollection(collection_id, BufferCollectionUsage::kRenderTarget);
}

VK_TEST_P(ScreenCaptureBCTestParameterized, GetBufferCountFromCollectionId) {
  auto collection_id = allocation::GenerateUniqueBufferCollectionId();
  // Set constraints.
  const auto pixel_format = GetParam();
  const uint32_t kWidth = 32;
  const uint32_t kHeight = 32;
  const uint32_t buffer_count = 2;
  fuchsia::sysmem2::BufferCollectionConstraints constraints =
      utils::CreateDefaultConstraints(buffer_count, kWidth, kHeight);
  constraints.mutable_image_format_constraints()->at(0).set_pixel_format(pixel_format);

  fuchsia::sysmem2::BufferCollectionInfo buffer_collection_info =
      CreateBufferCollectionInfoWithConstraints(std::move(constraints), collection_id);

  std::optional<uint32_t> info = importer_->GetBufferCollectionBufferCount(collection_id);

  EXPECT_NE(info, std::nullopt);
  EXPECT_EQ(info.value(), buffer_count);

  // Cleanup.
  importer_->ReleaseBufferCollection(collection_id, BufferCollectionUsage::kRenderTarget);
}

VK_TEST_F(ScreenCaptureBufferCollectionTest, ImportBufferCollection_ErrorCases) {
  fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator =
      utils::CreateSysmemAllocatorClient(loop_.dispatcher(), "SCBCTest-ImportBC_ErrorCases");

  const auto collection_id = allocation::GenerateUniqueBufferCollectionId();

  fuchsia::sysmem2::BufferCollectionTokenSyncPtr token1;
  {
    fidl::Arena arena;
    fidl::OneWayStatus result = sysmem_allocator->AllocateSharedCollection(
        fuchsia_sysmem2::wire::AllocatorAllocateSharedCollectionRequest::Builder(arena)
            .token_request(fidl::ServerEnd<fuchsia_sysmem2::BufferCollectionToken>(
                token1.NewRequest().TakeChannel()))
            .Build());
    EXPECT_TRUE(result.ok());
  }
  bool result =
      importer_->ImportBufferCollection(collection_id, sysmem_allocator, std::move(token1),
                                        BufferCollectionUsage::kRenderTarget, std::nullopt);
  EXPECT_TRUE(result);

  // Buffer collection id dup.
  {
    fuchsia::sysmem2::BufferCollectionTokenSyncPtr token2;
    fidl::Arena arena;
    fidl::OneWayStatus result = sysmem_allocator->AllocateSharedCollection(
        fuchsia_sysmem2::wire::AllocatorAllocateSharedCollectionRequest::Builder(arena)
            .token_request(fidl::ServerEnd<fuchsia_sysmem2::BufferCollectionToken>(
                token2.NewRequest().TakeChannel()))
            .Build());
    EXPECT_TRUE(result.ok());
    bool success =
        importer_->ImportBufferCollection(collection_id, sysmem_allocator, std::move(token2),
                                          BufferCollectionUsage::kRenderTarget, std::nullopt);
    EXPECT_FALSE(success);
  }
}

VK_TEST_P(ScreenCaptureBCTestParameterized, ImportBufferImage_ErrorCases) {
  auto collection_id = allocation::GenerateUniqueBufferCollectionId();
  // Set constraints.
  const auto pixel_format = GetParam();
  const uint32_t kWidth = 32;
  const uint32_t kHeight = 32;
  const uint32_t buffer_count = 2;
  fuchsia::sysmem2::BufferCollectionConstraints constraints =
      utils::CreateDefaultConstraints(buffer_count, kWidth, kHeight);
  constraints.mutable_image_format_constraints()->at(0).set_pixel_format(pixel_format);

  fuchsia::sysmem2::BufferCollectionInfo buffer_collection_info =
      CreateBufferCollectionInfoWithConstraints(std::move(constraints), collection_id);

  zx_status_t status;
  bool result;

  // Buffer collection id mismatch.
  {
    allocation::ImageMetadata metadata;
    metadata.collection_id = allocation::GenerateUniqueBufferCollectionId();
    result = importer_->ImportBufferImage(metadata, BufferCollectionUsage::kRenderTarget);
    EXPECT_FALSE(result);
  }

  // Buffer collection id invalid.
  {
    allocation::ImageMetadata metadata;
    metadata.collection_id = 0;
    result = importer_->ImportBufferImage(metadata, BufferCollectionUsage::kRenderTarget);
    EXPECT_FALSE(result);
  }

  // Buffer collection has 0 width and height.
  {
    allocation::ImageMetadata metadata;
    metadata.collection_id = collection_id;
    metadata.width = 0;
    metadata.height = 0;
    result = importer_->ImportBufferImage(metadata, BufferCollectionUsage::kRenderTarget);
    EXPECT_FALSE(result);
  }

  // Buffer count is does not correspond with vmo_index
  {
    allocation::ImageMetadata metadata;
    metadata.collection_id = collection_id;
    metadata.width = 32;
    metadata.height = 32;
    metadata.vmo_index = 3;
    result = importer_->ImportBufferImage(metadata, BufferCollectionUsage::kRenderTarget);
    EXPECT_FALSE(result);
  }

  // Cleanup.
  importer_->ReleaseBufferCollection(collection_id, BufferCollectionUsage::kRenderTarget);
}

VK_TEST_P(ScreenCaptureBCTestParameterized, GetBufferCollectionBufferCount_ErrorCases) {
  auto collection_id = allocation::GenerateUniqueBufferCollectionId();
  // Set constraints.
  const auto pixel_format = GetParam();
  const uint32_t kWidth = 32;
  const uint32_t kHeight = 32;
  const uint32_t buffer_count = 2;
  fuchsia::sysmem2::BufferCollectionConstraints constraints =
      utils::CreateDefaultConstraints(buffer_count, kWidth, kHeight);
  constraints.mutable_image_format_constraints()->at(0).set_pixel_format(pixel_format);

  fuchsia::sysmem2::BufferCollectionInfo buffer_collection_info =
      CreateBufferCollectionInfoWithConstraints(std::move(constraints), collection_id);

  // collection_id does not exist
  {
    auto new_collection_id = allocation::GenerateUniqueBufferCollectionId();
    std::optional<uint32_t> info = importer_->GetBufferCollectionBufferCount(new_collection_id);
    EXPECT_EQ(info, std::nullopt);
  }

  // Cleanup.
  importer_->ReleaseBufferCollection(collection_id, BufferCollectionUsage::kRenderTarget);
}

VK_TEST_P(ScreenCaptureBCTestParameterized, GetBufferCollectionBufferCount_BuffersNotAllocated) {
  auto collection_id = allocation::GenerateUniqueBufferCollectionId();
  zx_status_t status;
  fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator =
      utils::CreateSysmemAllocatorClient(loop_.dispatcher(), "GetBCBC_BuffersNotAllocated");
  // Create Sysmem tokens.
  auto [local_token, dup_token] = utils::CreateSysmemTokensHlcpp(sysmem_allocator);
  // Import into ScreenCaptureBufferCollectionImporter.
  bool success =
      importer_->ImportBufferCollection(collection_id, sysmem_allocator, std::move(dup_token),
                                        BufferCollectionUsage::kRenderTarget, std::nullopt);
  EXPECT_TRUE(success);

  fuchsia::sysmem2::BufferCollectionSyncPtr buffer_collection;
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>(
              local_token.Unbind().TakeChannel()))
          .buffer_collection_request(fidl::ServerEnd<fuchsia_sysmem2::BufferCollection>(
              buffer_collection.NewRequest().TakeChannel()))
          .Build());
  EXPECT_TRUE(result.ok());

  // CheckForBuffersAllocated will return false
  std::optional<uint32_t> info = importer_->GetBufferCollectionBufferCount(collection_id);
  EXPECT_EQ(info, std::nullopt);

  // Cleanup.
  importer_->ReleaseBufferCollection(collection_id, BufferCollectionUsage::kRenderTarget);
}

}  // namespace screen_capture::test
