// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include <random>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/ui/scenic/lib/flatland/global_image_data.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"
#include "src/ui/scenic/lib/flatland/tests/flatland_unittest.h"
#include "src/ui/scenic/lib/types/rectangle_f.h"

namespace flatland {

namespace test {

// A test fixture which supports direct comparison of the "legacy" and "Flatland2 schema" execution
// paths, to guarantee that both paths produce an equivalent vector of `ResolvedLayer` (and will
// therefore produce identical on-screen results).
class FlatlandDifferentialTest : public FlatlandTest {
 protected:
  // A scene script that issues Flatland API commands and triggers rendering updates
  // by calling `present_and_capture`.
  using ScriptFunc = std::function<void(Flatland* flatland, allocation::Allocator* allocator,
                                        std::function<void()> present_and_capture)>;

  // Runs the given `script` under the specified UberStruct schema (legacy if `use_flatland2_schema`
  // is false, or Flatland2 if true). Returns a list of resolved layer vectors, one vector for each
  // `present_and_capture` call made by the script.
  std::vector<std::vector<ResolvedLayer>> RunScript(bool use_flatland2_schema,
                                                    const ScriptFunc& script) {
    // Ensure a completely clean state.
    flatlands_.clear();
    pending_release_fences_.clear();
    pending_release_counters_.clear();
    requested_presentation_times_.clear();
    pending_instance_updates_.clear();

    FlatlandConfig config;
    config.use_flatland2_uberstruct_schema = use_flatland2_schema;

    auto flatland = CreateFlatland(config);
    auto allocator = CreateAllocator();

    std::vector<std::vector<ResolvedLayer>> capture_list;

    auto present_and_capture = [&]() {
      EXPECT_CALL(*mock_flatland_presenter_,
                  ScheduleUpdateForSession(::testing::_, ::testing::_, ::testing::_, ::testing::_,
                                           ::testing::_, ::testing::_, ::testing::_))
          .Times(1)
          .RetiresOnSaturation();

      flatland->Present(fuchsia_ui_composition::PresentArgs());
      RunLoopUntilIdle();
      ApplySessionUpdatesAndSignalFences();
      flatland->OnNextFrameBegin(1, {});

      auto snapshot = uber_struct_system_->Snapshot();
      auto links = link_system_->GetResolvedTopologyLinks();
      auto link_system_id = link_system_->GetInstanceId();
      auto root_transform = flatland->GetRoot();

      // Note: the following calls (generating topology_data, global_matrices, and clip_regions)
      // mirror the steps that are duplicated between `Engine::SceneState::InitializeFlatland1()`
      // and `Engine::SceneState::InitializeFlatland2()`.  Keep them in sync until the legacy path
      // is deleted.
      GlobalTopologyData topology_data;
      GlobalTopologyData::ComputeGlobalTopologyData(/*output=*/topology_data, snapshot.map, links,
                                                    link_system_id, root_transform);

      GlobalMatrixVector global_matrices;
      ComputeGlobalMatrices(/*output=*/global_matrices, topology_data.topology_vector,
                            topology_data.parent_indices, snapshot.map);

      GlobalTransformClipRegionVector clip_regions;
      ComputeGlobalTransformClipRegions(/*output=*/clip_regions, topology_data.topology_vector,
                                        topology_data.parent_indices, global_matrices,
                                        snapshot.map);

      // Note: these if/else branches correspond to the unshared parts of `InitializeFlatland2()`
      // and `InitializeFlatland1()`, respectively.
      std::vector<ResolvedLayer> resolved_layers;
      if (use_flatland2_schema) {
        resolved_layers =
            ComputeGlobalResolvedLayers(topology_data, snapshot.map, global_matrices, clip_regions);
      } else {
        GlobalIndexVector image_indices;
        GlobalImageVector images;
        ComputeGlobalImageData(/*output_indices=*/image_indices, /*output_images=*/images,
                               topology_data.topology_vector, topology_data.parent_indices,
                               snapshot.map);

        GlobalImageSampleRegionVector image_sample_regions;
        ComputeGlobalImageSampleRegions(/*output=*/image_sample_regions,
                                        topology_data.topology_vector, topology_data.parent_indices,
                                        snapshot.map);

        std::vector<ImageRect> image_rectangles;
        ComputeGlobalRectangles(/*output=*/image_rectangles, global_matrices, image_sample_regions,
                                clip_regions, image_indices, images);

        ComputeGlobalResolvedLayers(resolved_layers, image_rectangles, images, image_indices);
      }

      CullLayersInPlace(&resolved_layers, 1000, 1000);
      capture_list.push_back(std::move(resolved_layers));
    };

    script(flatland.get(), allocator.get(), present_and_capture);

    flatlands_.clear();
    return capture_list;
  }

