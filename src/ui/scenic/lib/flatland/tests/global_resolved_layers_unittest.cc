// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// This suite drives the engine scene walk using hand-built UberStructs (no Flatland sessions are
// instantiated). Flatland2 API semantics through FIDL are covered by the native Flatland2 test
// plan.  At this tier `GlobalRenderListTest.FlatlandVersionGatesImageReplace` is deliberately the
// only test case which is sensitive to `UberStruct::flatland_version`.

#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"

#include <array>
#include <cstddef>
#include <memory_resource>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/display/fidl_id_types.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/utils/pmr_stack_arena.h"

#include <glm/gtc/constants.hpp>
#include <glm/gtx/matrix_transform_2d.hpp>

using flatland::GlobalTopologyData;
using flatland::kUnclippedRegion;
using flatland::ResolveBlendAndOpacity;
using flatland::ResolvedLayer;
using flatland::SrcToDest;
using flatland::TransformClipRegion;
using flatland::TransformHandle;
using flatland::UberStruct;
using flatland::UberStructLayer;
using types::BlendMode;
using types::Rectangle;
using types::RectangleF;
using types::RotateFlip;

namespace flatland::test {
namespace {

// Convenience test helper which computes inherited opacities on the fly before calling
// flatland::ComputeGlobalResolvedLayers().
std::vector<ResolvedLayer> ComputeGlobalResolvedLayers(
    const GlobalTopologyData& topology, const UberStruct::InstanceMap& snapshot,
    const GlobalMatrixVector& global_matrices,
    const GlobalTransformClipRegionVector& clip_regions) {
  return flatland::ComputeGlobalResolvedLayers(
      topology, snapshot, global_matrices, clip_regions,
      ComputeGlobalOpacityValues(topology.topology_vector, topology.parent_indices, snapshot));
}

// Convenience test helper which computes inherited opacities on the fly before calling
// flatland::ComputeGlobalResolvedLayerStacks().
std::vector<ResolvedLayerStack> ComputeGlobalResolvedLayerStacks(
    const GlobalTopologyData& topology, const UberStruct::InstanceMap& snapshot,
    const GlobalMatrixVector& global_matrices,
    const GlobalTransformClipRegionVector& clip_regions) {
  return flatland::ComputeGlobalResolvedLayerStacks(
      topology, snapshot, global_matrices, clip_regions,
      ComputeGlobalOpacityValues(topology.topology_vector, topology.parent_indices, snapshot));
}

// Test behavior of the `ResolveBlendAndOpacity()` helper, which is used internally
// by `ComputeGlobalResolvedLayers()` to encapsulate the semantics of both the Flatland1
// and Flatland2 APIs.
TEST(ResolveBlendAndOpacityTest, ResolvesBlendAndOpacity) {
  // Matrix: stored_blend x effective_opacity {1.0, 0.5} x pin_replace {true, false}

  // 1. kReplace, opacity 1.0, pin_replace true
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kReplace(), 1.0f, /*pin_replace=*/true);
    EXPECT_EQ(blend, BlendMode::kReplace());
    EXPECT_EQ(multiply, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  }

  // 2. kReplace, opacity 1.0, pin_replace false
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kReplace(), 1.0f, /*pin_replace=*/false);
    EXPECT_EQ(blend, BlendMode::kReplace());
    EXPECT_EQ(multiply, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  }

  // 3. kReplace, opacity 0.5, pin_replace true (Flatland1 image case: blend stays REPLACE)
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kReplace(), 0.5f, /*pin_replace=*/true);
    EXPECT_EQ(blend, BlendMode::kReplace());
    EXPECT_EQ(multiply, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  }

  // 4. kReplace, opacity 0.5, pin_replace false (Demoted to PREMULTIPLIED_ALPHA)
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kReplace(), 0.5f, /*pin_replace=*/false);
    EXPECT_EQ(blend, BlendMode::kPremultipliedAlpha());
    EXPECT_EQ(multiply, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  }

  // 5. kPremultipliedAlpha, opacity 1.0, pin_replace false
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kPremultipliedAlpha(), 1.0f, /*pin_replace=*/false);
    EXPECT_EQ(blend, BlendMode::kPremultipliedAlpha());
    EXPECT_EQ(multiply, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  }

  // 6. kPremultipliedAlpha, opacity 0.5, pin_replace false
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kPremultipliedAlpha(), 0.5f, /*pin_replace=*/false);
    EXPECT_EQ(blend, BlendMode::kPremultipliedAlpha());
    EXPECT_EQ(multiply, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  }

  // 7. kStraightAlpha, opacity 1.0, pin_replace false
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kStraightAlpha(), 1.0f, /*pin_replace=*/false);
    EXPECT_EQ(blend, BlendMode::kStraightAlpha());
    EXPECT_EQ(multiply, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  }

  // 8. kStraightAlpha, opacity 0.5, pin_replace false
  {
    auto [blend, multiply] =
        ResolveBlendAndOpacity(BlendMode::kStraightAlpha(), 0.5f, /*pin_replace=*/false);
    EXPECT_EQ(blend, BlendMode::kStraightAlpha());
    EXPECT_EQ(multiply, (std::array<float, 4>{1.f, 1.f, 1.f, 0.5f}));
  }
}

// TODO(https://fxbug.dev/523371761): Consider rewriting these tests to share
// constant values between the scene specification and the expectations; the
// stored SrcToDest uses the same RectangleF/RotateFlip types as the
// UberStructLayer inputs, so e.g. the same rectangle can appear in the
// UberStruct and in the EXPECT_EQ.

TEST(GlobalRenderListTest, EmptyScene) {
  GlobalTopologyData topology;
  topology.topology_vector = {{1, 0}};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{{1, 0}, 0}};
  snapshot[1] = std::move(uber_struct);

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  EXPECT_TRUE(result.empty());
}

TEST(GlobalRenderListTest, SingleImageLayerIdentityMatrix) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
              .image_width = 100,
              .image_height = 200,
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.geometry.dest, types::RectangleF({0.f, 0.f, 100.f, 200.f}));
  EXPECT_EQ(layer.geometry.transform, types::RotateFlip::kIdentity());
  EXPECT_EQ(layer.geometry.src, types::RectangleF({0.f, 0.f, 100.f, 200.f}));
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());
  EXPECT_EQ(layer.topology_index, 0);

  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::ImageContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::ImageContent>(layer.content);
  EXPECT_EQ(content.image_id, display::ImageId(42));
  EXPECT_EQ(content.width, 100u);
  EXPECT_EQ(content.height, 200u);
}

TEST(GlobalRenderListTest, TranslationAndScaleApplyToDisplayRect) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
              .image_width = 100,
              .image_height = 200,
          },
      .common =
          {
              .display_rect = Rectangle({.x = 10, .y = 20, .width = 100, .height = 200}),
              .opacity = 1.f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  // Parent matrix translation of (5, 5) and scale of (2, 3)
  glm::mat3 T = glm::translate(glm::mat3(1.f), {5.f, 5.f});
  glm::mat3 S = glm::scale(glm::mat3(1.f), {2.f, 3.f});

  std::vector<glm::mat3> global_matrices = {T * S};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  // Expected destination:
  // display_rect.x = 10 -> transformed x = 10 * 2 + 5 = 25
  // display_rect.y = 20 -> transformed y = 20 * 3 + 5 = 65
  // display_rect.width = 100 -> transformed width = 100 * 2 = 200
  // display_rect.height = 200 -> transformed height = 200 * 3 = 600
  EXPECT_EQ(layer.geometry.dest,
            types::RectangleF({.x = 25.f, .y = 65.f, .width = 200.f, .height = 600.f}));
}

TEST(GlobalRenderListTest, Rotation90ProducesOrientationAndPermutedUVs) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 10.f, .y = 20.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
              .image_width = 500,
              .image_height = 500,
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  // Rotation of 90 degrees CCW
  float angle = glm::half_pi<float>();
  float s = sin(angle);
  float c = cos(angle);
  glm::mat3 R(1.f);
  R[0][0] = c;
  R[0][1] = s;
  R[1][0] = -s;
  R[1][1] = c;

  std::vector<glm::mat3> global_matrices = {R};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.geometry.transform, types::RotateFlip::kRotateCcw270());
  EXPECT_EQ(layer.geometry.src, types::RectangleF({10.f, 20.f, 100.f, 200.f}));
}

