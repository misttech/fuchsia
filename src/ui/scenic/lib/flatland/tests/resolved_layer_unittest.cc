// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <gtest/gtest.h>

#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"

namespace flatland {
namespace test {

using ImageContent = ResolvedLayer::ImageContent;
using SolidColorContent = ResolvedLayer::SolidColorContent;

TEST(ComputeGlobalResolvedLayersTest, EmptyInputsYieldEmptyOutput) {
  std::vector<ImageRect> rectangles;
  std::vector<allocation::ImageMetadata> images;
  auto result = ComputeGlobalResolvedLayers(rectangles, images);
  EXPECT_TRUE(result.empty());
}

TEST(ComputeGlobalResolvedLayersTest, SingleImage) {
  std::vector<ImageRect> rectangles;
  rectangles.push_back(ImageRect(glm::vec2(10, 20), glm::vec2(100, 200)));

  std::vector<allocation::ImageMetadata> images;
  allocation::ImageMetadata meta;
  meta.identifier = display::ImageId(42);
  meta.width = 100;
  meta.height = 200;
  meta.multiply_color = {0.5f, 0.6f, 0.7f, 0.8f};
  meta.blend_mode = BlendMode::kReplace();
  meta.flip = fuchsia_ui_composition::ImageFlip::kNone;
  images.push_back(meta);

  auto result = ComputeGlobalResolvedLayers(rectangles, images);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.rect, rectangles[0]);
  EXPECT_EQ(layer.color, meta.multiply_color);
  EXPECT_EQ(layer.blend_mode, meta.blend_mode);
  EXPECT_EQ(layer.flip, meta.flip);

  ASSERT_TRUE(std::holds_alternative<ImageContent>(layer.content));
  const auto& content = std::get<ImageContent>(layer.content);
  EXPECT_EQ(content.image_id, meta.identifier);
  EXPECT_EQ(content.width, meta.width);
  EXPECT_EQ(content.height, meta.height);
}

TEST(ComputeGlobalResolvedLayersTest, PreservesOrder) {
  std::vector<ImageRect> rectangles;
  std::vector<allocation::ImageMetadata> images;

  for (uint32_t i = 1; i <= 3; ++i) {
    rectangles.push_back(ImageRect(glm::vec2(0, 0), glm::vec2(10, 10)));
    allocation::ImageMetadata meta;
    meta.identifier = display::ImageId(i);
    meta.width = 10;
    meta.height = 10;
    images.push_back(meta);
  }

  auto result = ComputeGlobalResolvedLayers(rectangles, images);
  ASSERT_EQ(result.size(), 3u);

  for (uint32_t i = 0; i < 3; ++i) {
    ASSERT_TRUE(std::holds_alternative<ImageContent>(result[i].content));
    const auto& content = std::get<ImageContent>(result[i].content);
    EXPECT_EQ(content.image_id, display::ImageId(i + 1));
  }
}

TEST(ComputeGlobalResolvedLayersTest, FilledRectBecomesSolidColorContent) {
  std::vector<ImageRect> rectangles;
  rectangles.push_back(ImageRect(glm::vec2(0, 0), glm::vec2(10, 10)));

  std::vector<allocation::ImageMetadata> images;
  allocation::ImageMetadata meta;
  meta.identifier = allocation::kInvalidImageId;
  meta.multiply_color = {0.5f, 0.25f, 1.f, 1.f};
  images.push_back(meta);

  auto result = ComputeGlobalResolvedLayers(rectangles, images);
  ASSERT_EQ(result.size(), 1u);

  // `layer.color` is used for global opacity, debugging tint, etc.
  const auto& layer = result[0];
  EXPECT_EQ(layer.color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));

  ASSERT_TRUE(std::holds_alternative<SolidColorContent>(layer.content));
  const auto& content = std::get<SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, meta.multiply_color);
}