  void ExpectResolvedLayersEqual(const std::vector<ResolvedLayer>& l1,
                                 const std::vector<ResolvedLayer>& l2) {
    ASSERT_EQ(l1.size(), l2.size())
        << "ResolvedLayers size mismatch: " << l1.size() << " vs " << l2.size();

    // GlobalImageIds are allocated via `allocation::GenerateUniqueImageId()`, which uses a
    // process-wide monotonic counter.  Because the counter is not reset between the legacy (l1)
    // and facade (l2) script runs, corresponding images will have different IDs in each run.  We
    // cannot expect exact ID equality.  Instead, we verify relabeling-invariance: the mapping
    // between legacy and facade image IDs must be a strict 1-to-1 bijection.
    std::unordered_map<allocation::GlobalImageId, allocation::GlobalImageId> image_id_map;
    std::unordered_map<allocation::GlobalImageId, allocation::GlobalImageId> reverse_image_id_map;

    for (size_t i = 0; i < l1.size(); ++i) {
      const auto& layer1 = l1[i];
      const auto& layer2 = l2[i];

      SCOPED_TRACE(testing::Message()
                   << "Comparing ResolvedLayer at index " << i << "\nLegacy layer: " << layer1
                   << "\nFacade layer: " << layer2);

      // Rect comparison uses ImageRect::operator== with 0.001f epsilon.
      EXPECT_EQ(layer1.rect, layer2.rect);

      EXPECT_EQ(layer1.blend_mode, layer2.blend_mode);
      EXPECT_EQ(layer1.flip, layer2.flip);
      EXPECT_EQ(layer1.topology_index, layer2.topology_index);

      ASSERT_EQ(layer1.content.index(), layer2.content.index());
      if (std::holds_alternative<ResolvedLayer::ImageContent>(layer1.content)) {
        EXPECT_FLOAT_EQ(layer1.multiply_color[0], layer2.multiply_color[0]);
        EXPECT_FLOAT_EQ(layer1.multiply_color[1], layer2.multiply_color[1]);
        EXPECT_FLOAT_EQ(layer1.multiply_color[2], layer2.multiply_color[2]);
        EXPECT_FLOAT_EQ(layer1.multiply_color[3], layer2.multiply_color[3]);

        auto img1 = std::get<ResolvedLayer::ImageContent>(layer1.content);
        auto img2 = std::get<ResolvedLayer::ImageContent>(layer2.content);

        // Enforce relabeling-invariance: the legacy <-> facade id correspondence must be a
        // consistent bijection.
        auto [it1, inserted1] = image_id_map.try_emplace(img1.image_id, img2.image_id);
        EXPECT_EQ(it1->second, img2.image_id)
            << "Image ID mapping mismatch (legacy -> facade): " << img1.image_id
            << " was previously mapped to " << it1->second << " but now seen with "
            << img2.image_id;

        auto [it2, inserted2] = reverse_image_id_map.try_emplace(img2.image_id, img1.image_id);
        EXPECT_EQ(it2->second, img1.image_id)
            << "Image ID mapping mismatch (facade -> legacy): " << img2.image_id
            << " was previously mapped to " << it2->second << " but now seen with "
            << img1.image_id;

        EXPECT_EQ(img1.width, img2.width);
        EXPECT_EQ(img1.height, img2.height);
      } else {
        // The legacy implementation of opacity folds it into the content color, whereas the
        // facade implementation stores it in `multiply_color`.  Thus, we can't check for
        // field-by-field equality.  However, the `DisplayCompositor` multiplies these together
        // for both the GPU composition and direct-to-display paths, so if the multiplied factors,
        // the result would be identical on screen.
        auto col1 = std::get<ResolvedLayer::SolidColorContent>(layer1.content);
        auto col2 = std::get<ResolvedLayer::SolidColorContent>(layer2.content);
        EXPECT_FLOAT_EQ(layer1.multiply_color[0] * col1.color[0],
                        layer2.multiply_color[0] * col2.color[0]);
        EXPECT_FLOAT_EQ(layer1.multiply_color[1] * col1.color[1],
                        layer2.multiply_color[1] * col2.color[1]);
        EXPECT_FLOAT_EQ(layer1.multiply_color[2] * col1.color[2],
                        layer2.multiply_color[2] * col2.color[2]);
        EXPECT_FLOAT_EQ(layer1.multiply_color[3] * col1.color[3],
                        layer2.multiply_color[3] * col2.color[3]);
      }
    }
  }