TEST(GlobalRenderListTest, FlipComposesWithRotation) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  // Flip LEFT_RIGHT under a 90° CCW parent
  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 10.f, .y = 20.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kReflectY(),  // LEFT_RIGHT
              .image_id = display::ImageId(42),
              .image_width = 500,
              .image_height = 500,
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  // Rotation of 90 degrees CCW on parent
  float angle = glm::half_pi<float>();
  float s = sin(angle);
  float c = cos(angle);
  glm::mat3 R(1.f);
  R[0][0] = c;
  R[0][1] = s;
  R[1][0] = -s;
  R[1][1] = c;

  std::vector<glm::mat3> global_matrices = {R};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.geometry.transform, types::RotateFlip::kRotateCcw90ReflectY());
  EXPECT_EQ(layer.geometry.src, types::RectangleF({10.f, 20.f, 100.f, 200.f}));
}

TEST(GlobalRenderListTest, ClipShrinksDstAndUVsProportionally) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
              .image_width = 100,
              .image_height = 200,
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  // Clip region cuts off the left half (x starts at 50) and bottom half (height is 100)
  std::vector<TransformClipRegion> clip_regions = {TransformClipRegion({50, 0, 50, 100})};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.geometry.dest, types::RectangleF({50.f, 0.f, 50.f, 100.f}));
  EXPECT_EQ(layer.geometry.transform, types::RotateFlip::kIdentity());
  EXPECT_EQ(layer.geometry.src, types::RectangleF({50.f, 0.f, 50.f, 100.f}));
}

TEST(GlobalRenderListTest, ClipToEmptyDropsLayer) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
              .image_width = 100,
              .image_height = 200,
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  // Clip completely outside the image bounds
  std::vector<TransformClipRegion> clip_regions = {TransformClipRegion({200, 200, 50, 50})};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  EXPECT_TRUE(result.empty());
}

TEST(GlobalRenderListTest, OpacityMultipliesDownTheChain) {
  const TransformHandle kRoot = {1, 0};
  const TransformHandle kChild = {1, 1};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot, kChild};
  topology.parent_indices = {0, 0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 1}, {kChild, 0}};
  uber_struct->local_opacity_values[kRoot] = 0.5f;

  const LayerHandle kLayer1(1, 1);
  const LayerHandle kLayer2(1, 2);
  uber_struct->layer_stacks[kChild] = {kLayer1, kLayer2};

  UberStructLayer uber_layer1{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
              .image_width = 100,
              .image_height = 200,
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 0.5f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  UberStructLayer uber_layer2{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {1.f, 1.f, 1.f, 1.f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 0.5f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer1] = uber_layer1;
  uber_struct->layers[kLayer2] = uber_layer2;
  snapshot[1] = uber_struct;

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f), glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion, kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 2u);

  // 1. Verify ImageContent layer
  //    (effective opacity 0.25 < 1.0; flatland_version 1 -> blend mode remains REPLACE)
  {
    const auto& layer = result[0];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.25f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());
    EXPECT_TRUE(std::holds_alternative<ResolvedLayer::ImageContent>(layer.content));
  }

  // 2. Verify SolidColorContent layer
  //    (effective opacity 0.25 < 1.0; demotes to PREMULTIPLIED_ALPHA)
  {
    const auto& layer = result[1];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.25f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());

    ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
    const auto& solid_content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
    EXPECT_FLOAT_EQ(solid_content.color[0], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[1], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[2], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[3], 1.f);
  }

  // For the next sub-tests, we change `flatland_version == 2` to demonstrate that image REPLACE
  // is handled differently.
  uber_struct->flatland_version = 2;
  result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 2u);

  // 3. Verify ImageContent layer
  //    (effective opacity 0.25 < 1.0; flatland_version 2 -> demotes to PREMULTIPLIED_ALPHA)
  {
    const auto& layer = result[0];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.25f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
    EXPECT_TRUE(std::holds_alternative<ResolvedLayer::ImageContent>(layer.content));
  }

  // 4. Verify SolidColorContent layer (for completeness: identical to Flatland 1)
  //    (effective opacity 0.25 < 1.0; demotes to PREMULTIPLIED_ALPHA)
  {
    const auto& layer = result[1];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.25f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.25f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());

    ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
    const auto& solid_content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
    EXPECT_FLOAT_EQ(solid_content.color[0], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[1], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[2], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[3], 1.f);
  }

  // For the next sub-tests, we change the inherited transform opacity to 1.f.  Demotion from
  // REPLACE -> PREMULTIPLIED will still occur, because per-layer opacity is still set to < 1.0
  uber_struct->local_opacity_values[kRoot] = 1.f;
  result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 2u);

  // 5. Verify ImageContent layer
  //    (effective opacity 0.5 < 1.0; flatland_version 2 -> demotes to PREMULTIPLIED_ALPHA)
  {
    const auto& layer = result[0];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.5f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.5f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.5f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.5f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
    EXPECT_TRUE(std::holds_alternative<ResolvedLayer::ImageContent>(layer.content));
  }

  // 6. Verify SolidColorContent layer (for completeness: identical to Flatland 1)
  //    (effective opacity 0.5 < 1.0; demotes to PREMULTIPLIED_ALPHA)
  {
    const auto& layer = result[1];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.5f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.5f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.5f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.5f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());

    ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
    const auto& solid_content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
    EXPECT_FLOAT_EQ(solid_content.color[0], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[1], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[2], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[3], 1.f);
  }

  // For the next sub-tests, we change per-layer opacity to 1.f.  Now, finally, demotion from
  // REPLACE -> PREMULTIPLIED will no longer occur, because the effective opacity is 1.0
  uber_struct->layers[kLayer1].common.opacity = 1.f;
  uber_struct->layers[kLayer2].common.opacity = 1.f;

  uber_struct->local_opacity_values[kRoot] = 1.f;
  result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 2u);

  // 7. Verify ImageContent layer
  //    (effective opacity 1.0; no blend mode demotion so remains REPLACE)
  {
    const auto& layer = result[0];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 1.f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 1.f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 1.f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 1.f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());
    EXPECT_TRUE(std::holds_alternative<ResolvedLayer::ImageContent>(layer.content));
  }

  // 8. Verify SolidColorContent layer
  //    (effective opacity 1.0; no blend mode demotion so remains REPLACE)
  {
    const auto& layer = result[1];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 1.f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 1.f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 1.f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 1.f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());

    ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
    const auto& solid_content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
    EXPECT_FLOAT_EQ(solid_content.color[0], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[1], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[2], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[3], 1.f);
  }

  // For the final sub-tests, for completeness we change the inherited transform opacity to 0.123f
  // This demonstrates that either layer opacity < 1 or inherited transform opacity < 1 makes the
  // effective opacity < 1, and therefore triggers demotion from REPLACE -> PREMULTIPLIED.
  uber_struct->local_opacity_values[kRoot] = 0.123f;
  result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 2u);

  // 9. Verify ImageContent layer
  //    (effective opacity 0.123 < 1.0; flatland_version 2 -> demotes to PREMULTIPLIED_ALPHA)
  {
    const auto& layer = result[0];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.123f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.123f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.123f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.123f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
    EXPECT_TRUE(std::holds_alternative<ResolvedLayer::ImageContent>(layer.content));
  }

  // 10. Verify SolidColorContent layer (for completeness: identical to Flatland 1)
  //     (effective opacity 0.123 < 1.0; demotes to PREMULTIPLIED_ALPHA)
  {
    const auto& layer = result[1];
    EXPECT_FLOAT_EQ(layer.multiply_color[0], 0.123f);
    EXPECT_FLOAT_EQ(layer.multiply_color[1], 0.123f);
    EXPECT_FLOAT_EQ(layer.multiply_color[2], 0.123f);
    EXPECT_FLOAT_EQ(layer.multiply_color[3], 0.123f);
    EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());

    ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
    const auto& solid_content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
    EXPECT_FLOAT_EQ(solid_content.color[0], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[1], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[2], 1.f);
    EXPECT_FLOAT_EQ(solid_content.color[3], 1.f);
  }
}