TEST(ComputeGlobalResolvedLayersTest, MixedImageAndSolidColor) {
  std::vector<ImageRect> rectangles;
  std::vector<allocation::ImageMetadata> images;

  // 1. Image
  rectangles.push_back(ImageRect(glm::vec2(0, 0), glm::vec2(10, 10)));
  allocation::ImageMetadata meta1;
  meta1.identifier = display::ImageId(1);
  images.push_back(meta1);

  // 2. FilledRect
  rectangles.push_back(ImageRect(glm::vec2(0, 0), glm::vec2(10, 10)));
  allocation::ImageMetadata meta2;
  meta2.identifier = allocation::kInvalidImageId;
  meta2.multiply_color = {1.f, 0.f, 0.f, 1.f};
  images.push_back(meta2);

  // 3. Image
  rectangles.push_back(ImageRect(glm::vec2(0, 0), glm::vec2(10, 10)));
  allocation::ImageMetadata meta3;
  meta3.identifier = display::ImageId(3);
  images.push_back(meta3);

  auto result = ComputeGlobalResolvedLayers(rectangles, images);
  ASSERT_EQ(result.size(), 3u);

  EXPECT_TRUE(std::holds_alternative<ImageContent>(result[0].content));
  EXPECT_TRUE(std::holds_alternative<SolidColorContent>(result[1].content));
  EXPECT_TRUE(std::holds_alternative<ImageContent>(result[2].content));
}

TEST(ComputeGlobalResolvedLayersTest, CopiesBlendModeAndFlip) {
  std::vector<ImageRect> rectangles;
  rectangles.push_back(ImageRect(glm::vec2(0, 0), glm::vec2(10, 10)));

  std::vector<allocation::ImageMetadata> images;
  allocation::ImageMetadata meta;
  meta.identifier = display::ImageId(1);
  meta.blend_mode = BlendMode::kPremultipliedAlpha();
  meta.flip = fuchsia_ui_composition::ImageFlip::kLeftRight;
  images.push_back(meta);

  auto result = ComputeGlobalResolvedLayers(rectangles, images);
  ASSERT_EQ(result.size(), 1u);

  EXPECT_EQ(result[0].blend_mode, BlendMode::kPremultipliedAlpha());
  EXPECT_EQ(result[0].flip, fuchsia_ui_composition::ImageFlip::kLeftRight);
}

TEST(ResolvedLayerTest, EqualityComparesAllFields) {
  ResolvedLayer layer1;
  layer1.rect = ImageRect(glm::vec2(0, 0), glm::vec2(10, 10));
  layer1.color = {1.f, 1.f, 1.f, 1.f};
  layer1.blend_mode = BlendMode::kReplace();
  layer1.flip = fuchsia_ui_composition::ImageFlip::kNone;
  layer1.content = ImageContent{.image_id = display::ImageId(1)};

  ResolvedLayer layer2 = layer1;
  EXPECT_EQ(layer1, layer2);

  // Flip each field and verify inequality:

  // 1. rect
  layer2 = layer1;
  layer2.rect = ImageRect(glm::vec2(1, 0), glm::vec2(10, 10));
  EXPECT_NE(layer1, layer2);

  // 2. color
  layer2 = layer1;
  layer2.color = {0.f, 1.f, 1.f, 1.f};
  EXPECT_NE(layer1, layer2);

  // 3. blend_mode
  layer2 = layer1;
  layer2.blend_mode = BlendMode::kPremultipliedAlpha();
  EXPECT_NE(layer1, layer2);

  // 4. flip
  layer2 = layer1;
  layer2.flip = fuchsia_ui_composition::ImageFlip::kLeftRight;
  EXPECT_NE(layer1, layer2);

  // 5. content variant alternative type (ImageContent -> SolidColorContent)
  layer2 = layer1;
  layer2.content = SolidColorContent{.color = {1.f, 1.f, 1.f, 1.f}};
  EXPECT_NE(layer1, layer2);

  // 6. content inner fields (ImageContent image_id)
  layer2 = layer1;
  layer2.content = ImageContent{.image_id = display::ImageId(2)};
  EXPECT_NE(layer1, layer2);
}

}  // namespace test
}  // namespace flatland
