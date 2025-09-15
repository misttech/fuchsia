// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/global_matrix_data.h"

#include <cstdint>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/allocation/buffer_collection_importer.h"
#include "src/ui/scenic/lib/display/fidl_id_types.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_image_data.h"

namespace flatland::test {
namespace {

using allocation::ImageMetadata;
using ::testing::ElementsAre;
using ::testing::IsEmpty;

constexpr int kDisplayWidth = 100;
constexpr int kDisplayHeight = 100;

ImageMetadata OpaqueImage(uint64_t id) {
  return {.identifier = display::ImageId(id), .blend_mode = BlendMode::kReplace()};
}

ImageMetadata TransparentImage(uint64_t id) {
  return {.identifier = display::ImageId(id), .blend_mode = BlendMode::kPremultipliedAlpha()};
}

TEST(CullRectanglesInPlaceTest, EmptyInput) {
  GlobalRectangleVector rects;
  GlobalImageVector images;
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_THAT(rects, IsEmpty());
  EXPECT_THAT(images, IsEmpty());
}

TEST(CullRectanglesInPlaceTest, NoCulling) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}), ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};
  GlobalRectangleVector expected_rects = rects;
  GlobalImageVector expected_images = images;
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(rects, expected_rects);
  EXPECT_EQ(images, expected_images);
}

// A full-screen occluder results in culling of all images underneath.
TEST(CullRectanglesInPlaceTest, FullOcclusion) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2), OpaqueImage(3)};
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_THAT(rects, ElementsAre(ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({50, 50}, {10, 10})));
  EXPECT_THAT(images, ElementsAre(OpaqueImage(2), OpaqueImage(3)));
}

// A transparent image cannot be an occluder, even if it is full-screen.
// (compare with `FullOcclusion` test).
TEST(CullRectanglesInPlaceTest, TransparentOccluder) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), TransparentImage(2), OpaqueImage(3)};
  GlobalRectangleVector expected_rects = rects;
  GlobalImageVector expected_images = images;
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(rects, expected_rects);
  EXPECT_EQ(images, expected_images);
}

TEST(CullRectanglesInPlaceTest, PartialOcclusion) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}),
                                 ImageRect({0, 0}, {kDisplayWidth / 2, kDisplayHeight})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};
  GlobalRectangleVector expected_rects = rects;
  GlobalImageVector expected_images = images;
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(rects, expected_rects);
  EXPECT_EQ(images, expected_images);
}

TEST(CullRectanglesInPlaceTest, MultipleOccluders) {
  GlobalRectangleVector rects = {
      ImageRect({10, 10}, {20, 20}), ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
      ImageRect({20, 20}, {10, 10}), ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
      ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2), OpaqueImage(3), OpaqueImage(4),
                              OpaqueImage(5)};
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_THAT(rects, ElementsAre(ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({50, 50}, {10, 10})));
  EXPECT_THAT(images, ElementsAre(OpaqueImage(4), OpaqueImage(5)));
}

TEST(CullRectanglesInPlaceTest, WidthZeroRectFiltered) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {0, 20}), ImageRect({30, 30}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_THAT(rects, ElementsAre(ImageRect({30, 30}, {10, 10})));
  EXPECT_THAT(images, ElementsAre(OpaqueImage(2)));
}

TEST(CullRectanglesInPlaceTest, HeightZeroRectFiltered) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 0}), ImageRect({30, 30}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};
  CullRectanglesInPlace(&rects, &images, kDisplayWidth, kDisplayHeight);
  EXPECT_THAT(rects, ElementsAre(ImageRect({30, 30}, {10, 10})));
  EXPECT_THAT(images, ElementsAre(OpaqueImage(2)));
}

}  // namespace
}  // namespace flatland::test