TEST(GlobalRenderListTest, EffectiveOpacityCombinesLayerAndInheritedOpacity) {
  const TransformHandle kRoot = {1, 0};
  const TransformHandle kChild = {1, 1};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot, kChild};
  topology.parent_indices = {0, 0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 1}, {kChild, 0}};
  uber_struct->local_opacity_values[kRoot] = 0.5f;

  const LayerHandle kImageLayer(1, 1);
  const LayerHandle kSolidLayer(1, 2);
  uber_struct->layer_stacks[kChild] = {kImageLayer, kSolidLayer};

  uber_struct->layers[kImageLayer] = UberStructLayer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 0.5f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };

  uber_struct->layers[kSolidLayer] = UberStructLayer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {0.5f, 0.25f, 1.f, 0.8f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 0.5f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };
  snapshot[1] = uber_struct;

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f), glm::mat3(1.f)},
                                            {kUnclippedRegion, kUnclippedRegion});
  ASSERT_EQ(result.size(), 2u);

  // Assert both layers receive effective opacity = 0.5 * 0.5 = 0.25
  EXPECT_EQ(result[0].multiply_color, (std::array<float, 4>{0.25f, 0.25f, 0.25f, 0.25f}));
  EXPECT_EQ(result[1].multiply_color, (std::array<float, 4>{0.25f, 0.25f, 0.25f, 0.25f}));

  // Solid content color is premultiplied by its own alpha (0.8)
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(result[1].content));
  const auto& solid_content = std::get<ResolvedLayer::SolidColorContent>(result[1].content);
  EXPECT_FLOAT_EQ(solid_content.color[0], 0.4f);
  EXPECT_FLOAT_EQ(solid_content.color[1], 0.2f);
  EXPECT_FLOAT_EQ(solid_content.color[2], 0.8f);
  EXPECT_FLOAT_EQ(solid_content.color[3], 0.8f);
}

TEST(GlobalRenderListTest, InvisibleLayersSkipped) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  // 1. Empty display_rect
  {
    UberStruct::InstanceMap snapshot;
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};
    const LayerHandle kLayer1(1, 1);
    const LayerHandle kLayer2(1, 2);
    uber_struct->layer_stacks[kRoot] = {kLayer1, kLayer2};
    UberStructLayer uber_layer1{
        .content =
            UberStructLayer::ImageModeProperties{
                .transform = RotateFlip::kIdentity(),
                .image_id = display::ImageId(42),
            },
        // width is 0, therefore rect is considered empty.
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 0, .height = 200}),
                .opacity = 1.f,
            },
    };
    UberStructLayer uber_layer2{
        .content =
            UberStructLayer::SolidColorModeProperties{
                .color = {1.f, 1.f, 1.f, 1.f},
            },
        // height is 0, therefore rect is considered empty.
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 0}),
                .opacity = 1.f,
            },
    };
    uber_struct->layers[kLayer1] = uber_layer1;
    uber_struct->layers[kLayer2] = uber_layer2;
    snapshot[1] = std::move(uber_struct);
    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    EXPECT_TRUE(result.empty());
  }

  // 2. Opacity = 0
  {
    UberStruct::InstanceMap snapshot;
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};
    const LayerHandle kLayer1(1, 1);
    const LayerHandle kLayer2(1, 2);
    uber_struct->layer_stacks[kRoot] = {kLayer1, kLayer2};
    UberStructLayer uber_layer1{
        .content =
            UberStructLayer::ImageModeProperties{
                .transform = RotateFlip::kIdentity(),
                .image_id = display::ImageId(42),
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
                .opacity = 0.f,
            },
    };
    UberStructLayer uber_layer2{
        .content =
            UberStructLayer::SolidColorModeProperties{
                .color = {1.f, 1.f, 1.f, 1.f},
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
                .opacity = 0.f,
                .blend_mode = BlendMode::kReplace(),
            },
    };
    uber_struct->layers[kLayer1] = uber_layer1;
    uber_struct->layers[kLayer2] = uber_layer2;
    snapshot[1] = std::move(uber_struct);
    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    EXPECT_TRUE(result.empty());
  }

  // 3. Unbound image (image_id = kInvalidImageId)
  {
    UberStruct::InstanceMap snapshot;
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};
    const LayerHandle kLayer(1, 1);
    uber_struct->layer_stacks[kRoot] = {kLayer};
    UberStructLayer uber_layer{
        .content =
            UberStructLayer::ImageModeProperties{
                .transform = RotateFlip::kIdentity(),
                .image_id = allocation::kInvalidImageId,
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
                .opacity = 1.f,
            },
    };
    uber_struct->layers[kLayer] = uber_layer;
    snapshot[1] = std::move(uber_struct);
    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    EXPECT_TRUE(result.empty());
  }

  // 4. Hole-punch non-skip: kReplace solid with color.a == 0 and opacity == 1 is emitted verbatim.
  // Written alpha is still the authored 0; premultiplying zeros the RGB, which prevents non-zero
  // RGB at alpha 0 from additively tinting the underlay the punch is supposed to reveal.
  {
    UberStruct::InstanceMap snapshot;
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};
    const LayerHandle kLayer(1, 1);
    uber_struct->layer_stacks[kRoot] = {kLayer};
    UberStructLayer uber_layer{
        .content =
            UberStructLayer::SolidColorModeProperties{
                .color = {0.5f, 0.25f, 1.f, 0.f},
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
                .opacity = 1.f,
                .blend_mode = BlendMode::kReplace(),
            },
    };
    uber_struct->layers[kLayer] = uber_layer;
    snapshot[1] = std::move(uber_struct);
    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    ASSERT_EQ(result.size(), 1u);
    const auto& layer = result[0];
    EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());
    ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
    const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
    EXPECT_FLOAT_EQ(content.color[0], 0.f);
    EXPECT_FLOAT_EQ(content.color[1], 0.f);
    EXPECT_FLOAT_EQ(content.color[2], 0.f);
    EXPECT_FLOAT_EQ(content.color[3], 0.f);
  }
}

TEST(GlobalRenderListTest, InheritedOpacityZeroSkipsLayers) {
  const TransformHandle kRoot = {1, 0};
  const TransformHandle kChild = {1, 1};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot, kChild};
  topology.parent_indices = {0, 0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 1}, {kChild, 0}};
  uber_struct->local_opacity_values[kRoot] = 0.f;

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kChild] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {1.f, 1.f, 1.f, 1.f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = uber_struct;

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f), glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion, kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  EXPECT_TRUE(result.empty());
}

TEST(GlobalRenderListTest, StackZOrderIsBackToFront) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  // Three layers in stack
  const LayerHandle kLayer1(1, 1);
  const LayerHandle kLayer2(1, 2);
  const LayerHandle kLayer3(1, 3);
  uber_struct->layer_stacks[kRoot] = {kLayer1, kLayer2, kLayer3};

  const auto make_layer = [](display::ImageId image_id) {
    UberStructLayer layer;
    layer.content = UberStructLayer::ImageModeProperties{
        .transform = RotateFlip::kIdentity(),
        .image_id = image_id,
    };
    layer.common.display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200});
    layer.common.opacity = 1.f;
    return layer;
  };

  uber_struct->layers[kLayer1] = make_layer(display::ImageId(11));
  uber_struct->layers[kLayer2] = make_layer(display::ImageId(22));
  uber_struct->layers[kLayer3] = make_layer(display::ImageId(33));
  snapshot[1] = std::move(uber_struct);

  auto result =
      ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
  ASSERT_EQ(result.size(), 3u);
  // Emits back-to-front (first layer in stack is furthest back, renders first)
  EXPECT_EQ(std::get<ResolvedLayer::ImageContent>(result[0].content).image_id,
            display::ImageId(11));
  EXPECT_EQ(std::get<ResolvedLayer::ImageContent>(result[1].content).image_id,
            display::ImageId(22));
  EXPECT_EQ(std::get<ResolvedLayer::ImageContent>(result[2].content).image_id,
            display::ImageId(33));
}

TEST(GlobalRenderListTest, DagInstancingEmitsPerPath) {
  const TransformHandle kParent1 = {1, 0};
  const TransformHandle kParent2 = {1, 1};
  const TransformHandle kChild = {1, 2};

  GlobalTopologyData topology;
  topology.topology_vector = {kParent1, kChild, kParent2, kChild};
  topology.parent_indices = {0, 0, 0, 2};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kParent1, 1}, {kChild, 0}, {kParent2, 1}, {kChild, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kChild] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.f,
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  // Two different parent matrices
  glm::mat3 M1 = glm::translate(glm::mat3(1.f), {10.f, 0.f});
  glm::mat3 M2 = glm::translate(glm::mat3(1.f), {50.f, 0.f});

  std::vector<glm::mat3> global_matrices = {M1, M1, M2, M2};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion, kUnclippedRegion,
                                                   kUnclippedRegion, kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  // Emits twice (once per topological index of child)
  ASSERT_EQ(result.size(), 2u);

  EXPECT_EQ(result[0].geometry.dest,
            types::RectangleF({.x = 10.f, .y = 0.f, .width = 100.f, .height = 200.f}));
  EXPECT_EQ(result[0].topology_index, 1);

  EXPECT_EQ(result[1].geometry.dest,
            types::RectangleF({.x = 50.f, .y = 0.f, .width = 100.f, .height = 200.f}));
  EXPECT_EQ(result[1].topology_index, 3);
}