  void ExpectMultiPresentResolvedLayersEqual(const std::vector<std::vector<ResolvedLayer>>& l1,
                                             const std::vector<std::vector<ResolvedLayer>>& l2) {
    ASSERT_EQ(l1.size(), l2.size())
        << "Number of presents mismatch: " << l1.size() << " vs " << l2.size();
    for (size_t step = 0; step < l1.size(); ++step) {
      SCOPED_TRACE(testing::Message() << "Comparing present step " << step);
      ExpectResolvedLayersEqual(l1[step], l2[step]);
    }
  }
};

// 1. SingleImageDefault - create, attach, present.
TEST_F(FlatlandDifferentialTest, SingleImageDefault) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 2. SampleRegion - sub-rect sample region.
TEST_F(FlatlandDifferentialTest, SampleRegion) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    flatland->SetImageSampleRegion(
        kImageId, types::RectangleF{{.x = 10.f, .y = 20.f, .width = 30.f, .height = 40.f}});
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 3. SampleRegionClampIfNear - region inside the epsilon of the image edge.
TEST_F(FlatlandDifferentialTest, SampleRegionClampIfNear) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    // Epsilon is 1e-3f, so 99.999f is within epsilon of the right boundary (100.f).
    flatland->SetImageSampleRegion(
        kImageId, types::RectangleF{{.x = 0.f, .y = 0.f, .width = 99.999f, .height = 200.f}});
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 4. DestinationSize - non-native dest size (scale).
TEST_F(FlatlandDifferentialTest, DestinationSize) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    flatland->SetImageDestinationSize(kImageId, fuchsia_math::SizeU{400, 500});
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 5. OpacityOnImage - 0.5.
TEST_F(FlatlandDifferentialTest, OpacityOnImage) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    flatland->SetImageOpacity(kImageId, 0.5f);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 6. OpacityInheritedChain - parent transform opacity 0.5 × image opacity 0.5.
TEST_F(FlatlandDifferentialTest, OpacityInheritedChain) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kChildId{3};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kChildId);
    flatland->AddChild(kRootId, kChildId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kChildId, kImageId);
    flatland->SetOpacity(kRootId, 0.5f);
    flatland->SetImageOpacity(kImageId, 0.5f);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 7. BlendModeReplaceVsPremultiplied - two images, one each.
TEST_F(FlatlandDifferentialTest, BlendModeReplaceVsPremultiplied) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kTrans1{2};
    const TransformId kTrans2{3};
    const ContentId kImage1{4};
    const ContentId kImage2{5};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kTrans1);
    flatland->CreateTransform(kTrans2);
    flatland->AddChild(kRootId, kTrans1);
    flatland->AddChild(kRootId, kTrans2);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});

    auto ref_pair1 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImage1, std::move(ref_pair1), properties);

    auto ref_pair2 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImage2, std::move(ref_pair2), properties);

    flatland->SetContent(kTrans1, kImage1);
    flatland->SetContent(kTrans2, kImage2);

    flatland->SetImageBlendMode(kImage1, BlendMode::kReplace());
    flatland->SetImageBlendMode(kImage2, BlendMode::kPremultipliedAlpha());
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 2 resolved layers.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 2U);
}

// 8. FlipLeftRight, FlipUpDown - one present, two images.
TEST_F(FlatlandDifferentialTest, FlipLeftRightAndUpDown) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kTrans1{2};
    const TransformId kTrans2{3};
    const ContentId kImage1{4};
    const ContentId kImage2{5};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kTrans1);
    flatland->CreateTransform(kTrans2);
    flatland->AddChild(kRootId, kTrans1);
    flatland->AddChild(kRootId, kTrans2);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});

    auto ref_pair1 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImage1, std::move(ref_pair1), properties);

    auto ref_pair2 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImage2, std::move(ref_pair2), properties);

    flatland->SetContent(kTrans1, kImage1);
    flatland->SetContent(kTrans2, kImage2);

    flatland->SetImageFlip(kImage1, fuchsia_ui_composition::ImageFlip::kLeftRight);
    flatland->SetImageFlip(kImage2, fuchsia_ui_composition::ImageFlip::kUpDown);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 2 resolved layers.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 2U);
}

