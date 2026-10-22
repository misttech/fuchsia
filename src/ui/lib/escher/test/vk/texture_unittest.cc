// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/lib/escher/vk/texture.h"

#include <optional>

#include <gtest/gtest.h>

#include "src/ui/lib/escher/escher.h"
#include "src/ui/lib/escher/impl/vulkan_utils.h"
#include "src/ui/lib/escher/renderer/frame.h"
#include "src/ui/lib/escher/resources/resource_recycler.h"
#include "src/ui/lib/escher/test/common/gtest_escher.h"
#include "src/ui/lib/escher/vk/color_space.h"
#include "src/ui/lib/escher/vk/gpu_allocator.h"
#include "src/ui/lib/escher/vk/image_view.h"
#include "src/ui/lib/escher/vk/sampler.h"

namespace escher {
namespace {

using TextureTest = test::TestWithVkValidationLayer;

constexpr uint32_t kTestImageSize = 64;

std::optional<vk::Format> SelectSupportedYuvFormat(Escher* escher) {
  if (!escher->allow_ycbcr()) {
    return std::nullopt;
  }
  constexpr vk::Format kYuvFormats[] = {
      vk::Format::eG8B8R82Plane420Unorm,
      vk::Format::eG8B8G8R8422Unorm,
  };
  for (const auto format : kYuvFormats) {
    if (impl::IsYuvConversionSupported(escher->vk_physical_device(), format)) {
      return format;
    }
  }
  return std::nullopt;
}

ImagePtr AllocateTestImage(Escher* escher, vk::Format format, ColorSpace color_space,
                           bool is_mutable = false) {
  ImageInfo info;
  info.format = format;
  info.width = kTestImageSize;
  info.height = kTestImageSize;
  info.sample_count = 1;
  info.usage = vk::ImageUsageFlagBits::eSampled;
  info.memory_flags = vk::MemoryPropertyFlagBits::eDeviceLocal;
  info.color_space = color_space;
  info.is_mutable = is_mutable;
  return escher->gpu_allocator()->AllocateImage(escher->resource_recycler(), info);
}

// Verifies that destroying an immutable YUV Sampler destroys the VkSampler prior to
// destroying its VkSamplerYcbcrConversion
// (VUID-vkDestroySamplerYcbcrConversion-ycbcrConversion-01650).
VK_TEST_F(TextureTest, DestroysYuvSamplerBeforeConversion) {
  auto escher = test::GetEscher();
  auto yuv_format = SelectSupportedYuvFormat(escher);
  if (!yuv_format) {
    GTEST_SKIP() << "YCbCr conversion not supported on this device.";
  }

  auto sampler = fxl::MakeRefCounted<Sampler>(escher->resource_recycler(), *yuv_format,
                                              vk::Filter::eNearest, ColorSpace::kRec709,
                                              /*use_unnormalized_coordinates=*/false);
  ASSERT_TRUE(sampler);
  EXPECT_TRUE(sampler->is_immutable());
  EXPECT_NE(sampler->GetExtensionData(), nullptr);

  sampler.reset();
  EXPECT_VULKAN_VALIDATION_OK();
}

// Verifies that when a YUV Texture or ImageView holds the sole reference to an immutable
// Sampler and Image, destroying the Texture/ImageView destroys the VkImageView before
// releasing the Sampler (and its VkSamplerYcbcrConversion) and Image.
VK_TEST_F(TextureTest, DestroysYuvImageViewBeforeSoleOwnedSamplerConversion) {
  auto escher = test::GetEscher();
  auto yuv_format = SelectSupportedYuvFormat(escher);
  if (!yuv_format) {
    GTEST_SKIP() << "YCbCr conversion not supported on this device.";
  }

  // Case 1: Texture::New() creates an uncached immutable Sampler owned solely by the Texture.
  {
    auto image = AllocateTestImage(escher, *yuv_format, ColorSpace::kRec709);
    ASSERT_TRUE(image);

    auto texture =
        Texture::New(escher->resource_recycler(), std::move(image), vk::Filter::eNearest);
    ASSERT_TRUE(texture);
    EXPECT_TRUE(texture->is_yuv_format());
    ASSERT_TRUE(texture->sampler());
    EXPECT_TRUE(texture->sampler()->is_immutable());

    texture.reset();
    EXPECT_VULKAN_VALIDATION_OK();
  }

  // Case 2: Direct Texture construction with a sole-owned immutable SamplerPtr.
  {
    auto image = AllocateTestImage(escher, *yuv_format, ColorSpace::kRec709);
    ASSERT_TRUE(image);
    auto sampler = fxl::MakeRefCounted<Sampler>(escher->resource_recycler(), *yuv_format,
                                                vk::Filter::eNearest, ColorSpace::kRec709);
    ASSERT_TRUE(sampler);
    EXPECT_TRUE(sampler->is_immutable());

    auto texture = fxl::MakeRefCounted<Texture>(escher->resource_recycler(), std::move(sampler),
                                                std::move(image));
    ASSERT_TRUE(texture);
    EXPECT_TRUE(texture->is_yuv_format());

    texture.reset();
    EXPECT_VULKAN_VALIDATION_OK();
  }

  // Case 3: Direct ImageView::New() construction (both 3-arg and 4-arg overloads) with a
  // sole-owned immutable SamplerPtr.
  {
    auto image = AllocateTestImage(escher, *yuv_format, ColorSpace::kRec709);
    ASSERT_TRUE(image);
    auto sampler = fxl::MakeRefCounted<Sampler>(escher->resource_recycler(), *yuv_format,
                                                vk::Filter::eNearest, ColorSpace::kRec709);
    ASSERT_TRUE(sampler);

    auto view = ImageView::New(escher->resource_recycler(), std::move(image),
                               vk::ImageAspectFlagBits::eColor, std::move(sampler));
    ASSERT_TRUE(view);
    ASSERT_TRUE(view->sampler());
    EXPECT_TRUE(view->sampler()->is_immutable());

    view.reset();
    EXPECT_VULKAN_VALIDATION_OK();
  }
  {
    auto image = AllocateTestImage(escher, *yuv_format, ColorSpace::kRec709);
    ASSERT_TRUE(image);
    auto sampler = fxl::MakeRefCounted<Sampler>(escher->resource_recycler(), *yuv_format,
                                                vk::Filter::eNearest, ColorSpace::kRec709);
    ASSERT_TRUE(sampler);

    auto view =
        ImageView::New(std::move(image), vk::ImageAspectFlagBits::eColor, std::move(sampler));
    ASSERT_TRUE(view);
    ASSERT_TRUE(view->sampler());
    EXPECT_TRUE(view->sampler()->is_immutable());

    view.reset();
    EXPECT_VULKAN_VALIDATION_OK();
  }
}

// Verifies that when multiple YUV Textures share a single immutable Sampler, dropping the
// external Sampler reference early keeps the VkSamplerYcbcrConversion alive until the last
// Texture's VkImageView is destroyed.
VK_TEST_F(TextureTest, DestroysSharedYuvSamplerAfterLastTextureDestroyed) {
  auto escher = test::GetEscher();
  auto yuv_format = SelectSupportedYuvFormat(escher);
  if (!yuv_format) {
    GTEST_SKIP() << "YCbCr conversion not supported on this device.";
  }

  auto shared_sampler = fxl::MakeRefCounted<Sampler>(escher->resource_recycler(), *yuv_format,
                                                     vk::Filter::eNearest, ColorSpace::kRec709);
  ASSERT_TRUE(shared_sampler);
  EXPECT_TRUE(shared_sampler->is_immutable());

  auto image1 = AllocateTestImage(escher, *yuv_format, ColorSpace::kRec709);
  auto image2 = AllocateTestImage(escher, *yuv_format, ColorSpace::kRec709);
  ASSERT_TRUE(image1);
  ASSERT_TRUE(image2);

  auto texture1 =
      fxl::MakeRefCounted<Texture>(escher->resource_recycler(), shared_sampler, std::move(image1));
  auto texture2 =
      fxl::MakeRefCounted<Texture>(escher->resource_recycler(), shared_sampler, std::move(image2));
  EXPECT_EQ(texture1->sampler(), shared_sampler);
  EXPECT_EQ(texture2->sampler(), shared_sampler);

  // Drop the caller's Sampler reference first; both Textures still retain it.
  shared_sampler.reset();

  // Destroying the first Texture destroys its VkImageView while the second Texture still
  // retains the Sampler.
  texture1.reset();
  EXPECT_VULKAN_VALIDATION_OK();

  // Destroying the second Texture must destroy its VkImageView before releasing the last
  // reference to the shared Sampler.
  texture2.reset();
  EXPECT_VULKAN_VALIDATION_OK();
}

// Verifies that deferred destruction of an in-flight YUV Texture via ResourceRecycler
// preserves the required Vulkan destruction order once the command buffer finishes.
VK_TEST_F(TextureTest, DeferredDestructionViaResourceRecyclerPreservesOrder) {
  auto escher = test::GetEscher();
  auto yuv_format = SelectSupportedYuvFormat(escher);
  if (!yuv_format) {
    GTEST_SKIP() << "YCbCr conversion not supported on this device.";
  }

  auto frame = escher->NewFrame("TextureTest", /*frame_number=*/1, /*enable_gpu_logging=*/false,
                                CommandBuffer::Type::kTransfer);
  ASSERT_TRUE(frame);

  auto image = AllocateTestImage(escher, *yuv_format, ColorSpace::kRec709);
  ASSERT_TRUE(image);
  auto texture = Texture::New(escher->resource_recycler(), std::move(image), vk::Filter::eNearest);
  ASSERT_TRUE(texture);

  // Keep both the texture and its sampler alive for the in-flight frame command buffer, then drop
  // the caller's RefPtr so ResourceRecycler parks the Texture in unused_resources_ until the
  // command buffer finishes.
  frame->cmds()->KeepAlive(texture.get());
  frame->cmds()->KeepAlive(texture->sampler().get());
  texture.reset();

  frame->EndFrame(SemaphorePtr(), []() {});
  EXPECT_VK_SUCCESS(escher->vk_device().waitIdle());
  EXPECT_TRUE(escher->Cleanup());
  EXPECT_VULKAN_VALIDATION_OK();
}

// Verifies null Image handling and non-YUV Texture/ImageView construction and destruction.
VK_TEST_F(TextureTest, NullImageAndNonYuvTextureCornerCases) {
  auto escher = test::GetEscher();

  EXPECT_FALSE(Texture::New(escher->resource_recycler(), nullptr, vk::Filter::eNearest));

  auto rgba_image =
      AllocateTestImage(escher, vk::Format::eR8G8B8A8Unorm, ColorSpace::kSrgb, /*is_mutable=*/true);
  ASSERT_TRUE(rgba_image);

  auto image_view = ImageView::New(rgba_image);
  ASSERT_TRUE(image_view);
  EXPECT_FALSE(image_view->sampler());
  EXPECT_EQ(image_view->width(), kTestImageSize);
  EXPECT_EQ(image_view->height(), kTestImageSize);
  image_view.reset();
  EXPECT_VULKAN_VALIDATION_OK();

  auto rgba_texture =
      Texture::New(escher->resource_recycler(), std::move(rgba_image), vk::Filter::eLinear);
  ASSERT_TRUE(rgba_texture);
  EXPECT_FALSE(rgba_texture->is_yuv_format());
  ASSERT_TRUE(rgba_texture->sampler());
  EXPECT_FALSE(rgba_texture->sampler()->is_immutable());
  EXPECT_EQ(rgba_texture->sampler()->GetExtensionData(), nullptr);

  rgba_texture.reset();
  EXPECT_VULKAN_VALIDATION_OK();
}

}  // namespace
}  // namespace escher