// Pins that a solid layer with REPLACE blend mode and effective opacity < 1 is demoted to
// PREMULTIPLIED_ALPHA.
TEST(GlobalRenderListTest, SolidColorLayer_DemotedReplace) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {0.5f, 0.25f, 1.f, 0.8f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 0.5f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());

  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  // Content color is premultiplied by its own alpha (0.8):
  EXPECT_FLOAT_EQ(content.color[0], 0.4f);
  EXPECT_FLOAT_EQ(content.color[1], 0.2f);
  EXPECT_FLOAT_EQ(content.color[2], 0.8f);
  EXPECT_FLOAT_EQ(content.color[3], 0.8f);
}

// Pins that a solid layer with REPLACE blend mode and effective opacity == 1 emits surviving
// REPLACE blend mode with premultiplied content color.
TEST(GlobalRenderListTest, SolidColorLayer_SurvivingReplace) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {0.5f, 0.25f, 1.f, 0.8f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.0f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());

  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  // Content color is premultiplied by its own alpha (0.8):
  EXPECT_FLOAT_EQ(content.color[0], 0.4f);
  EXPECT_FLOAT_EQ(content.color[1], 0.2f);
  EXPECT_FLOAT_EQ(content.color[2], 0.8f);
  EXPECT_FLOAT_EQ(content.color[3], 0.8f);
}

// A hole punch: content alpha 0 under REPLACE. The layer is emitted
// (invisibility keys on layer and inherited opacity, never on content
// alpha), and premultiplication by content alpha zeroes the RGB
// channels, so the authored color is irrelevant: {0,0,0,0} is written
// verbatim, cutting a transparent hole for an underlay.
TEST(GlobalRenderListTest, SolidColorLayer_AlphaZeroPunch) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {0.5f, 0.25f, 1.f, 0.f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 1.0f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = std::move(uber_struct);

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, global_matrices, clip_regions);
  ASSERT_EQ(result.size(), 1u);

  const auto& layer = result[0];
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());

  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_FLOAT_EQ(content.color[0], 0.f);
  EXPECT_FLOAT_EQ(content.color[1], 0.f);
  EXPECT_FLOAT_EQ(content.color[2], 0.f);
  EXPECT_FLOAT_EQ(content.color[3], 0.f);
}

TEST(GlobalRenderListTest, SolidColorLayer_StraightAlphaNormalized) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  const LayerHandle kLayer(1, 1);
  const Rectangle kDisplayRect({.x = 0, .y = 0, .width = 100, .height = 200});
  const UberStructLayer::SolidColorModeProperties kSolidColor{.color = {1.f, 0.f, 0.f, 0.5f}};
  const float kOpacity = 0.5f;

  UberStruct::InstanceMap straight_snapshot;
  {
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};
    uber_struct->layer_stacks[kRoot] = {kLayer};
    uber_struct->layers[kLayer] = UberStructLayer{
        .content = kSolidColor,
        .common =
            {
                .display_rect = kDisplayRect,
                .opacity = kOpacity,
                .blend_mode = BlendMode::kStraightAlpha(),
            },
    };
    straight_snapshot[1] = std::move(uber_struct);
  }

  UberStruct::InstanceMap premul_snapshot;
  {
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};
    uber_struct->layer_stacks[kRoot] = {kLayer};
    uber_struct->layers[kLayer] = UberStructLayer{
        .content = kSolidColor,
        .common =
            {
                .display_rect = kDisplayRect,
                .opacity = kOpacity,
                .blend_mode = BlendMode::kPremultipliedAlpha(),
            },
    };
    premul_snapshot[1] = std::move(uber_struct);
  }

  std::vector<glm::mat3> global_matrices = {glm::mat3(1.f)};
  std::vector<TransformClipRegion> clip_regions = {kUnclippedRegion};

  auto straight_result =
      ComputeGlobalResolvedLayers(topology, straight_snapshot, global_matrices, clip_regions);
  ASSERT_EQ(straight_result.size(), 1u);

  auto premul_result =
      ComputeGlobalResolvedLayers(topology, premul_snapshot, global_matrices, clip_regions);
  ASSERT_EQ(premul_result.size(), 1u);

  // Directly assert the FIDL contract: STRAIGHT_ALPHA composites identically to
  // PREMULTIPLIED_ALPHA for solid-color content.
  EXPECT_EQ(straight_result[0], premul_result[0]);

  // For readable failures, also assert on the straight-alpha result directly.
  const auto& resolved_layer = straight_result[0];
  EXPECT_EQ(resolved_layer.blend_mode, BlendMode::kPremultipliedAlpha());
  EXPECT_EQ(resolved_layer.multiply_color, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));

  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(resolved_layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(resolved_layer.content);
  // Content color is premultiplied by its own alpha (0.5):
  EXPECT_FLOAT_EQ(content.color[0], 0.5f);
  EXPECT_FLOAT_EQ(content.color[1], 0.f);
  EXPECT_FLOAT_EQ(content.color[2], 0.f);
  EXPECT_FLOAT_EQ(content.color[3], 0.5f);
}

TEST(GlobalRenderListDeathTest, DanglingLayerHandleDies) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_unique<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};
  // Intentionally omit putting kLayer in uber_struct->layers.

  snapshot[1] = std::move(uber_struct);

  EXPECT_DEATH(
      ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion}), "");
}

TEST(GlobalRenderListTest, MultipleSessionsMerge) {
  const TransformHandle kRoot1 = {1, 0};
  const TransformHandle kRoot2 = {2, 0};

  GlobalTopologyData topology;
  topology.topology_vector = {kRoot1, kRoot2};
  topology.parent_indices = {0, 0};

  UberStruct::InstanceMap snapshot;

  // Session 1
  {
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot1, 0}};
    const LayerHandle kLayer(1, 1);
    uber_struct->layer_stacks[kRoot1] = {kLayer};
    UberStructLayer uber_layer{
        .content =
            UberStructLayer::ImageModeProperties{
                .transform = RotateFlip::kIdentity(),
                .image_id = display::ImageId(11),
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
                .opacity = 1.f,
            },
    };
    uber_struct->layers[kLayer] = uber_layer;
    snapshot[1] = std::move(uber_struct);
  }

  // Session 2
  {
    auto uber_struct = std::make_unique<UberStruct>();
    uber_struct->local_topology = {{kRoot2, 0}};
    const LayerHandle kLayer(2, 1);
    uber_struct->layer_stacks[kRoot2] = {kLayer};
    UberStructLayer uber_layer{
        .content =
            UberStructLayer::ImageModeProperties{
                .transform = RotateFlip::kIdentity(),
                .image_id = display::ImageId(22),
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
                .opacity = 1.f,
            },
    };
    uber_struct->layers[kLayer] = uber_layer;
    snapshot[2] = std::move(uber_struct);
  }

  auto result = ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f), glm::mat3(1.f)},
                                            {kUnclippedRegion, kUnclippedRegion});
  ASSERT_EQ(result.size(), 2u);
  EXPECT_EQ(std::get<ResolvedLayer::ImageContent>(result[0].content).image_id,
            display::ImageId(11));
  EXPECT_EQ(std::get<ResolvedLayer::ImageContent>(result[1].content).image_id,
            display::ImageId(22));
}

TEST(ResolvedLayerTest, EqualityComparesAllFields) {
  ResolvedLayer layer1;
  layer1.geometry = SrcToDest(types::RectangleF({0, 0, 10, 10}));
  layer1.multiply_color = {1.f, 1.f, 1.f, 1.f};
  layer1.blend_mode = BlendMode::kReplace();
  layer1.content = ResolvedLayer::ImageContent{.image_id = display::ImageId(1)};

  ResolvedLayer layer2 = layer1;
  EXPECT_EQ(layer1, layer2);

  // Flip each field and verify inequality:

  // 1. geometry
  layer2 = layer1;
  layer2.geometry = SrcToDest(types::RectangleF({1, 0, 10, 10}));
  EXPECT_NE(layer1, layer2);

  // 2. color
  layer2 = layer1;
  layer2.multiply_color = {0.f, 1.f, 1.f, 1.f};
  EXPECT_NE(layer1, layer2);

  // 3. blend_mode
  layer2 = layer1;
  layer2.blend_mode = BlendMode::kPremultipliedAlpha();
  EXPECT_NE(layer1, layer2);

  // 4. content variant alternative type (ImageContent -> SolidColorContent)
  layer2 = layer1;
  layer2.content = ResolvedLayer::SolidColorContent{.color = {1.f, 1.f, 1.f, 1.f}};
  EXPECT_NE(layer1, layer2);

  // 5. content inner fields (ImageContent image_id)
  layer2 = layer1;
  layer2.content = ResolvedLayer::ImageContent{.image_id = display::ImageId(2)};
  EXPECT_NE(layer1, layer2);

  // 6. topology_index
  layer2 = layer1;
  layer2.topology_index = 42;
  EXPECT_NE(layer1, layer2);
}