// 9. OrientationEachQuarterTurn - parent SetOrientation 90/180/270, three presents (compare after
// each).
TEST_F(FlatlandDifferentialTest, OrientationEachQuarterTurn) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);

    flatland->SetOrientation(kRootId, fuchsia_ui_composition::Orientation::kCcw90Degrees);
    present_and_capture();

    flatland->SetOrientation(kRootId, fuchsia_ui_composition::Orientation::kCcw180Degrees);
    present_and_capture();

    flatland->SetOrientation(kRootId, fuchsia_ui_composition::Orientation::kCcw270Degrees);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 3 frames, each with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 3U);
  for (size_t i = 0; i < 3; ++i) {
    EXPECT_EQ(layers1[i].size(), 1U);
  }
}

// 10. FlipUnderRotation - flip + parent 90° (the composition case).
TEST_F(FlatlandDifferentialTest, FlipUnderRotation) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kChildId{3};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kChildId);
    flatland->AddChild(kRootId, kChildId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kChildId, kImageId);

    flatland->SetOrientation(kRootId, fuchsia_ui_composition::Orientation::kCcw90Degrees);
    flatland->SetImageFlip(kImageId, fuchsia_ui_composition::ImageFlip::kLeftRight);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 11. TranslateScaleNest - two nested transforms with translation + scale.
TEST_F(FlatlandDifferentialTest, TranslateScaleNest) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kChildId{3};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kChildId);
    flatland->AddChild(kRootId, kChildId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kChildId, kImageId);

    flatland->SetTranslation(kRootId, fuchsia_math::Vec{10, 20});
    flatland->SetScale(kRootId, fuchsia_math::VecF{2.f, 3.f});

    flatland->SetTranslation(kChildId, fuchsia_math::Vec{5, 15});
    flatland->SetScale(kChildId, fuchsia_math::VecF{1.5f, 2.5f});
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 12. ClipPartial - SetClipBoundary cropping an image partially.
TEST_F(FlatlandDifferentialTest, ClipPartial) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    flatland->SetClipBoundary(
        kRootId, std::make_unique<fuchsia_math::Rect>(fuchsia_math::Rect{10, 20, 50, 60}));
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 12a. ClipInheritedChain - nested clipping: parent clip boundary and child clip boundary
// intersecting.
TEST_F(FlatlandDifferentialTest, ClipInheritedChain) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kChildId{2};
    const TransformId kGrandchildId{3};
    const ContentId kImageId{4};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kChildId);
    flatland->AddChild(kRootId, kChildId);
    flatland->SetTranslation(kChildId, fuchsia_math::Vec{-10, -10});

    flatland->CreateTransform(kGrandchildId);
    flatland->AddChild(kChildId, kGrandchildId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{200, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kGrandchildId, kImageId);

    // Parent clips to [20, 20, 50, 50]
    flatland->SetClipBoundary(
        kRootId, std::make_unique<fuchsia_math::Rect>(fuchsia_math::Rect{20, 20, 50, 50}));

    // Child clips to [10, 10, 100, 100] (in child space, which translates to [0, 0, 100, 100]
    // globally).  The intersection of parent and child clip regions should be [20, 20, 50, 50]
    // globally.
    flatland->SetClipBoundary(
        kChildId, std::make_unique<fuchsia_math::Rect>(fuchsia_math::Rect{10, 10, 100, 100}));

    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);

  // Verify that the resolved layer destination region matches the global intersection [20, 20, 50,
  // 50].  We only need to look at one layer, since we already established that both paths are
  // equivalent.
  auto& layer = layers1[0][0];

  // Verify destination region.
  EXPECT_EQ(layer.rect.origin.x, 20.f);
  EXPECT_EQ(layer.rect.origin.y, 20.f);
  EXPECT_EQ(layer.rect.extent.x, 50.f);
  EXPECT_EQ(layer.rect.extent.y, 50.f);

  // Verify source region (UVs) corresponding to [30, 30, 80, 80] in texels.
  EXPECT_EQ(layer.rect.texel_uvs[0], glm::ivec2(30, 30));
  EXPECT_EQ(layer.rect.texel_uvs[1], glm::ivec2(80, 30));
  EXPECT_EQ(layer.rect.texel_uvs[2], glm::ivec2(80, 80));
  EXPECT_EQ(layer.rect.texel_uvs[3], glm::ivec2(30, 80));
}

// 13. ClipToEmpty - clip fully outside; both paths must emit nothing.
TEST_F(FlatlandDifferentialTest, ClipToEmpty) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 200});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    // Clip completely outside the image bounds (0, 0, 100, 200).
    flatland->SetClipBoundary(
        kRootId, std::make_unique<fuchsia_math::Rect>(fuchsia_math::Rect{500, 500, 10, 10}));
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 0 resolved layers due to full clipping.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 0U);
}

