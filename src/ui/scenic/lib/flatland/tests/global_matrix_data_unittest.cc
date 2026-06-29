// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/global_matrix_data.h"

#include <cstdint>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/allocation/image_metadata.h"
#include "src/ui/scenic/lib/display/fidl_id_types.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_image_data.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"

namespace flatland::test {
namespace {

using allocation::ImageMetadata;
using allocation::kInvalidImageId;
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

TEST(CullLayersInPlaceTest, EmptyInput) {
  std::vector<ResolvedLayer> layers;
  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);
  EXPECT_THAT(layers, IsEmpty());
}

TEST(CullLayersInPlaceTest, NoCulling) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}), ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);
  auto expected_layers = layers;

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(layers, expected_layers);
}

// A full-screen occluder results in culling of all images underneath.
TEST(CullLayersInPlaceTest, FullOcclusion) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2), OpaqueImage(3)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);

  auto expected_layers = ComputeGlobalResolvedLayers(
      {ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}), ImageRect({50, 50}, {10, 10})},
      {OpaqueImage(2), OpaqueImage(3)});
  EXPECT_EQ(layers, expected_layers);
}

// A transparent image cannot be an occluder, even if it is full-screen.
// (compare with `FullOcclusion` test).
TEST(CullLayersInPlaceTest, TransparentOccluder) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), TransparentImage(2), OpaqueImage(3)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);
  auto expected_layers = layers;

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(layers, expected_layers);
}

// The culling algorithm is not smart: tt only considers full-screen occluders, even if a
// a partial-screen occluder should be able to fully occlude a layer under it.
TEST(CullLayersInPlaceTest, PartialOcclusion) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}),
                                 ImageRect({0, 0}, {kDisplayWidth / 2, kDisplayHeight})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);
  auto expected_layers = layers;

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(layers, expected_layers);
}

// If there are multiple full-screen occluders, only the latest one (and any layers above it)
// is kept; all layers below the latest full-screen occluder are culled.
TEST(CullLayersInPlaceTest, MultipleOccluders) {
  GlobalRectangleVector rects = {
      ImageRect({10, 10}, {20, 20}), ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
      ImageRect({20, 20}, {10, 10}), ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
      ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2), OpaqueImage(3), OpaqueImage(4),
                              OpaqueImage(5)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);

  auto expected_layers = ComputeGlobalResolvedLayers(
      {ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}), ImageRect({50, 50}, {10, 10})},
      {OpaqueImage(4), OpaqueImage(5)});
  EXPECT_EQ(layers, expected_layers);
}

TEST(CullLayersInPlaceTest, WidthZeroRectFiltered) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {0, 20}), ImageRect({30, 30}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);

  auto expected_layers =
      ComputeGlobalResolvedLayers({ImageRect({30, 30}, {10, 10})}, {OpaqueImage(2)});
  EXPECT_EQ(layers, expected_layers);
}

TEST(CullLayersInPlaceTest, HeightZeroRectFiltered) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 0}), ImageRect({30, 30}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);

  auto expected_layers =
      ComputeGlobalResolvedLayers({ImageRect({30, 30}, {10, 10})}, {OpaqueImage(2)});
  EXPECT_EQ(layers, expected_layers);
}

// Opaque solid-color layer culls layers beneath it.
TEST(CullLayersInPlaceTest, SolidColorFullScreenReplaceOccludes) {
  GlobalRectangleVector rects = {ImageRect({10, 10}, {20, 20}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({50, 50}, {10, 10})};
  GlobalImageVector images = {OpaqueImage(1),
                              {.identifier = kInvalidImageId, .blend_mode = BlendMode::kReplace()},
                              OpaqueImage(3)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);

  auto expected_layers = ComputeGlobalResolvedLayers(
      {ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}), ImageRect({50, 50}, {10, 10})},
      {{.identifier = kInvalidImageId, .blend_mode = BlendMode::kReplace()}, OpaqueImage(3)});
  EXPECT_EQ(layers, expected_layers);
}

// If a full-screen rect is first, the output should match the input exactly (nothing to cull).
TEST(CullLayersInPlaceTest, FullScreenRectIsFirst) {
  GlobalRectangleVector rects = {ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({10, 20}, {30, 40}), ImageRect({60, 100}, {300, 200})};
  GlobalImageVector images = {OpaqueImage(1), OpaqueImage(2), OpaqueImage(3)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);
  auto expected_layers = layers;

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(layers, expected_layers);
}

// Test where there are multiple fullscreen rects, but some of them are transparent, so
// they should not cull the rects behind them.
TEST(CullLayersInPlaceTest, MultipleFullScreenRectsWithTransparency) {
  // There are full screen rects at indices [1, 3, and 6]. Indices 3 and 6 are transparent,
  // but 1 is not. So we should ultimately only cull the rect at index 0, leaving 7 output
  // rects in total.
  GlobalRectangleVector rects = {ImageRect({10, 20}, {30, 40}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({60, 100}, {300, 200}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({60, 100}, {150, 90}),
                                 ImageRect({70, 15}, {75, 55}),
                                 ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
                                 ImageRect({80, 110}, {900, 350})};
  GlobalImageVector images = {OpaqueImage(0),      OpaqueImage(1), OpaqueImage(2),
                              TransparentImage(3), OpaqueImage(4), OpaqueImage(5),
                              TransparentImage(6), OpaqueImage(7)};

  auto layers = ComputeGlobalResolvedLayers(rects, images);

  CullLayersInPlace(&layers, kDisplayWidth, kDisplayHeight);

  auto expected_layers = ComputeGlobalResolvedLayers(
      {ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}), ImageRect({60, 100}, {300, 200}),
       ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}), ImageRect({60, 100}, {150, 90}),
       ImageRect({70, 15}, {75, 55}), ImageRect({0, 0}, {kDisplayWidth, kDisplayHeight}),
       ImageRect({80, 110}, {900, 350})},
      {OpaqueImage(1), OpaqueImage(2), TransparentImage(3), OpaqueImage(4), OpaqueImage(5),
       TransparentImage(6), OpaqueImage(7)});
  EXPECT_EQ(layers, expected_layers);
}

}  // namespace
}  // namespace flatland::test