// The layer stage reads transform state only from its `ResolvedLayerStack` entries
// and layer state only from the fresh snapshot.  So running it on entries built from
// one snapshot and a later snapshot, in which only layer properties or a stack's
// layer list changed, must match a full rebuild from the later snapshot.  Both sides
// run the same two stages here; when entries are cached across frames, this equivalence
// catches an entry that carries layer-derived data.
TEST(LayerStageTest, LayerOnlyChangesMatchFullRebuild) {
  const TransformHandle kRoot = {1, 0};
  const TransformHandle kChild = {1, 1};

  GlobalTopologyData topology;
  topology.topology_vector = {kRoot, kChild};
  topology.parent_indices = {0, 0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 1}, {kChild, 0}};

  const LayerHandle kLayer1(1, 1);
  const LayerHandle kLayer2(1, 2);
  uber_struct->layer_stacks[kChild] = {kLayer1, kLayer2};
  uber_struct->layers[kLayer1] = UberStructLayer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kRotateCcw90(),
              .image_id = display::ImageId(10),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 10, .y = 20, .width = 50, .height = 80}),
              .opacity = 0.8f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };
  uber_struct->layers[kLayer2] = UberStructLayer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {1.f, 0.f, 0.f, 1.f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 40, .height = 40}),
              .opacity = 0.5f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };
  snapshot[1] = uber_struct;

  glm::mat3 child_matrix = glm::translate(glm::mat3(1.f), {100.f, 200.f}) *
                           glm::rotate(glm::mat3(1.f), -glm::half_pi<float>());
  GlobalMatrixVector matrices = {glm::mat3(1.f), child_matrix};
  GlobalTransformClipRegionVector clips = {
      kUnclippedRegion,
      types::Rectangle({.x = 0, .y = 0, .width = 500, .height = 500}),
  };
  GlobalOpacityVector opacities = {1.0f, 0.75f};

  // Build initial `ResolvedLayerStack` list once.
  const auto stacks =
      ComputeGlobalResolvedLayerStacks(topology, snapshot, matrices, clips, opacities);
  ASSERT_EQ(stacks.size(), 1u);
  EXPECT_EQ(stacks[0].handle, kChild);
  EXPECT_EQ(stacks[0].topology_index, 1);
  EXPECT_EQ(stacks[0].node_rotation, RotateFlip::kRotateCcw90());

  // Mutate only layer properties in a new UberStruct snapshot without rebuilding `stacks`.
  auto updated_uber = std::make_shared<UberStruct>();
  updated_uber->local_topology = uber_struct->local_topology;
  updated_uber->layer_stacks = uber_struct->layer_stacks;
  updated_uber->layers[kLayer1] = UberStructLayer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 5.f, .y = 10.f, .width = 80.f, .height = 120.f}),
              .transform = RotateFlip::kReflectX(),
              .image_id = display::ImageId(99),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 15, .y = 25, .width = 60, .height = 90}),
              .opacity = 0.6f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };
  updated_uber->layers[kLayer2] = UberStructLayer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {0.f, 1.f, 0.5f, 1.f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 5, .y = 5, .width = 30, .height = 30}),
              .opacity = 0.9f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };
  UberStruct::InstanceMap updated_snapshot;
  updated_snapshot[1] = updated_uber;

  const auto from_cached_stacks = ComputeGlobalResolvedLayers(stacks, updated_snapshot);
  const auto from_full_rebuild =
      ComputeGlobalResolvedLayers(topology, updated_snapshot, matrices, clips, opacities);
  ASSERT_EQ(from_cached_stacks.size(), 2u);
  EXPECT_EQ(from_cached_stacks, from_full_rebuild);

  // Reorder the stack in a later snapshot, still without rebuilding `stacks`.
  auto reordered_uber = std::make_shared<UberStruct>();
  reordered_uber->local_topology = uber_struct->local_topology;
  reordered_uber->layer_stacks[kChild] = {kLayer2, kLayer1};
  reordered_uber->layers = updated_uber->layers;
  UberStruct::InstanceMap reordered_snapshot;
  reordered_snapshot[1] = reordered_uber;

  const auto reordered_from_cached_stacks = ComputeGlobalResolvedLayers(stacks, reordered_snapshot);
  const auto reordered_from_full_rebuild =
      ComputeGlobalResolvedLayers(topology, reordered_snapshot, matrices, clips, opacities);
  ASSERT_EQ(reordered_from_cached_stacks.size(), 2u);
  EXPECT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(
      reordered_from_cached_stacks[0].content));
  EXPECT_EQ(reordered_from_cached_stacks, reordered_from_full_rebuild);

  // Swap the image layer for a solid-color one in a later snapshot, still without rebuilding
  // `stacks`.
  auto swapped_uber = std::make_shared<UberStruct>();
  swapped_uber->local_topology = uber_struct->local_topology;
  swapped_uber->layer_stacks = updated_uber->layer_stacks;
  swapped_uber->layers = updated_uber->layers;
  swapped_uber->layers[kLayer1] = UberStructLayer{
      .content =
          UberStructLayer::SolidColorModeProperties{
              .color = {0.f, 0.f, 1.f, 1.f},
          },
      .common =
          {
              .display_rect = Rectangle({.x = 15, .y = 25, .width = 60, .height = 90}),
              .opacity = 0.6f,
              .blend_mode = BlendMode::kPremultipliedAlpha(),
          },
  };
  UberStruct::InstanceMap swapped_snapshot;
  swapped_snapshot[1] = swapped_uber;

  const auto swapped_from_cached_stacks = ComputeGlobalResolvedLayers(stacks, swapped_snapshot);
  const auto swapped_from_full_rebuild =
      ComputeGlobalResolvedLayers(topology, swapped_snapshot, matrices, clips, opacities);
  ASSERT_EQ(swapped_from_cached_stacks.size(), 2u);
  EXPECT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(
      swapped_from_cached_stacks[0].content));
  EXPECT_EQ(swapped_from_cached_stacks, swapped_from_full_rebuild);
}

// One `ResolvedLayerStack` entry serves every layer in its stack:
// the hosting node's rotation is decoded once and each layer composes its own
// `transform` onto it. Both layers get the same `dst` because they share
// `display_rect` and the hosting node; only their leaf transforms differ.
TEST(LayerStageTest, MultiLayerStackSharesHostRotation) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  const LayerHandle kLayerA(1, 1);
  const LayerHandle kLayerB(1, 2);
  uber_struct->layer_stacks[kRoot] = {kLayerA, kLayerB};
  uber_struct->layers[kLayerA] = UberStructLayer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(1),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
          },
  };
  uber_struct->layers[kLayerB] = UberStructLayer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kReflectX(),
              .image_id = display::ImageId(2),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
          },
  };
  snapshot[1] = uber_struct;

  // Host node is rotated 90 degrees CCW.
  glm::mat3 host_matrix = glm::rotate(glm::mat3(1.f), -glm::half_pi<float>());
  const auto stacks =
      ComputeGlobalResolvedLayerStacks(topology, snapshot, {host_matrix}, {kUnclippedRegion});
  ASSERT_EQ(stacks.size(), 1u);
  EXPECT_EQ(stacks[0].node_rotation, RotateFlip::kRotateCcw90());

  const auto layers = ComputeGlobalResolvedLayers(stacks, snapshot);
  ASSERT_EQ(layers.size(), 2u);
  EXPECT_EQ(layers[0].geometry.transform, RotateFlip::kRotateCcw90());
  EXPECT_EQ(layers[1].geometry.transform, RotateFlip::kRotateCcw90ReflectY());
  // Under 90 CCW host rotation, `display_rect` (100x200) swaps dimensions to 200x100 in screen
  // space for both layers.
  EXPECT_NEAR(layers[0].geometry.dest.x(), 0.f, 1e-4f);
  EXPECT_NEAR(layers[0].geometry.dest.y(), -100.f, 1e-4f);
  EXPECT_NEAR(layers[0].geometry.dest.width(), 200.f, 1e-4f);
  EXPECT_NEAR(layers[0].geometry.dest.height(), 100.f, 1e-4f);
  EXPECT_NEAR(layers[1].geometry.dest.x(), 0.f, 1e-4f);
  EXPECT_NEAR(layers[1].geometry.dest.y(), -100.f, 1e-4f);
  EXPECT_NEAR(layers[1].geometry.dest.width(), 200.f, 1e-4f);
  EXPECT_NEAR(layers[1].geometry.dest.height(), 100.f, 1e-4f);
}