// 14. FilledRectBasic - color + size.
TEST_F(FlatlandDifferentialTest, FilledRectBasic) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kRectId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 1.0f},
                           fuchsia_math::SizeU{120, 240});

    flatland->SetContent(kRootId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 1U);
}

// 15. FilledRectBetweenImages - z-order: image, rect, image.
TEST_F(FlatlandDifferentialTest, FilledRectBetweenImages) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kTrans1{2};
    const TransformId kTrans2{3};
    const TransformId kTrans3{4};
    const ContentId kImage1{5};
    const ContentId kRectId{6};
    const ContentId kImage2{7};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kTrans1);
    flatland->CreateTransform(kTrans2);
    flatland->CreateTransform(kTrans3);
    flatland->AddChild(kRootId, kTrans1);
    flatland->AddChild(kRootId, kTrans2);
    flatland->AddChild(kRootId, kTrans3);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 100});

    auto ref_pair1 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImage1, std::move(ref_pair1), properties);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{1.0f, 0.0f, 0.0f, 1.0f},
                           fuchsia_math::SizeU{80, 80});

    auto ref_pair2 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImage2, std::move(ref_pair2), properties);

    flatland->SetContent(kTrans1, kImage1);
    flatland->SetContent(kTrans2, kRectId);
    flatland->SetContent(kTrans3, kImage2);

    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 3 resolved layers.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 3U);
}

// 16. MultiAttachSameContent - one ContentId under two transforms (DAG instancing).
TEST_F(FlatlandDifferentialTest, MultiAttachSameContent) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kLeftBranchParent{2};
    const TransformId kRightBranchParent{3};
    const TransformId kLeftBranchChild{4};
    const TransformId kRightBranchChild{5};
    const ContentId kImageId{6};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kLeftBranchParent);
    flatland->CreateTransform(kRightBranchParent);
    flatland->CreateTransform(kLeftBranchChild);
    flatland->CreateTransform(kRightBranchChild);

    flatland->AddChild(kRootId, kLeftBranchParent);
    flatland->AddChild(kRootId, kRightBranchParent);
    flatland->AddChild(kLeftBranchParent, kLeftBranchChild);
    flatland->AddChild(kRightBranchParent, kRightBranchChild);

    // Apply different translations, scales, and orientations along the two paths.
    flatland->SetTranslation(kLeftBranchParent, fuchsia_math::Vec{-50, -100});
    flatland->SetScale(kLeftBranchParent, fuchsia_math::VecF{2.f, 3.f});
    flatland->SetOrientation(kLeftBranchParent, fuchsia_ui_composition::Orientation::kCcw90Degrees);

    flatland->SetTranslation(kLeftBranchChild, fuchsia_math::Vec{10, 20});
    flatland->SetScale(kLeftBranchChild, fuchsia_math::VecF{1.5f, 0.5f});

    flatland->SetTranslation(kRightBranchParent, fuchsia_math::Vec{50, 100});
    flatland->SetScale(kRightBranchParent, fuchsia_math::VecF{0.5f, 1.5f});
    flatland->SetOrientation(kRightBranchParent,
                             fuchsia_ui_composition::Orientation::kCcw180Degrees);

    flatland->SetTranslation(kRightBranchChild, fuchsia_math::Vec{-5, -15});
    flatland->SetScale(kRightBranchChild, fuchsia_math::VecF{2.f, 2.f});

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 100});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kLeftBranchChild, kImageId);
    flatland->SetContent(kRightBranchChild, kImageId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame with 2 resolved layers, due to DAG multi-parenting.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_EQ(layers1[0].size(), 2U);
}

// 17. BufferSwapAcrossPresents - SetContent(t, A), present, SetContent(t, B), present, back to A.
TEST_F(FlatlandDifferentialTest, BufferSwapAcrossPresents) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageA{2};
    const ContentId kImageB{3};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 100});

    auto ref_pairA = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageA, std::move(ref_pairA), properties);

    auto ref_pairB = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageB, std::move(ref_pairB), properties);

    flatland->SetContent(kRootId, kImageA);
    present_and_capture();

    flatland->SetContent(kRootId, kImageB);
    present_and_capture();

    flatland->SetContent(kRootId, kImageA);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 3 frames, each with 1 resolved layer.
  ASSERT_EQ(layers1.size(), 3U);
  for (size_t i = 0; i < 3; ++i) {
    EXPECT_EQ(layers1[i].size(), 1U);
  }
}

// 18. ReleaseWhileAttached - release image, present (still displayed), detach, present (gone).
TEST_F(FlatlandDifferentialTest, ReleaseWhileAttached) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kImageId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 100});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kRootId, kImageId);
    present_and_capture();

    // Release image - should still display because it is attached.
    flatland->ReleaseImage(kImageId);
    present_and_capture();

    // Detach image by setting content to invalid - should disappear.
    flatland->SetContent(kRootId, kInvalidContentId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 3 frames, with the last frame empty due to detachment.
  ASSERT_EQ(layers1.size(), 3U);
  EXPECT_EQ(layers1[0].size(), 1U);
  EXPECT_EQ(layers1[1].size(), 1U);
  EXPECT_EQ(layers1[2].size(), 0U);
}

// 19. HiddenBranch - content under a transform removed from the topology.
TEST_F(FlatlandDifferentialTest, HiddenBranch) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kChildId{2};
    const ContentId kImageId{3};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kChildId);
    flatland->AddChild(kRootId, kChildId);

    fuchsia_ui_composition::ImageProperties properties;
    properties.size(fuchsia_math::SizeU{100, 100});
    auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageId, std::move(ref_pair), std::move(properties));

    flatland->SetContent(kChildId, kImageId);
    present_and_capture();

    // Remove child from root - branch is now hidden/detached.
    flatland->RemoveChild(kRootId, kChildId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 2 frames, with the last frame empty due to subtree detachment.
  ASSERT_EQ(layers1.size(), 2U);
  EXPECT_EQ(layers1[0].size(), 1U);
  EXPECT_EQ(layers1[1].size(), 0U);
}

// 20. TwentyLayerStress - generated: N images in a row with varied properties (seeded loop,
// deterministic).
TEST_F(FlatlandDifferentialTest, TwentyLayerStress) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    // Seeded random number generator for reproducibility.
    std::mt19937 rng(42);
    std::uniform_int_distribution<int> bool_dist(0, 1);
    std::uniform_int_distribution<int> flip_dist(0, 2);
    std::uniform_int_distribution<int> orient_dist(0, 3);
    std::uniform_real_distribution<float> float_dist(0.1f, 1.0f);

    std::vector<TransformId> transforms;
    for (int i = 0; i < 20; ++i) {
      TransformId trans_id{static_cast<uint64_t>(10 + i)};
      flatland->CreateTransform(trans_id);

      if (i == 0) {
        flatland->AddChild(kRootId, trans_id);
      } else {
        flatland->AddChild(transforms[static_cast<size_t>(i - 1)], trans_id);
      }
      transforms.push_back(trans_id);

      // Randomize transform properties
      flatland->SetTranslation(trans_id, fuchsia_math::Vec{static_cast<int32_t>(rng() % 20 - 10),
                                                           static_cast<int32_t>(rng() % 20 - 10)});
      flatland->SetScale(trans_id, fuchsia_math::VecF{float_dist(rng), float_dist(rng)});

      fuchsia_ui_composition::Orientation orients[] = {
          fuchsia_ui_composition::Orientation::kCcw0Degrees,
          fuchsia_ui_composition::Orientation::kCcw90Degrees,
          fuchsia_ui_composition::Orientation::kCcw180Degrees,
          fuchsia_ui_composition::Orientation::kCcw270Degrees};
      flatland->SetOrientation(trans_id, orients[orient_dist(rng)]);
      flatland->SetOpacity(trans_id, float_dist(rng));

      // 50% chance to put content
      if (bool_dist(rng)) {
        ContentId img_id{static_cast<uint64_t>(100 + i)};
        fuchsia_ui_composition::ImageProperties properties;
        properties.size(fuchsia_math::SizeU{50, 50});
        auto ref_pair = allocation::cpp::BufferCollectionImportExportTokens::New();
        CreateImage(flatland, allocator, img_id, std::move(ref_pair), std::move(properties));

        flatland->SetContent(trans_id, img_id);

        fuchsia_ui_composition::ImageFlip flips[] = {fuchsia_ui_composition::ImageFlip::kNone,
                                                     fuchsia_ui_composition::ImageFlip::kLeftRight,
                                                     fuchsia_ui_composition::ImageFlip::kUpDown};
        flatland->SetImageFlip(img_id, flips[flip_dist(rng)]);
        flatland->SetImageOpacity(img_id, float_dist(rng));

        if (bool_dist(rng)) {
          flatland->SetImageBlendMode(img_id, BlendMode::kReplace());
        } else {
          flatland->SetImageBlendMode(img_id, BlendMode::kPremultipliedAlpha());
        }
      }
    }
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 1 frame and generated layers.
  ASSERT_EQ(layers1.size(), 1U);
  EXPECT_GE(layers1[0].size(), 1U);
}