// The hosting node's rotation decodes the same at every positive scale, uniform or not, and under
// any translation.  The scales catch three ways a decode can go wrong.  Matching the corners of a
// transformed unit square within a fixed tolerance such as 0.001 returns `kIdentity` for every
// rotation at a scale of 1e-4, where all four corners lie within the tolerance.  Comparing the two
// components of the +x column misreads quarter turns when the x and y scales are 1e8 apart: the
// tiny value a float quarter turn leaves on the other axis, times the larger scale, outweighs the
// real one.  Comparing the diagonal and anti-diagonal products of the upper-left 2x2 in float
// misreads quarter turns at a uniform scale of 1e-23, where both products underflow to zero.
TEST(LayerStageTest, HostRotationDecodeIgnoresScale) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};
  uber_struct->layer_stacks[kRoot] = {};
  snapshot[1] = uber_struct;

  // View space has +y pointing down, so a counter-clockwise turn is a negative `glm::rotate()`
  // angle, as in `Flatland::MatrixData`.
  struct RotationCase {
    float angle;
    RotateFlip expected;
  };
  const RotationCase kRotations[] = {
      {.angle = 0.f, .expected = RotateFlip::kIdentity()},
      {.angle = -glm::half_pi<float>(), .expected = RotateFlip::kRotateCcw90()},
      {.angle = -glm::pi<float>(), .expected = RotateFlip::kRotateCcw180()},
      {.angle = -glm::three_over_two_pi<float>(), .expected = RotateFlip::kRotateCcw270()},
  };
  const glm::vec2 kScales[] = {{1e-4f, 1e-4f},  {1.f, 1.f},      {1e4f, 1e4f},    {1e-4f, 1e3f},
                               {1e4f, 1e-4f},   {1e-4f, 1e4f},   {1e6f, 1e-6f},   {1e-6f, 1e6f},
                               {1e10f, 1e-10f}, {1e-10f, 1e10f}, {1e-23f, 1e-23f}};
  const glm::mat3 kTranslation = glm::translate(glm::mat3(1.f), {5000.f, -3000.f});

  for (const auto& rotation : kRotations) {
    SCOPED_TRACE(::testing::Message() << "rotation " << rotation.expected);
    const glm::mat3 r = glm::rotate(glm::mat3(1.f), rotation.angle);
    for (const auto& scale : kScales) {
      SCOPED_TRACE(::testing::Message() << "scale (" << scale.x << ", " << scale.y << ")");
      const glm::mat3 s = glm::scale(glm::mat3(1.f), scale);
      // Scale applied in the node's own space, then in its parent's space.
      for (const glm::mat3& matrix : {kTranslation * r * s, kTranslation * s * r}) {
        const auto stacks =
            ComputeGlobalResolvedLayerStacks(topology, snapshot, {matrix}, {kUnclippedRegion});
        ASSERT_EQ(stacks.size(), 1u);
        EXPECT_EQ(stacks[0].node_rotation, rotation.expected);
      }
    }
  }
}

// A transform-graph change (here a node added to the topology) invalidates the entries.
// Rebuilding them from the new topology and rerunning the layer stage picks up the new
// node's stack at the position its global matrix gives it.
TEST(LayerStageTest, RebuildAfterTransformGraphChange) {
  const TransformHandle kRoot = {1, 0};
  const TransformHandle kNodeA = {1, 1};
  const TransformHandle kNodeB = {1, 2};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 1}, {kNodeA, 0}};
  const LayerHandle kLayerA(1, 1);
  const LayerHandle kLayerB(1, 2);
  uber_struct->layer_stacks[kNodeA] = {kLayerA};
  uber_struct->layer_stacks[kNodeB] = {kLayerB};
  uber_struct->layers[kLayerA] = UberStructLayer{
      .content = UberStructLayer::SolidColorModeProperties{.color = {1.f, 0.f, 0.f, 1.f}},
      .common = {.display_rect = Rectangle({.x = 0, .y = 0, .width = 10, .height = 10})},
  };
  uber_struct->layers[kLayerB] = UberStructLayer{
      .content = UberStructLayer::SolidColorModeProperties{.color = {0.f, 1.f, 0.f, 1.f}},
      .common = {.display_rect = Rectangle({.x = 0, .y = 0, .width = 20, .height = 20})},
  };
  snapshot[1] = uber_struct;

  GlobalTopologyData topo1;
  topo1.topology_vector = {kRoot, kNodeA};
  topo1.parent_indices = {0, 0};
  std::vector<ResolvedLayerStack> stacks;
  stacks = ComputeGlobalResolvedLayerStacks(topo1, snapshot, {glm::mat3(1.f), glm::mat3(1.f)},
                                            {kUnclippedRegion, kUnclippedRegion});
  ASSERT_EQ(stacks.size(), 1u);

  auto layers1 = ComputeGlobalResolvedLayers(stacks, snapshot);
  ASSERT_EQ(layers1.size(), 1u);
  EXPECT_EQ(layers1[0].geometry.dest,
            RectangleF({.x = 0.f, .y = 0.f, .width = 10.f, .height = 10.f}));

  // Now add `kNodeB` to the topology and rebuild `stacks`.
  GlobalTopologyData topo2;
  topo2.topology_vector = {kRoot, kNodeA, kNodeB};
  topo2.parent_indices = {0, 0, 0};
  stacks = ComputeGlobalResolvedLayerStacks(
      topo2, snapshot,
      {glm::mat3(1.f), glm::mat3(1.f), glm::translate(glm::mat3(1.f), {50.f, 60.f})},
      {kUnclippedRegion, kUnclippedRegion, kUnclippedRegion});
  ASSERT_EQ(stacks.size(), 2u);

  auto layers2 = ComputeGlobalResolvedLayers(stacks, snapshot);
  ASSERT_EQ(layers2.size(), 2u);
  EXPECT_EQ(layers2[1].geometry.dest,
            RectangleF({.x = 50.f, .y = 60.f, .width = 20.f, .height = 20.f}));
}