// 21. CullingOpaqueOccluder - full-screen opaque image culls layers behind it.
TEST_F(FlatlandDifferentialTest, CullingOpaqueOccluder) {
  auto script = [this](Flatland* flatland, allocation::Allocator* allocator,
                       std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kTransSmall{2};
    const TransformId kTransFull{3};
    const ContentId kImageSmall{4};
    const ContentId kImageFull{5};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kTransSmall);
    flatland->CreateTransform(kTransFull);
    flatland->AddChild(kRootId, kTransSmall);
    flatland->AddChild(kRootId, kTransFull);

    fuchsia_ui_composition::ImageProperties properties_small;
    properties_small.size(fuchsia_math::SizeU{100, 100});
    auto ref_pair1 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageSmall, std::move(ref_pair1), properties_small);
    flatland->SetContent(kTransSmall, kImageSmall);

    fuchsia_ui_composition::ImageProperties properties_full;
    properties_full.size(fuchsia_math::SizeU{1000, 1000});
    auto ref_pair2 = allocation::cpp::BufferCollectionImportExportTokens::New();
    CreateImage(flatland, allocator, kImageFull, std::move(ref_pair2), properties_full);
    flatland->SetContent(kTransFull, kImageFull);

    // Frame 1: Full-screen image is transparent/premultiplied (non-opaque), so culling does not
    // occur.
    flatland->SetImageBlendMode(kImageFull, BlendMode::kPremultipliedAlpha());
    present_and_capture();

    // Frame 2: Full-screen image is opaque (kReplace), culling the small image behind it.
    flatland->SetImageBlendMode(kImageFull, BlendMode::kReplace());
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  // Verify the script resulted in 2 frames:
  // - Frame 1: 2 layers (non-opaque full-screen does not cull the small image).
  // - Frame 2: 1 layer (opaque full-screen culls the small image behind it).
  ASSERT_EQ(layers1.size(), 2U);
  EXPECT_EQ(layers1[0].size(), 2U);
  EXPECT_EQ(layers1[1].size(), 1U);
}

// 22. FilledRectExplicitReplace
// The call order used by the CTF pixel tests: fill first, blend mode second.
// For an opaque fill (i.e. alpha = 1.f) the value derived by `SetSolidFill()`
// matches the subsequent REPLACE set by `SetImageBlendMode()`, so the blend mode
// remains REPLACE.
TEST_F(FlatlandDifferentialTest, FilledRectExplicitReplace) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kRectId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 1.0f},
                           fuchsia_math::SizeU{120, 240});
    flatland->SetImageBlendMode(kRectId, BlendMode::kReplace());

    flatland->SetContent(kRootId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.5f, 0.25f, 0.75f, 1.0f}));
}

// 23. FilledRectTranslucentReplace
// Last call wins: SetSolidFill derives PREMULTIPLIED_ALPHA from the
// translucent fill, then the explicit REPLACE overrides it. The layer
// stays REPLACE because demotion depends on effective opacity (1 here),
// never on content alpha. This is the hole-punch mechanism: REPLACE
// writes the premultiplied color, sub-unity alpha included, verbatim.
TEST_F(FlatlandDifferentialTest, FilledRectTranslucentReplace) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kRectId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 0.5f},
                           fuchsia_math::SizeU{120, 240});
    flatland->SetImageBlendMode(kRectId, BlendMode::kReplace());

    flatland->SetContent(kRootId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.25f, 0.125f, 0.375f, 0.5f}));
}

// 24. FilledRectFillClobbersBlend
// The reverse order: SetSolidFill re-derives the blend mode from the
// fill alpha on every call, silently clobbering the explicit REPLACE set
// beforehand. Classic Flatland1 behavior: last call wins.
TEST_F(FlatlandDifferentialTest, FilledRectFillClobbersBlend) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kRectId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetImageBlendMode(kRectId, BlendMode::kReplace());
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 0.5f},
                           fuchsia_math::SizeU{120, 240});

    flatland->SetContent(kRootId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.25f, 0.125f, 0.375f, 0.5f}));
}

// 25. FilledRectPunch
// A hole punch: alpha 0 fill under REPLACE. Premultiplication by content
// alpha zeroes the RGB channels, so the authored color is irrelevant and
// both schemas emit {0,0,0,0} with REPLACE.
TEST_F(FlatlandDifferentialTest, FilledRectPunch) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kRectId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 0.0f},
                           fuchsia_math::SizeU{120, 240});
    flatland->SetImageBlendMode(kRectId, BlendMode::kReplace());

    flatland->SetContent(kRootId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kReplace());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.f, 0.f, 0.f, 0.f}));
}

// FilledRectStraightAlpha
// Setting STRAIGHT_ALPHA on a solid fill should be normalized to PREMULTIPLIED_ALPHA
// and both schemas should produce identical ResolvedLayers with premultiplied color.
TEST_F(FlatlandDifferentialTest, FilledRectStraightAlpha) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const ContentId kRectId{2};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 0.5f},
                           fuchsia_math::SizeU{120, 240});
    flatland->SetImageBlendMode(kRectId, BlendMode::kStraightAlpha());

    flatland->SetContent(kRootId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{1.f, 1.f, 1.f, 1.f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.25f, 0.125f, 0.375f, 0.5f}));
}

// 26. FilledRectUnderFade
// An opaque fill derives REPLACE, but under a faded ancestor the blend
// demotes to PREMULTIPLIED_ALPHA so the fade reveals the background
// (continuously until it becomes invisible at opacity 0).
TEST_F(FlatlandDifferentialTest, FilledRectUnderFade) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kTransId{2};
    const ContentId kRectId{3};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);
    flatland->SetOpacity(kRootId, 0.5f);

    flatland->CreateTransform(kTransId);
    flatland->AddChild(kRootId, kTransId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 1.0f},
                           fuchsia_math::SizeU{120, 240});

    flatland->SetContent(kTransId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.5f, 0.25f, 0.75f, 1.0f}));
}

// 27. FilledRectTranslucentUnderFade
// Verifies that results agree when there is both:
//   - translucent color specified by fill content (implying PREMULTIPLIED_ALPHA blend mode)
//   - inherited opacity < 1
TEST_F(FlatlandDifferentialTest, FilledRectTranslucentUnderFade) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kTransId{2};
    const ContentId kRectId{3};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);
    flatland->SetOpacity(kRootId, 0.5f);

    flatland->CreateTransform(kTransId);
    flatland->AddChild(kRootId, kTransId);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 0.5f},
                           fuchsia_math::SizeU{120, 240});

    flatland->SetContent(kTransId, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{0.5f, 0.5f, 0.5f, 0.5f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.25f, 0.125f, 0.375f, 0.5f}));
}

// 28. FilledRectUnderNestedFades
// Nested fades compose multiplicatively: 0.5 * 0.5 = 0.25 arrives in
// multiply_color as a single product, and the composed value (not either
// factor alone) drives the REPLACE demotion.
TEST_F(FlatlandDifferentialTest, FilledRectUnderNestedFades) {
  auto script = [](Flatland* flatland, allocation::Allocator* allocator,
                   std::function<void()> present_and_capture) {
    const TransformId kRootId{1};
    const TransformId kTrans1{2};
    const TransformId kTrans2{3};
    const ContentId kRectId{4};

    flatland->CreateTransform(kRootId);
    flatland->SetRootTransform(kRootId);

    flatland->CreateTransform(kTrans1);
    flatland->CreateTransform(kTrans2);
    flatland->AddChild(kRootId, kTrans1);
    flatland->AddChild(kTrans1, kTrans2);
    flatland->SetOpacity(kTrans1, 0.5f);
    flatland->SetOpacity(kTrans2, 0.5f);

    flatland->CreateFilledRect(kRectId);
    flatland->SetSolidFill(kRectId, fuchsia_ui_composition::ColorRgba{0.5f, 0.25f, 0.75f, 1.0f},
                           fuchsia_math::SizeU{120, 240});

    flatland->SetContent(kTrans2, kRectId);
    present_and_capture();
  };

  auto layers1 = RunScript(false, script);
  auto layers2 = RunScript(true, script);
  ExpectMultiPresentResolvedLayersEqual(layers1, layers2);

  ASSERT_EQ(layers2.size(), 1U);
  ASSERT_EQ(layers2[0].size(), 1U);
  const auto& layer = layers2[0][0];
  EXPECT_EQ(layer.blend_mode, BlendMode::kPremultipliedAlpha());
  EXPECT_EQ(layer.multiply_color, (std::array<float, 4>{0.25f, 0.25f, 0.25f, 0.25f}));
  ASSERT_TRUE(std::holds_alternative<ResolvedLayer::SolidColorContent>(layer.content));
  const auto& content = std::get<ResolvedLayer::SolidColorContent>(layer.content);
  EXPECT_EQ(content.color, (std::array<float, 4>{0.5f, 0.25f, 0.75f, 1.0f}));
}

}  // namespace test
}  // namespace flatland