// Exercises difference between how Flatland1 and Flatland2 APIs treat the interaction between
// REPLACE blend mode and opacity.
TEST(GlobalRenderListTest, FlatlandVersionGatesImageReplace) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};
  uber_struct->flatland_version = 1;

  const LayerHandle kLayer(1, 1);
  uber_struct->layer_stacks[kRoot] = {kLayer};

  UberStructLayer uber_layer{
      .content =
          UberStructLayer::ImageModeProperties{
              .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f}),
              .transform = RotateFlip::kIdentity(),
              .image_id = display::ImageId(42),
          },
      .common =
          {
              .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
              .opacity = 0.5f,
              .blend_mode = BlendMode::kReplace(),
          },
  };
  uber_struct->layers[kLayer] = uber_layer;
  snapshot[1] = uber_struct;

  // Case 1: flatland_version == 1, kReplace + opacity 0.5 -> color is scaled, but blend_mode
  // remains kReplace
  {
    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    ASSERT_EQ(result.size(), 1u);
    EXPECT_EQ(result[0].blend_mode, BlendMode::kReplace());
    EXPECT_EQ(result[0].multiply_color, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  }

  // Case 2: flatland_version == 2, kReplace + opacity 0.5 -> blend_mode demoted to
  // kPremultipliedAlpha
  {
    uber_struct->flatland_version = 2;
    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    ASSERT_EQ(result.size(), 1u);
    EXPECT_EQ(result[0].blend_mode, BlendMode::kPremultipliedAlpha());
    EXPECT_EQ(result[0].multiply_color, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  }

  // Case 3: flatland_version == 2, kReplace + opacity 1.0 -> no demotion, color verbatim
  {
    uber_struct->layers[kLayer].common.opacity = 1.0f;
    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    ASSERT_EQ(result.size(), 1u);
    EXPECT_EQ(result[0].blend_mode, BlendMode::kReplace());
    EXPECT_EQ(result[0].multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  }
}

// A layer's own transform must not move it: the destination is `display_rect`,
// whose dimensions already account for any rotation (see `LayerProperties.display_rect` in
// `flatland2.fidl`).  Only the leaf transform (`geometry.transform`) carries the rotation.
TEST(GlobalRenderListTest, LayerTransformKeepsDisplayRect) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  const RectangleF kSampleRect({.x = 0.f, .y = 0.f, .width = 200.f, .height = 100.f});
  const RotateFlip kTransforms[] = {
      RotateFlip::kIdentity(),
      RotateFlip::kReflectX(),
      RotateFlip::kReflectY(),
      RotateFlip::kRotateCcw180(),
      RotateFlip::kRotateCcw90(),
      RotateFlip::kRotateCcw90ReflectX(),
      RotateFlip::kRotateCcw90ReflectY(),
      RotateFlip::kRotateCcw270(),
  };

  for (const RotateFlip& transform : kTransforms) {
    SCOPED_TRACE(::testing::Message() << "layer transform " << transform);

    UberStruct::InstanceMap snapshot;
    auto uber_struct = std::make_shared<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};

    const LayerHandle kLayer(1, 1);
    uber_struct->layer_stacks[kRoot] = {kLayer};

    UberStructLayer uber_layer{
        .content =
            UberStructLayer::ImageModeProperties{
                .sample_rect = kSampleRect,
                .transform = transform,
                .image_id = display::ImageId(42),
            },
        .common =
            {
                .display_rect = Rectangle({.x = 10, .y = 20, .width = 100, .height = 200}),
            },
    };
    uber_struct->layers[kLayer] = uber_layer;
    snapshot[1] = uber_struct;

    auto result =
        ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
    ASSERT_EQ(result.size(), 1u);
    EXPECT_EQ(result[0].geometry.dest,
              RectangleF({.x = 10.f, .y = 20.f, .width = 100.f, .height = 200.f}));
    EXPECT_EQ(result[0].geometry.transform, transform);
    EXPECT_EQ(result[0].geometry.src, kSampleRect);
  }
}

// The hosting node's matrix places `display_rect`: the dst is the bounding box
// of its corners under that matrix, regardless of the layer's own transform.
// The node's rotation then composes onto the layer's transform, layer first,
// as `RotatedBy()` does.  Under the 90 degree CCW parent the expected transforms are
// literals, which pins that order: a reflection does not commute with a quarter turn,
// so `kReflectX` becomes `kRotateCcw90ReflectY`, not `kRotateCcw90ReflectX`.
TEST(GlobalRenderListTest, ParentTransformAndLayerTransformComposition) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  const Rectangle kDisplayRect({.x = 10, .y = 20, .width = 100, .height = 200});
  const RectangleF kSampleRect({.x = 0.f, .y = 0.f, .width = 100.f, .height = 200.f});

  // Each parent is a quarter turn then a translation of (300, 50), built from exact 0 and 1
  // entries so that the expected `dst` is exact.  `glm::mat3` takes its entries column by column.
  struct ParentCase {
    glm::mat3 matrix;
    RotateFlip rotation;
    RectangleF expected_dst;
  };
  const ParentCase kParents[] = {
      // (x, y) -> (300 - y, 50 + x)
      {.matrix = glm::mat3(0.f, 1.f, 0.f, -1.f, 0.f, 0.f, 300.f, 50.f, 1.f),
       .rotation = RotateFlip::kRotateCcw270(),
       .expected_dst = RectangleF({.x = 80.f, .y = 60.f, .width = 200.f, .height = 100.f})},
      // (x, y) -> (300 - x, 50 - y)
      {.matrix = glm::mat3(-1.f, 0.f, 0.f, 0.f, -1.f, 0.f, 300.f, 50.f, 1.f),
       .rotation = RotateFlip::kRotateCcw180(),
       .expected_dst = RectangleF({.x = 190.f, .y = -170.f, .width = 100.f, .height = 200.f})},
      // (x, y) -> (300 + y, 50 - x)
      {.matrix = glm::mat3(0.f, -1.f, 0.f, 1.f, 0.f, 0.f, 300.f, 50.f, 1.f),
       .rotation = RotateFlip::kRotateCcw90(),
       .expected_dst = RectangleF({.x = 320.f, .y = -60.f, .width = 200.f, .height = 100.f})},
  };

  struct LayerCase {
    RotateFlip transform;
    // Expected `geometry.transform` under the `kRotateCcw90` parent.
    RotateFlip expected_under_ccw90;
  };
  const LayerCase kLayers[] = {
      {.transform = RotateFlip::kIdentity(), .expected_under_ccw90 = RotateFlip::kRotateCcw90()},
      {.transform = RotateFlip::kReflectX(),
       .expected_under_ccw90 = RotateFlip::kRotateCcw90ReflectY()},
      {.transform = RotateFlip::kReflectY(),
       .expected_under_ccw90 = RotateFlip::kRotateCcw90ReflectX()},
      {.transform = RotateFlip::kRotateCcw180(),
       .expected_under_ccw90 = RotateFlip::kRotateCcw270()},
      {.transform = RotateFlip::kRotateCcw90(),
       .expected_under_ccw90 = RotateFlip::kRotateCcw180()},
      {.transform = RotateFlip::kRotateCcw90ReflectX(),
       .expected_under_ccw90 = RotateFlip::kReflectX()},
      {.transform = RotateFlip::kRotateCcw90ReflectY(),
       .expected_under_ccw90 = RotateFlip::kReflectY()},
      {.transform = RotateFlip::kRotateCcw270(), .expected_under_ccw90 = RotateFlip::kIdentity()},
  };

  for (const auto& parent : kParents) {
    SCOPED_TRACE(::testing::Message() << "parent rotation " << parent.rotation);
    for (const auto& layer : kLayers) {
      SCOPED_TRACE(::testing::Message() << "layer transform " << layer.transform);

      UberStruct::InstanceMap snapshot;
      auto uber_struct = std::make_shared<UberStruct>();
      uber_struct->local_topology = {{kRoot, 0}};
      const LayerHandle kLayer(1, 1);
      uber_struct->layer_stacks[kRoot] = {kLayer};
      uber_struct->layers[kLayer] = UberStructLayer{
          .content =
              UberStructLayer::ImageModeProperties{
                  .sample_rect = kSampleRect,
                  .transform = layer.transform,
                  .image_id = display::ImageId(42),
              },
          .common =
              {
                  .display_rect = kDisplayRect,
              },
      };
      snapshot[1] = uber_struct;

      auto result =
          ComputeGlobalResolvedLayers(topology, snapshot, {parent.matrix}, {kUnclippedRegion});
      ASSERT_EQ(result.size(), 1u);
      EXPECT_EQ(result[0].geometry.dest, parent.expected_dst);
      EXPECT_EQ(result[0].geometry.src, kSampleRect);
      const RotateFlip expected_transform = parent.rotation == RotateFlip::kRotateCcw90()
                                                ? layer.expected_under_ccw90
                                                : layer.transform.RotatedBy(parent.rotation);
      EXPECT_EQ(result[0].geometry.transform, expected_transform);
    }
  }
}

// Verifies that when a layer with any of the 8 `RotateFlip` transforms is clipped, both `dst` and
// `src` UVs shrink proportionally along the corresponding source axes.
TEST(GlobalRenderListTest, RotatedLayerWithClipShrinksDstAndUVsProportionally) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  // Unclipped `dst` is [0, 0, 100, 200]. Clip removes:
  //   left:   10 px (10% of dst width  100 -> f_left   = 0.1)
  //   right:  30 px (30% of dst width  100 -> f_right  = 0.3)
  //   top:    20 px (10% of dst height 200 -> f_top    = 0.1)
  //   bottom: 80 px (40% of dst height 200 -> f_bottom = 0.4)
  // Resulting clipped `dst` is [10, 20, 60, 100].
  const types::Rectangle kClipRect({.x = 10, .y = 20, .width = 60, .height = 100});
  const RectangleF kExpectedDst({.x = 10.f, .y = 20.f, .width = 60.f, .height = 100.f});

  // Unclipped `src` is [50, 70, 1000, 2000].
  const RectangleF kSampleRect({.x = 50.f, .y = 70.f, .width = 1000.f, .height = 2000.f});

  struct TestCase {
    RotateFlip transform;
    RectangleF expected_src;
  };
  const TestCase kCases[] = {
      // (f_left, f_right, f_top, f_bottom) = (0.1, 0.3, 0.1, 0.4)
      {.transform = RotateFlip::kIdentity(),
       .expected_src = RectangleF({.x = 150.f, .y = 270.f, .width = 600.f, .height = 1000.f})},
      // ReflectX (vertical flip across X axis): (0.1, 0.3, 0.4, 0.1)
      {.transform = RotateFlip::kReflectX(),
       .expected_src = RectangleF({.x = 150.f, .y = 870.f, .width = 600.f, .height = 1000.f})},
      // ReflectY (horizontal flip across Y axis): (0.3, 0.1, 0.1, 0.4)
      {.transform = RotateFlip::kReflectY(),
       .expected_src = RectangleF({.x = 350.f, .y = 270.f, .width = 600.f, .height = 1000.f})},
      // RotateCcw180: (0.3, 0.1, 0.4, 0.1)
      {.transform = RotateFlip::kRotateCcw180(),
       .expected_src = RectangleF({.x = 350.f, .y = 870.f, .width = 600.f, .height = 1000.f})},
      // RotateCcw90: (1 - f_bottom=0.4, 1 - f_top=0.1, f_left=0.1, f_right=0.3)
      {.transform = RotateFlip::kRotateCcw90(),
       .expected_src = RectangleF({.x = 450.f, .y = 270.f, .width = 500.f, .height = 1200.f})},
      // RotateCcw270: (f_top=0.1, f_bottom=0.4, 1 - f_right=0.3, 1 - f_left=0.1)
      {.transform = RotateFlip::kRotateCcw270(),
       .expected_src = RectangleF({.x = 150.f, .y = 670.f, .width = 500.f, .height = 1200.f})},
      // RotateCcw90ReflectX: (f_top=0.1, f_bottom=0.4, f_left=0.1, f_right=0.3)
      {.transform = RotateFlip::kRotateCcw90ReflectX(),
       .expected_src = RectangleF({.x = 150.f, .y = 270.f, .width = 500.f, .height = 1200.f})},
      // RotateCcw90ReflectY: (1 - f_bottom=0.4, 1 - f_top=0.1, 1 - f_right=0.3, 1 - f_left=0.1)
      {.transform = RotateFlip::kRotateCcw90ReflectY(),
       .expected_src = RectangleF({.x = 450.f, .y = 670.f, .width = 500.f, .height = 1200.f})},
  };

  for (const auto& tc : kCases) {
    SCOPED_TRACE(::testing::Message() << "layer transform " << tc.transform);
    UberStruct::InstanceMap snapshot;
    auto uber_struct = std::make_shared<UberStruct>();
    uber_struct->local_topology = {{kRoot, 0}};
    const LayerHandle kLayer(1, 1);
    uber_struct->layer_stacks[kRoot] = {kLayer};
    uber_struct->layers[kLayer] = UberStructLayer{
        .content =
            UberStructLayer::ImageModeProperties{
                .sample_rect = kSampleRect,
                .transform = tc.transform,
                .image_id = display::ImageId(42),
            },
        .common =
            {
                .display_rect = Rectangle({.x = 0, .y = 0, .width = 100, .height = 200}),
            },
    };
    snapshot[1] = uber_struct;

    auto result = ComputeGlobalResolvedLayers(topology, snapshot, {glm::mat3(1.f)}, {kClipRect});
    ASSERT_EQ(result.size(), 1u);
    EXPECT_NEAR(result[0].geometry.dest.x(), kExpectedDst.x(), 1e-4f);
    EXPECT_NEAR(result[0].geometry.dest.y(), kExpectedDst.y(), 1e-4f);
    EXPECT_NEAR(result[0].geometry.dest.width(), kExpectedDst.width(), 1e-4f);
    EXPECT_NEAR(result[0].geometry.dest.height(), kExpectedDst.height(), 1e-4f);
    EXPECT_NEAR(result[0].geometry.src.x(), tc.expected_src.x(), 1e-3f);
    EXPECT_NEAR(result[0].geometry.src.y(), tc.expected_src.y(), 1e-3f);
    EXPECT_NEAR(result[0].geometry.src.width(), tc.expected_src.width(), 1e-3f);
    EXPECT_NEAR(result[0].geometry.src.height(), tc.expected_src.height(), 1e-3f);
    EXPECT_EQ(result[0].geometry.transform, tc.transform);
  }
}

// Forwards to `std::pmr::new_delete_resource()` and counts allocations.
class CountingMemoryResource : public std::pmr::memory_resource {
 public:
  size_t allocation_count() const { return allocation_count_; }

 private:
  void* do_allocate(size_t bytes, size_t alignment) override {
    ++allocation_count_;
    return std::pmr::new_delete_resource()->allocate(bytes, alignment);
  }

  void do_deallocate(void* p, size_t bytes, size_t alignment) override {
    std::pmr::new_delete_resource()->deallocate(p, bytes, alignment);
  }

  bool do_is_equal(const std::pmr::memory_resource& other) const noexcept override {
    return this == &other;
  }

  size_t allocation_count_ = 0;
};

// `ComputeGlobalResolvedLayers()` emitting into a pre-reserved `std::pmr::vector<ResolvedLayer>`
// over a `utils::PmrStackArena` whose buffer holds `kNumLayers` layers must
// preserve the reserved capacity across `output.clear()` and never reach the upstream resource.
// The result must also match `std::vector<ResolvedLayer>` both before and after
// `CullLayersInPlace()` erases occluded layers.
TEST(LayerStageTest, PmrMonotonicArenaZeroHeapAllocation) {
  const TransformHandle kRoot = {1, 0};
  GlobalTopologyData topology;
  topology.topology_vector = {kRoot};
  topology.parent_indices = {0};

  UberStruct::InstanceMap snapshot;
  auto uber_struct = std::make_shared<UberStruct>();
  uber_struct->local_topology = {{kRoot, 0}};

  constexpr int32_t kDisplayWidth = 1920;
  constexpr int32_t kDisplayHeight = 1080;
  constexpr size_t kNumLayers = 16;
  constexpr size_t kOccluderIndex = 8;
  for (size_t i = 0; i < kNumLayers; ++i) {
    LayerHandle h(1, i + 1);
    uber_struct->layer_stacks[kRoot].push_back(h);
    const Rectangle display_rect =
        i == kOccluderIndex
            ? Rectangle({.x = 0, .y = 0, .width = kDisplayWidth, .height = kDisplayHeight})
            : Rectangle({.x = static_cast<int32_t>(i * 10), .y = 0, .width = 50, .height = 50});
    uber_struct->layers[h] = UberStructLayer{
        .content =
            UberStructLayer::ImageModeProperties{
                .sample_rect = RectangleF({.x = 0.f, .y = 0.f, .width = 100.f, .height = 100.f}),
                .transform = RotateFlip::kIdentity(),
                .image_id = display::ImageId(i + 1),
            },
        .common =
            {
                .display_rect = display_rect,
            },
    };
  }
  snapshot[1] = uber_struct;

  const auto stacks =
      ComputeGlobalResolvedLayerStacks(topology, snapshot, {glm::mat3(1.f)}, {kUnclippedRegion});
  ASSERT_EQ(stacks.size(), 1u);

  // Uses a counting upstream rather than `null_memory_resource()` because this binary is built
  // without exceptions, so a `bad_alloc` would abort the whole test binary instead of failing this
  // test.
  CountingMemoryResource upstream;
  utils::PmrStackArena<kNumLayers * sizeof(ResolvedLayer)> arena(&upstream);
  std::pmr::vector<ResolvedLayer> pmr_layers(arena.resource());
  pmr_layers.reserve(kNumLayers);
  ComputeGlobalResolvedLayers(pmr_layers, stacks, snapshot);
  EXPECT_EQ(upstream.allocation_count(), 0u);

  auto std_layers = ComputeGlobalResolvedLayers(stacks, snapshot);
  EXPECT_EQ(pmr_layers.size(), kNumLayers);
  EXPECT_THAT(pmr_layers, ::testing::ElementsAreArray(std_layers));

  CullLayersInPlace(&pmr_layers, kDisplayWidth, kDisplayHeight);
  CullLayersInPlace(&std_layers, kDisplayWidth, kDisplayHeight);
  EXPECT_EQ(upstream.allocation_count(), 0u);
  EXPECT_EQ(pmr_layers.size(), kNumLayers - kOccluderIndex);
  EXPECT_THAT(pmr_layers, ::testing::ElementsAreArray(std_layers));
}

}  // namespace
}  // namespace flatland::test
