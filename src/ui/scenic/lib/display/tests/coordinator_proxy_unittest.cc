// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/display/coordinator_proxy.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.math/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/task.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/time.h>

#include <memory>
#include <string>
#include <string_view>
#include <vector>

#include <gmock/gmock.h>
#include <gtest/gtest.h>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/ui/scenic/lib/display/tests/mock_display_coordinator.h"
#include "src/ui/scenic/lib/utils/helpers.h"

namespace display::test {

class CoordinatorProxyTest : public gtest::RealLoopFixture {
 public:
  static constexpr uint32_t kMaxDisplayLayersCount = 2;

  constexpr static std::array<WireDisplayMode, 2> kSupportedDisplayModes = {
      fuchsia_hardware_display_types::wire::Mode{.active_area = {.width = 1024, .height = 768},
                                                 .refresh_rate_millihertz = 60000,
                                                 .flags = {}},
      fuchsia_hardware_display_types::wire::Mode{.active_area = {.width = 1920, .height = 1080},
                                                 .refresh_rate_millihertz = 60000,
                                                 .flags = {}}};

  // Used to construct the default `mock_coordinator_` that is available in all tests.  It is also
  // available to tests that need to create their own mock coordinator (and `CoordinatorProxy`).
  std::pair<std::unique_ptr<testing::StrictMock<MockDisplayCoordinator>>,
            std::unique_ptr<CoordinatorProxy>>
  MakeMockDisplayCoordinatorAndCoordinatorProxy(
      CoordinatorProxy::CheckConfigHeuristics check_conifg_heuristics = {}) {
    zx::result endpoints_result = fidl::CreateEndpoints<fuchsia_hardware_display::Coordinator>();
    FX_CHECK(endpoints_result.is_ok())
        << "Failed to create FIDL endpoints for the display coordinator: "
        << endpoints_result.status_string();
    auto [coordinator_client, coordinator_server] = std::move(endpoints_result).value();

    fuchsia_hardware_display::wire::Info display_info = {
        .modes = fidl::VectorView<fuchsia_hardware_display_types::wire::Mode>::FromExternal(
            const_cast<WireDisplayMode*>(kSupportedDisplayModes.data()),
            kSupportedDisplayModes.size()),
        .max_layer_count = kMaxDisplayLayersCount,
    };

    auto mock_coordinator =
        std::make_unique<testing::StrictMock<MockDisplayCoordinator>>(display_info);
    // The fidl::Server requires the binding and teardown to occur on the
    // same thread where the FIDL server runs.
    libsync::Completion completion;
    async::PostTask(display_coordinator_loop_.dispatcher(),
                    [this, &mock_coordinator, &completion,
                     coordinator_server = std::move(coordinator_server)]() mutable {
                      mock_coordinator->Bind(std::move(coordinator_server), {},
                                             display_coordinator_loop_.dispatcher());
                      completion.Signal();
                    });
    completion.Wait();

    auto coordinator_proxy = std::make_unique<CoordinatorProxy>(
        std::move(coordinator_client), dispatcher(), check_conifg_heuristics);

    return std::pair{std::move(mock_coordinator), std::move(coordinator_proxy)};
  }

  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    sysmem_allocator_ = utils::CreateSysmemAllocatorClient(dispatcher(), "CoordinatorProxyTest");

    display_coordinator_loop_.StartThread("display-coordinator-loop");

    auto mock_and_proxy = MakeMockDisplayCoordinatorAndCoordinatorProxy();
    mock_coordinator_ = std::move(mock_and_proxy.first);
    coordinator_proxy_ = std::move(mock_and_proxy.second);
  }

  void TearDown() override {
    coordinator_proxy_.reset();

    // This is to make sure that the display coordinator loop has finished
    // handling all its pending tasks.
    RunLoopUntilIdle();
    libsync::Completion completion;
    async::PostTask(display_coordinator_loop_.dispatcher(), [this, &completion]() mutable {
      display_coordinator_loop_.RunUntilIdle();
      mock_coordinator_.reset();
      completion.Signal();
    });
    completion.Wait();
    display_coordinator_loop_.Quit();
    display_coordinator_loop_.JoinThreads();

    gtest::RealLoopFixture::TearDown();
  }

  // Synchronously calls `GetLatestCommittedConfigStamp()` on the display coordinator.  When we
  // receive the response from this, we know that the coordinator has also received/processed any
  // previously-sent messages.
  void PingMockDisplayCoordinator(CoordinatorProxy* proxy = nullptr) {
    auto _ =
        (proxy ? proxy : coordinator_proxy_.get())->raw().sync()->GetLatestCommittedConfigStamp();
  }

 protected:
  static constexpr fuchsia_images2::PixelFormat kPixelFormat =
      fuchsia_images2::PixelFormat::kB8G8R8A8;

  async::Loop display_coordinator_loop_{&kAsyncLoopConfigNeverAttachToThread};

  std::unique_ptr<MockDisplayCoordinator> mock_coordinator_;
  std::unique_ptr<CoordinatorProxy> coordinator_proxy_;

  // Only for use on the main thread. Establish a new connection when on the MockDisplayCoordinator
  // thread.
  fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator_;
};

// Test that CreateLayer() and DestroyLayer() immediately send the corresponding FIDL calls.
TEST_F(CoordinatorProxyTest, CreateAndDestroyLayer) {
  bool create_layer_called = false;
  mock_coordinator_->set_create_layer_fn([&](WireLayerId layer_id) { create_layer_called = true; });

  std::optional<LayerId> destroyed_layer_id;
  mock_coordinator_->set_destroy_layer_fn(
      [&](fuchsia_hardware_display::wire::CoordinatorDestroyLayerRequest* request) {
        destroyed_layer_id = LayerId(request->layer_id);
      });

  // CreateLayer should be called on the mock.
  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  EXPECT_TRUE(create_layer_called);
  EXPECT_NE(layer_id, kInvalidLayerId);

  // DestroyLayer should be called on the mock with the correct ID.
  coordinator_proxy_->DestroyLayer(layer_id);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(destroyed_layer_id.has_value());

  EXPECT_EQ(destroyed_layer_id.value(), layer_id);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetDisplayMode() does not immediately send a FIDL call, but that the call is sent
// when ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyDisplayMode) {
  bool set_display_mode_called = false;
  WireDisplayId captured_display_id = {};
  WireDisplayMode captured_mode = {};

  mock_coordinator_->set_display_mode_fn([&](WireDisplayId display_id, WireDisplayMode mode) {
    set_display_mode_called = true;
    captured_display_id = display_id;
    captured_mode = mode;
  });

  // SetDisplayMode should not call the mock.
  const DisplayId display_id(1);
  const DisplayMode display_mode({.active_area = types::Extent2({.width = 1920, .height = 1080}),
                                  .refresh_rate_millihertz = 60000,
                                  .mode_flags = 0});
  coordinator_proxy_->SetDisplayMode(display_id, display_mode);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_display_mode_called);

  // ApplyConfig should call the mock.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_display_mode_called);
  EXPECT_EQ(captured_display_id.value, display_id.value());
  EXPECT_EQ(captured_mode.active_area.width, 1920u);
  EXPECT_EQ(captured_mode.active_area.height, 1080u);
  EXPECT_EQ(captured_mode.refresh_rate_millihertz, 60000u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetDisplayColorConversion() does not immediately send a FIDL call, but that the call
// is sent when ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyDisplayColorConversion) {
  bool set_display_color_conversion_called = false;
  WireDisplayId captured_display_id = {};
  std::array<float, 3> captured_preblend;
  std::array<float, 9> captured_coefficients;
  std::array<float, 3> captured_postblend;

  mock_coordinator_->set_display_color_conversion_fn(
      [&](WireDisplayId display_id, fidl::Array<float, 3> preblend,
          fidl::Array<float, 9> coefficients, fidl::Array<float, 3> postblend) {
        set_display_color_conversion_called = true;
        captured_display_id = display_id;

        memcpy(captured_preblend.data(), preblend.data(), sizeof(captured_preblend));
        memcpy(captured_coefficients.data(), coefficients.data(), sizeof(captured_coefficients));
        memcpy(captured_postblend.data(), postblend.data(), sizeof(captured_postblend));
      });

  // SetDisplayColorConversion should not call the mock.
  const DisplayId display_id(1);
  const std::array<float, 3> kExpectedPreblend = {0.1f, 0.2f, 0.3f};
  const std::array<float, 9> kExpectedCoefficients = {1.f, 0.f, 0.f, 0.f, 1.f, 0.f, 0.f, 0.f, 1.f};
  const std::array<float, 3> kExpectedPostblend = {0.4f, 0.5f, 0.6f};
  coordinator_proxy_->SetDisplayColorConversion(display_id, kExpectedPreblend,
                                                kExpectedCoefficients, kExpectedPostblend);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_display_color_conversion_called);

  // ApplyConfig should call the mock.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_display_color_conversion_called);
  EXPECT_EQ(captured_display_id.value, display_id.value());
  EXPECT_EQ(captured_preblend, kExpectedPreblend);
  EXPECT_EQ(captured_coefficients, kExpectedCoefficients);
  EXPECT_EQ(captured_postblend, kExpectedPostblend);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetDisplayLayers() does not immediately send a FIDL call, but that the call is sent
// when ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyDisplayLayers) {
  bool set_display_layers_called = false;
  WireDisplayId captured_display_id = {};
  std::vector<LayerId> captured_layers;

  mock_coordinator_->set_set_display_layers_fn(
      [&](WireDisplayId display_id, fidl::VectorView<WireLayerId> layers) {
        set_display_layers_called = true;
        captured_display_id = display_id;
        captured_layers.clear();
        captured_layers.reserve(layers.size());
        for (const auto& wire_layer : layers) {
          captured_layers.push_back(LayerId(wire_layer));
        }
      });

  // Create a couple of layers to use.
  const LayerId layer_id1 = coordinator_proxy_->CreateLayer();
  const LayerId layer_id2 = coordinator_proxy_->CreateLayer();

  // SetDisplayLayers should not call the mock.
  const DisplayId display_id(1);
  const std::vector<LayerId> kExpectedLayers = {layer_id1, layer_id2};
  coordinator_proxy_->SetDisplayLayers(display_id, std::span<const LayerId>(kExpectedLayers));
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_display_layers_called);

  // ApplyConfig should call the mock.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_display_layers_called);
  EXPECT_EQ(captured_display_id.value, display_id.value());
  EXPECT_EQ(captured_layers, kExpectedLayers);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetLayerPrimaryConfig(), SetLayerPrimaryPosition(), SetLayerPrimaryAlpha(), and
// SetLayerImage() do not immediately send FIDL calls, but that the calls are sent when
// ApplyConfig() is called.
TEST_F(CoordinatorProxyTest, SetAndApplyLayerImage) {
  bool set_layer_primary_config_called = false;
  WireLayerId captured_config_layer_id = {};
  WireImageMetadata captured_image_metadata = {};
  mock_coordinator_->set_set_layer_primary_config_fn(
      [&](WireLayerId layer_id, WireImageMetadata image_metadata) {
        set_layer_primary_config_called = true;
        captured_config_layer_id = layer_id;
        captured_image_metadata = image_metadata;
      });

  bool set_layer_primary_position_called = false;
  WireLayerId captured_position_layer_id = {};
  WireCoordinateTransformation captured_transform = {};
  WireRectU captured_src_frame = {};
  WireRectU captured_dest_frame = {};
  mock_coordinator_->set_layer_primary_position_fn([&](WireLayerId layer_id,
                                                       WireCoordinateTransformation transform,
                                                       WireRectU src_frame, WireRectU dest_frame) {
    set_layer_primary_position_called = true;
    captured_position_layer_id = layer_id;
    captured_transform = transform;
    captured_src_frame = src_frame;
    captured_dest_frame = dest_frame;
  });

  bool set_layer_primary_alpha_called = false;
  WireLayerId captured_alpha_layer_id = {};
  WireAlphaMode captured_alpha_mode = {};
  float captured_alpha_val = 0.f;
  mock_coordinator_->set_set_layer_primary_alpha_fn(
      [&](WireLayerId layer_id, WireAlphaMode mode, float val) {
        set_layer_primary_alpha_called = true;
        captured_alpha_layer_id = layer_id;
        captured_alpha_mode = mode;
        captured_alpha_val = val;
      });

  bool set_layer_image_called = false;
  WireLayerId captured_image_layer_id = {};
  WireImageId captured_image_id = {};
  WireEventId captured_wait_event_id = {};
  mock_coordinator_->set_set_layer_image_fn(
      [&](WireLayerId layer_id, WireImageId image_id, WireEventId wait_event_id) {
        set_layer_image_called = true;
        captured_image_layer_id = layer_id;
        captured_image_id = image_id;
        captured_wait_event_id = wait_event_id;
      });

  // Create a layer to use.
  const LayerId layer_id = coordinator_proxy_->CreateLayer();

  // Call the SetLayer* methods. These should not call the mock yet.
  const ImageId kImageId(1);
  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const uint32_t kImageTilingType = 0;
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);

  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);

  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kAlpha);

  const EventId kWaitEventId(1);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kWaitEventId);

  // The proxy sends layer diffs only for displayed layers.
  const DisplayId kDisplayId(1);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));

  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_primary_config_called);
  EXPECT_FALSE(set_layer_primary_position_called);
  EXPECT_FALSE(set_layer_primary_alpha_called);
  EXPECT_FALSE(set_layer_image_called);

  // ApplyConfig should call the mock functions.
  const WireConfigStamp config_stamp{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);

  EXPECT_TRUE(set_layer_primary_config_called);
  EXPECT_EQ(captured_config_layer_id.value, layer_id.value());
  EXPECT_EQ(Extent2::From(captured_image_metadata.dimensions), kImageExtent);
  EXPECT_EQ(captured_image_metadata.tiling_type, kImageTilingType);

  EXPECT_TRUE(set_layer_primary_position_called);
  EXPECT_EQ(captured_position_layer_id.value, layer_id.value());
  EXPECT_EQ(RotateFlip::From(captured_transform), kTransform);
  EXPECT_EQ(Rectangle::From(captured_src_frame), kSrcFrame);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame);

  EXPECT_TRUE(set_layer_primary_alpha_called);
  EXPECT_EQ(captured_alpha_layer_id.value, layer_id.value());
  EXPECT_EQ(BlendMode::From(captured_alpha_mode), kBlendMode);
  EXPECT_EQ(captured_alpha_val, kAlpha);

  EXPECT_TRUE(set_layer_image_called);
  EXPECT_EQ(captured_image_layer_id.value, layer_id.value());
  EXPECT_EQ(captured_image_id.value, kImageId.value());
  EXPECT_EQ(captured_wait_event_id.value, kWaitEventId.value());

  // Updating alpha within (0.0, 1.0) should send `SetLayerPrimaryAlpha()` on the next
  // `ApplyConfig()`.
  const uint64_t initial_check_config_calls_skipped =
      coordinator_proxy_->check_config_calls_skipped();
  set_layer_primary_alpha_called = false;
  const float kUpdatedAlpha = 0.25f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kUpdatedAlpha);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_layer_primary_alpha_called);
  EXPECT_EQ(captured_alpha_val, kUpdatedAlpha);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(),
            initial_check_config_calls_skipped + 1);

  // Calling `ApplyConfig()` again without changing alpha skips the alpha update.
  set_layer_primary_alpha_called = false;
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_primary_alpha_called);

  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// The Coordinator resets a layer's position and alpha whenever `SetLayerPrimaryConfig()` is called.
// Test that the proxy re-sends them when it sends `SetLayerPrimaryConfig()`, even if they are
// unchanged, and that it mirrors the reset when the client doesn't re-set them.
TEST_F(CoordinatorProxyTest, SetLayerPrimaryConfigResetsPositionAndAlpha) {
  // The re-sent position and alpha only take effect if they arrive after the config, which
  // resets them, so the test records the order of the calls as well as their counts.
  std::vector<std::string> calls;

  int set_layer_primary_config_count = 0;
  mock_coordinator_->set_set_layer_primary_config_fn(
      [&](WireLayerId layer_id, WireImageMetadata image_metadata) {
        ++set_layer_primary_config_count;
        calls.push_back("config");
      });

  int set_layer_primary_position_count = 0;
  WireCoordinateTransformation captured_transform = {};
  WireRectU captured_src_frame = {};
  WireRectU captured_dest_frame = {};
  mock_coordinator_->set_layer_primary_position_fn([&](WireLayerId layer_id,
                                                       WireCoordinateTransformation transform,
                                                       WireRectU src_frame, WireRectU dest_frame) {
    ++set_layer_primary_position_count;
    calls.push_back("position");
    captured_transform = transform;
    captured_src_frame = src_frame;
    captured_dest_frame = dest_frame;
  });

  int set_layer_primary_alpha_count = 0;
  WireAlphaMode captured_alpha_mode = {};
  float captured_alpha_val = 0.f;
  mock_coordinator_->set_set_layer_primary_alpha_fn(
      [&](WireLayerId layer_id, WireAlphaMode mode, float val) {
        ++set_layer_primary_alpha_count;
        calls.push_back("alpha");
        captured_alpha_mode = mode;
        captured_alpha_val = val;
      });

  mock_coordinator_->set_set_layer_image_fn(
      [&](WireLayerId layer_id, WireImageId image_id, WireEventId wait_event_id) {
        calls.push_back("image");
      });

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const ImageId kImageId(1);
  const uint32_t kImageTilingType = 0;
  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;

  auto set_image_layer = [&](const types::Extent2& image_extent) {
    coordinator_proxy_->SetLayerPrimaryConfig(layer_id, image_extent, kImageTilingType);
    coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);
    coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kAlpha);
    coordinator_proxy_->SetLayerImage(layer_id, kImageId, kInvalidEventId);
  };

  set_image_layer(types::Extent2({.width = 100, .height = 100}));
  // The proxy sends layer diffs only for displayed layers.
  coordinator_proxy_->SetDisplayLayers(DisplayId(1), std::span<const LayerId>(&layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);
  EXPECT_EQ(set_layer_primary_config_count, 1);
  EXPECT_EQ(set_layer_primary_position_count, 1);
  EXPECT_EQ(set_layer_primary_alpha_count, 1);

  // Same config, position, and alpha: nothing but the image is re-sent.
  set_image_layer(types::Extent2({.width = 100, .height = 100}));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(set_layer_primary_config_count, 1);
  EXPECT_EQ(set_layer_primary_position_count, 1);
  EXPECT_EQ(set_layer_primary_alpha_count, 1);
  EXPECT_EQ(calls, (std::vector<std::string>{"config", "position", "alpha", "image", "image"}));
  calls.clear();

  // Changing only the image dimensions re-sends the config, and therefore also the (unchanged)
  // position and alpha, which the coordinator resets.
  set_image_layer(types::Extent2({.width = 120, .height = 120}));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(calls, (std::vector<std::string>{"config", "position", "alpha", "image"}));
  EXPECT_EQ(set_layer_primary_config_count, 2);
  EXPECT_EQ(set_layer_primary_position_count, 2);
  EXPECT_EQ(RotateFlip::From(captured_transform), kTransform);
  EXPECT_EQ(Rectangle::From(captured_src_frame), kSrcFrame);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame);
  EXPECT_EQ(set_layer_primary_alpha_count, 2);
  EXPECT_EQ(BlendMode::From(captured_alpha_mode), kBlendMode);
  EXPECT_EQ(captured_alpha_val, kAlpha);

  // Calling `SetLayerPrimaryConfig()` without re-setting position and alpha resets them to the
  // defaults, just like the coordinator does.
  const types::Extent2 kImageExtent({.width = 120, .height = 120});
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kInvalidEventId);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 4}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  const Rectangle kImageArea({.x = 0, .y = 0, .width = 120, .height = 120});
  EXPECT_EQ(set_layer_primary_position_count, 3);
  EXPECT_EQ(RotateFlip::From(captured_transform), RotateFlip::kIdentity());
  EXPECT_EQ(Rectangle::From(captured_src_frame), kImageArea);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kImageArea);
  EXPECT_EQ(set_layer_primary_alpha_count, 3);
  EXPECT_EQ(BlendMode::From(captured_alpha_mode), BlendMode::kReplace());
  EXPECT_EQ(captured_alpha_val, 1.f);

  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that an ApplyConfig() failure followed by applying a different property successfully does
// not send the abandoned draft alpha.
TEST_F(CoordinatorProxyTest, ResetDraftStateDiscardsAbandonedAlphaOnApplyConfigFailure) {
  bool set_layer_primary_alpha_called = false;
  float captured_alpha_val = 0.f;
  mock_coordinator_->set_set_layer_primary_alpha_fn(
      [&](WireLayerId layer_id, WireAlphaMode alpha_mode, float alpha_val) {
        set_layer_primary_alpha_called = true;
        captured_alpha_val = alpha_val;
      });

  bool set_layer_primary_position_called = false;
  mock_coordinator_->set_layer_primary_position_fn(
      [&](WireLayerId layer_id, WireCoordinateTransformation transform, WireRectU src_frame,
          WireRectU dest_frame) { set_layer_primary_position_called = true; });

  const LayerId layer_id = coordinator_proxy_->CreateLayer();
  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const uint32_t kImageTilingType = 0;
  coordinator_proxy_->SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);

  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);

  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kInitialAlpha = 0.5f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kInitialAlpha);

  const ImageId kImageId(1);
  const EventId kWaitEventId(1);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kWaitEventId);

  const DisplayId kDisplayId(1);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&layer_id, 1));

  // Successfully apply the initial config.
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_TRUE(set_layer_primary_alpha_called);
  EXPECT_EQ(captured_alpha_val, kInitialAlpha);

  // Update alpha to a new value and modify position so the config is not cached, then make
  // CheckConfig() fail.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kInvalidConfig; });
  const float kAbandonedAlpha = 0.8f;
  coordinator_proxy_->SetLayerPrimaryAlpha(layer_id, kBlendMode, kAbandonedAlpha);
  const Rectangle kFailingDestFrame({.x = 0, .y = 0, .width = 500, .height = 500});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kFailingDestFrame);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(),
            ZX_ERR_BAD_STATE);
  PingMockDisplayCoordinator();

  // Allow CheckConfig() to succeed again.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kOk; });

  // Apply a different property (destination frame) without changing alpha.
  // The proxy should have reset the draft alpha back to kInitialAlpha, so no alpha update
  // should be sent.
  set_layer_primary_alpha_called = false;
  set_layer_primary_position_called = false;
  const Rectangle kNewDestFrame({.x = 0, .y = 0, .width = 300, .height = 300});
  coordinator_proxy_->SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kNewDestFrame);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();

  EXPECT_TRUE(set_layer_primary_position_called);
  EXPECT_FALSE(set_layer_primary_alpha_called);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that SetLayerColorConfig() does not immediately send a FIDL call, but that the call is
// sent when ApplyConfig() is called.  Also test transitioning between image and color layers.
TEST_F(CoordinatorProxyTest, SetAndApplyLayerColor) {
  bool set_layer_color_config_called = false;
  WireLayerId captured_layer_id = {};
  WireColor captured_color = {};
  WireRectU captured_dest_frame = {};

  mock_coordinator_->set_set_layer_color_config_fn(
      [&](WireLayerId layer_id, WireColor color, WireRectU dest_frame) {
        set_layer_color_config_called = true;
        captured_layer_id = layer_id;
        captured_color = color;
        captured_dest_frame = dest_frame;
      });

  // Create a layer to use.
  const LayerId layer_id = coordinator_proxy_->CreateLayer();

  // Call SetLayerColorConfig. This should not call the mock yet.
  const std::array<float, 4> kColorBgra = {0.1f, 0.2f, 0.3f, 0.4f};
  const WireColor kWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes =
          {
              static_cast<uint8_t>(kColorBgra[0] * 255.f),  // Blue
              static_cast<uint8_t>(kColorBgra[1] * 255.f),  // Green
              static_cast<uint8_t>(kColorBgra[2] * 255.f),  // Red
              static_cast<uint8_t>(kColorBgra[3] * 255.f),  // Alpha
          },
  };

  const Rectangle kDestFrame({.x = 10, .y = 20, .width = 100, .height = 200});
  coordinator_proxy_->SetLayerColorConfig(layer_id, kWireColor, kDestFrame);

  // The proxy sends layer diffs only for displayed layers.
  coordinator_proxy_->SetDisplayLayers(DisplayId(1), std::span<const LayerId>(&layer_id, 1));

  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_color_config_called);

  // ApplyConfig should call the mock function.
  const WireConfigStamp config_stamp1{.value = 1};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp1).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);

  EXPECT_TRUE(set_layer_color_config_called);
  EXPECT_EQ(captured_layer_id.value, layer_id.value());
  EXPECT_EQ(captured_color.format, fuchsia_images2::wire::PixelFormat::kB8G8R8A8);
  EXPECT_EQ(captured_color.bytes, kWireColor.bytes);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);

  // Transition to an image layer.
  set_layer_color_config_called = false;
  const ImageId kImageId(1);
  const EventId kWaitEventId(1);
  coordinator_proxy_->SetLayerImage(layer_id, kImageId, kWaitEventId);

  const WireConfigStamp config_stamp2{.value = 2};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp2).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_FALSE(set_layer_color_config_called);  // Should not be called

  // Transition back to a color layer (RGBA this time, not BGRA).
  const std::array<float, 4> kColorRgba = {0.5f, 0.6f, 0.7f, 0.8f};
  const WireColor kWireColor2 = {
      .format = fuchsia_images2::wire::PixelFormat::kR8G8B8A8,
      .bytes =
          {
              static_cast<uint8_t>(kColorRgba[0] * 255.f),  // Red
              static_cast<uint8_t>(kColorRgba[1] * 255.f),  // Green
              static_cast<uint8_t>(kColorRgba[2] * 255.f),  // Blue
              static_cast<uint8_t>(kColorRgba[3] * 255.f),  // Alpha
          },
  };

  const Rectangle kDestFrame2({.x = 30, .y = 40, .width = 150, .height = 250});
  coordinator_proxy_->SetLayerColorConfig(layer_id, kWireColor2, kDestFrame2);

  const WireConfigStamp config_stamp3{.value = 3};
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(config_stamp3).status_value(), ZX_OK);
  PingMockDisplayCoordinator();

  EXPECT_TRUE(set_layer_color_config_called);
  EXPECT_EQ(captured_layer_id.value, layer_id.value());
  EXPECT_EQ(captured_color.format, fuchsia_images2::wire::PixelFormat::kR8G8B8A8);
  EXPECT_EQ(captured_color.bytes, kWireColor2.bytes);
  EXPECT_EQ(Rectangle::From(captured_dest_frame), kDestFrame2);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Records the layer calls that a mock coordinator receives, in order, as "<method>:<layer id>",
// along with the last values sent.
struct LayerCallRecorder {
  static std::string Call(std::string_view method, const LayerId& layer_id) {
    return std::string(method) + ":" + std::to_string(layer_id.value());
  }

  void Attach(MockDisplayCoordinator& mock) {
    mock.set_set_layer_primary_config_fn([this](WireLayerId layer_id, WireImageMetadata metadata) {
      calls.push_back(Call("config", LayerId(layer_id)));
      image_metadata = metadata;
    });
    mock.set_layer_primary_position_fn([this](WireLayerId layer_id,
                                              WireCoordinateTransformation transform,
                                              WireRectU src_frame, WireRectU dest_frame) {
      calls.push_back(Call("position", LayerId(layer_id)));
      position_dest_frame = dest_frame;
    });
    mock.set_set_layer_primary_alpha_fn(
        [this](WireLayerId layer_id, WireAlphaMode mode, float value) {
          calls.push_back(Call("alpha", LayerId(layer_id)));
          alpha_value = value;
        });
    mock.set_set_layer_image_fn(
        [this](WireLayerId layer_id, WireImageId wire_image_id, WireEventId wait_event_id) {
          calls.push_back(Call("image", LayerId(layer_id)));
          image_id = wire_image_id;
        });
    mock.set_set_layer_color_config_fn(
        [this](WireLayerId layer_id, WireColor wire_color, WireRectU dest_frame) {
          calls.push_back(Call("color", LayerId(layer_id)));
          color = wire_color;
        });
  }

  std::vector<std::string> calls;
  WireImageMetadata image_metadata = {};
  WireRectU position_dest_frame = {};
  float alpha_value = 0.f;
  WireImageId image_id = {};
  WireColor color = {};
};

// The Coordinator's `CommitConfig()` applies draft changes only to layers on a display, and
// `DiscardConfig()` drops the rest.  Test that the proxy sends no diffs for layers that are not in
// the display list, and sends all of them once the layers are displayed, because none were
// accepted while the layers were not displayed.
TEST_F(CoordinatorProxyTest, UndisplayedLayerDiffsAreHeldUntilDisplayed) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId image_layer_id = coordinator_proxy_->CreateLayer();
  const LayerId color_layer_id = coordinator_proxy_->CreateLayer();

  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  const float kAlpha = 0.5f;
  const ImageId kImageId(1);
  coordinator_proxy_->SetLayerPrimaryConfig(image_layer_id, kImageExtent, /*image_tiling_type=*/0);
  coordinator_proxy_->SetLayerPrimaryPosition(
      image_layer_id, RotateFlip::kIdentity(),
      Rectangle({.x = 0, .y = 0, .width = 100, .height = 100}), kDestFrame);
  coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, BlendMode::kStraightAlpha(), kAlpha);
  coordinator_proxy_->SetLayerImage(image_layer_id, kImageId, kInvalidEventId);

  const WireColor kWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {10, 20, 30, 40},
  };
  coordinator_proxy_->SetLayerColorConfig(color_layer_id, kWireColor,
                                          Rectangle({.x = 0, .y = 0, .width = 50, .height = 50}));

  // Neither layer is displayed, so none of their diffs are sent.
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_THAT(recorder.calls, testing::IsEmpty());
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 0u);

  // Once displayed, all of each layer's diffs are sent, without any further `Set*()` calls.
  const std::vector<LayerId> kBothLayers = {image_layer_id, color_layer_id};
  coordinator_proxy_->SetDisplayLayers(DisplayId(1), kBothLayers);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls, (std::vector<std::string>{
                                LayerCallRecorder::Call("config", image_layer_id),
                                LayerCallRecorder::Call("position", image_layer_id),
                                LayerCallRecorder::Call("alpha", image_layer_id),
                                LayerCallRecorder::Call("image", image_layer_id),
                                LayerCallRecorder::Call("color", color_layer_id),
                            }));
  EXPECT_EQ(Extent2::From(recorder.image_metadata.dimensions), kImageExtent);
  EXPECT_EQ(Rectangle::From(recorder.position_dest_frame), kDestFrame);
  EXPECT_EQ(recorder.alpha_value, kAlpha);
  EXPECT_EQ(recorder.image_id.value, kImageId.value());
  EXPECT_EQ(recorder.color.bytes, kWireColor.bytes);
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 1u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that a layer removed from the display list in the same frame that its properties change
// sends no diffs, and that when it is displayed again, its diffs are computed against the state
// that was last applied while it was displayed.  If the proxy accepted the undisplayed layer's
// draft, it would disagree with the Coordinator, which drops such drafts on `DiscardConfig()`.
TEST_F(CoordinatorProxyTest, RemovedLayerDiffsAreHeldUntilDisplayedAgain) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId image_layer_id = coordinator_proxy_->CreateLayer();
  const LayerId color_layer_id = coordinator_proxy_->CreateLayer();
  const DisplayId kDisplayId(1);
  const std::vector<LayerId> kBothLayers = {image_layer_id, color_layer_id};

  const RotateFlip kTransform = RotateFlip::kIdentity();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;
  const ImageId kImageId(1);
  coordinator_proxy_->SetLayerPrimaryConfig(image_layer_id,
                                            types::Extent2({.width = 100, .height = 100}),
                                            /*image_tiling_type=*/0);
  coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kDestFrame);
  coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, kBlendMode, kAlpha);
  coordinator_proxy_->SetLayerImage(image_layer_id, kImageId, kInvalidEventId);

  const Rectangle kColorDestFrame({.x = 0, .y = 0, .width = 50, .height = 50});
  const WireColor kWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {10, 20, 30, 40},
  };
  coordinator_proxy_->SetLayerColorConfig(color_layer_id, kWireColor, kColorDestFrame);

  coordinator_proxy_->SetDisplayLayers(kDisplayId, kBothLayers);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  recorder.calls.clear();

  // Change both layers, and remove the image layer from the display list.  Only the color layer's
  // diff is sent.
  const Rectangle kNewDestFrame({.x = 0, .y = 0, .width = 300, .height = 300});
  const float kNewAlpha = 0.25f;
  coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kNewDestFrame);
  coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, kBlendMode, kNewAlpha);

  const WireColor kNewWireColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {50, 60, 70, 80},
  };
  coordinator_proxy_->SetLayerColorConfig(color_layer_id, kNewWireColor, kColorDestFrame);

  coordinator_proxy_->SetDisplayLayers(kDisplayId, std::span<const LayerId>(&color_layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls,
            (std::vector<std::string>{LayerCallRecorder::Call("color", color_layer_id)}));
  EXPECT_EQ(recorder.color.bytes, kNewWireColor.bytes);
  recorder.calls.clear();

  // Display the image layer again, with its position set back to the applied value but its alpha
  // left at the value set while it was not displayed.  The diff is against the state applied in
  // the first frame: the alpha is sent, the position and config are not.
  coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kDestFrame);
  coordinator_proxy_->SetDisplayLayers(kDisplayId, kBothLayers);
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 3}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls,
            (std::vector<std::string>{
                LayerCallRecorder::Call("alpha", image_layer_id),
                // TODO(https://fxbug.dev/449807074): the unchanged image is always re-sent.
                LayerCallRecorder::Call("image", image_layer_id),
            }));
  EXPECT_EQ(recorder.alpha_value, kNewAlpha);
  EXPECT_EQ(mock_coordinator_->set_display_layers_count(), 3u);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test that a failed `ApplyConfig()` resets only the displayed layers' drafts.  An undisplayed
// layer keeps a draft that was set once, as `DisplayCompositor` does for its empty-scene layer, and
// sends it when the layer is later displayed.
TEST_F(CoordinatorProxyTest, FailedApplyConfigKeepsUndisplayedLayerDraft) {
  LayerCallRecorder recorder;
  recorder.Attach(*mock_coordinator_);

  const LayerId displayed_layer_id = coordinator_proxy_->CreateLayer();
  const LayerId undisplayed_layer_id = coordinator_proxy_->CreateLayer();
  const DisplayId kDisplayId(1);

  const WireColor kUndisplayedColor = {
      .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
      .bytes = {0, 0, 0, 255},
  };
  coordinator_proxy_->SetLayerColorConfig(undisplayed_layer_id, kUndisplayedColor,
                                          Rectangle({.x = 0, .y = 0, .width = 50, .height = 50}));

  // A frame that displays only the other layer fails `CheckConfig()`.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kInvalidConfig; });
  coordinator_proxy_->SetLayerColorConfig(
      displayed_layer_id,
      {.format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8, .bytes = {10, 20, 30, 40}},
      Rectangle({.x = 0, .y = 0, .width = 100, .height = 100}));
  coordinator_proxy_->SetDisplayLayers(kDisplayId,
                                       std::span<const LayerId>(&displayed_layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 1}).status_value(),
            ZX_ERR_BAD_STATE);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls,
            (std::vector<std::string>{LayerCallRecorder::Call("color", displayed_layer_id)}));
  recorder.calls.clear();

  // Displaying the undisplayed layer, with no further `Set*()` call for it, sends its draft.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kOk; });
  coordinator_proxy_->SetDisplayLayers(kDisplayId,
                                       std::span<const LayerId>(&undisplayed_layer_id, 1));
  EXPECT_EQ(coordinator_proxy_->ApplyConfig(WireConfigStamp{.value = 2}).status_value(), ZX_OK);
  PingMockDisplayCoordinator();
  EXPECT_EQ(recorder.calls,
            (std::vector<std::string>{LayerCallRecorder::Call("color", undisplayed_layer_id)}));
  EXPECT_EQ(recorder.color.bytes, kUndisplayedColor.bytes);
  EXPECT_EQ(mock_coordinator_->illegal_action_count(), 0u);
}

// Test the three states of the CheckConfig cache:
// 1. Not cached, success: CheckConfig() is called, returns success, and ApplyConfig() is called.
// 2. Cached, success: CheckConfig() is not called, but ApplyConfig() is.
// 3. Not cached, failure: CheckConfig() is called, returns failure, ApplyConfig() is not called,
//    and DiscardConfig() is.
// 4. Cached, failure: No FIDL calls are made, and an error is returned.
TEST_F(CoordinatorProxyTest, ApplyConfig_CheckConfigCache) {
  // Create a couple of layers to use.
  const std::array<LayerId, 2> layer_ids = {coordinator_proxy_->CreateLayer(),
                                            coordinator_proxy_->CreateLayer()};
  const LayerId& color_layer_id = layer_ids[0];
  const LayerId& image_layer_id = layer_ids[1];

  const DisplayId kDisplayID(1);

  // Set up color layer.
  {
    const std::array<float, 4> kColorBgra = {0.1f, 0.2f, 0.3f, 0.4f};
    const WireColor kWireColor = {
        .format = fuchsia_images2::wire::PixelFormat::kB8G8R8A8,
        .bytes =
            {
                static_cast<uint8_t>(kColorBgra[0] * 255.f),  // Blue
                static_cast<uint8_t>(kColorBgra[1] * 255.f),  // Green
                static_cast<uint8_t>(kColorBgra[2] * 255.f),  // Red
                static_cast<uint8_t>(kColorBgra[3] * 255.f),  // Alpha
            },
    };

    const Rectangle kDestFrame({.x = 10, .y = 20, .width = 100, .height = 200});
    coordinator_proxy_->SetLayerColorConfig(color_layer_id, kWireColor, kDestFrame);
  }

  // Set up image layer.
  {
    const types::Extent2 kImageExtent({.width = 100, .height = 100});
    const uint32_t kImageTilingType = 0;
    coordinator_proxy_->SetLayerPrimaryConfig(image_layer_id, kImageExtent, kImageTilingType);

    const RotateFlip kTransform = RotateFlip::kReflectY();
    const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
    const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
    coordinator_proxy_->SetLayerPrimaryPosition(image_layer_id, kTransform, kSrcFrame, kDestFrame);

    const BlendMode kBlendMode = BlendMode::kStraightAlpha();
    const float kAlpha = 0.5f;
    coordinator_proxy_->SetLayerPrimaryAlpha(image_layer_id, kBlendMode, kAlpha);

    const ImageId kImageId(1);
    const EventId kWaitEventId(1);
    coordinator_proxy_->SetLayerImage(image_layer_id, kImageId, kWaitEventId);
  }

  // Verify count of API calls made to the proxy.  None have been over FIDL yet.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 2 CreateLayer +
            // 1 SetLayerColorConfig +
            // 3 SetLayerPrimaryConfig/Position/Alpha +
            // 1 SetLayerImage
            7U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // only layer creation occurs immediately
            2U);

  // The mock coordinator doesn't care whether the stamp is strictly increasing.
  const WireConfigStamp kConfigStamp{.value = 1};

  // We'll pretend that only 1 layer is allowed, so either the color layer or image layer alone is
  // OK, but both together means that `CheckConfig()` fails.

  // The first two single layer configs will succeed.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kOk; });

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&color_layer_id, 1});
  auto result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 1U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 1U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 1U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 1U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 0U);

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&image_layer_id, 1});
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 2U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 2U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 2U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 2U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 0U);

  // Verify count of API calls made to the proxy.  All have been sent over FIDL.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 7 (previous value) +
            // 2 SetDisplayLayers
            9U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // The image layer's diffs were held until it was displayed, so they are sent with the
            // second config, and there is no extra `SetLayerImage` yet.
            9U);

  // The two-layer config will fail.
  mock_coordinator_->set_check_config_fn(
      [](WireConfigResult* out) { *out = WireConfigResult::kInvalidConfig; });

  coordinator_proxy_->SetDisplayLayers(kDisplayID, layer_ids);
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 3U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 2U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 0U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  // Verify count of API calls made to the proxy.  All have been sent over FIDL.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 9 (previous value) +
            // 1 SetDisplayLayers
            10U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // TODO(https://fxbug.dev/449807074): +1 extra call to `SetLayerImage`.
            10U + 1U);

  // Go through the same 3 configs.  We should get the same results as before, but without needing
  // to call `CheckConfig()`.
  mock_coordinator_->set_check_config_fn([](WireConfigResult* out) { ASSERT_TRUE(false); });

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&color_layer_id, 1});
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 4U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 3U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 1U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  coordinator_proxy_->SetDisplayLayers(kDisplayID, {&image_layer_id, 1});
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_ok());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 5U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 4U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 2U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  coordinator_proxy_->SetDisplayLayers(kDisplayID, layer_ids);
  result = coordinator_proxy_->ApplyConfig(kConfigStamp);
  EXPECT_TRUE(result.is_error());
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_received(), 6U);
  EXPECT_EQ(coordinator_proxy_->apply_config_calls_sent(), 4U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_skipped(), 3U);
  EXPECT_EQ(coordinator_proxy_->check_config_calls_sent(), 3U);
  PingMockDisplayCoordinator();
  EXPECT_EQ(mock_coordinator_->check_config_count(), 3U);
  EXPECT_EQ(mock_coordinator_->discard_config_count(), 1U);

  // Verify count of API calls made to the proxy.  All have been sent over FIDL.
  EXPECT_EQ(coordinator_proxy_->api_calls_received(),
            // 10 (previous value) +
            // 3 SetDisplayLayers
            13U);
  EXPECT_EQ(coordinator_proxy_->api_calls_sent(),
            // 10 (previous value) +
            // 2 SetDisplayLayers
            // NOTE: the third SetDisplayLayers is not sent because the proxy was able to determine
            //       that CheckConfig would fail without calling SetDisplayLayers, and then needing
            //       to subsequently call DiscardConfig to clean up afterward.
            // TODO(https://fxbug.dev/449807074): +1 previous +1 new extra call to `SetLayerImage`;
            // the color-only config sends none.
            12U + 2U);
}

// Helper for CheckConfigHeuristics test.  Sets up the display, and a simple config with one
// non-fullscreen layer, which triggers the "only fullscreen layers allowed" heuristic.
static void CheckConfigHeuristicsTestSetup(CoordinatorProxy& proxy) {
  const DisplayId display_id(1);
  const DisplayMode display_mode({.active_area = types::Extent2({.width = 1920, .height = 1080}),
                                  .refresh_rate_millihertz = 60000,
                                  .mode_flags = 0});
  proxy.SetDisplayMode(display_id, display_mode);

  const LayerId layer_id = proxy.CreateLayer();
  const ImageId kImageId(1);
  const types::Extent2 kImageExtent({.width = 100, .height = 100});
  const uint32_t kImageTilingType = 0;
  proxy.SetLayerPrimaryConfig(layer_id, kImageExtent, kImageTilingType);

  const RotateFlip kTransform = RotateFlip::kReflectY();
  const Rectangle kSrcFrame({.x = 0, .y = 0, .width = 100, .height = 100});
  const Rectangle kDestFrame({.x = 0, .y = 0, .width = 200, .height = 200});
  proxy.SetLayerPrimaryPosition(layer_id, kTransform, kSrcFrame, kDestFrame);

  const BlendMode kBlendMode = BlendMode::kStraightAlpha();
  const float kAlpha = 0.5f;
  proxy.SetLayerPrimaryAlpha(layer_id, kBlendMode, kAlpha);

  const EventId kWaitEventId(1);
  proxy.SetLayerImage(layer_id, kImageId, kWaitEventId);

  proxy.SetDisplayLayers(display_id, std::vector{layer_id});
}

// Test that enabling "check config heuristics" gives the expected results.  In particular, they
// should cause `ApplyConfig()` to fail without checking the `check_config_cache_`.
TEST_F(CoordinatorProxyTest, CheckConfigHeuristics) {
  const WireConfigStamp config_stamp{.value = 1};

  // Heuristics disabled, so `ApplyConfig()` succeeds.
  {
    auto [mock, proxy] =
        MakeMockDisplayCoordinatorAndCoordinatorProxy({.enable_heuristics = false});
    CheckConfigHeuristicsTestSetup(*proxy);
    EXPECT_EQ(proxy->ApplyConfig(config_stamp).status_value(), ZX_OK);

    // Drain all calls before destroying mock/proxy.
    PingMockDisplayCoordinator(proxy.get());
  }

  // Heuristics enabled, so `ApplyConfig()` fails.
  {
    auto [mock, proxy] = MakeMockDisplayCoordinatorAndCoordinatorProxy({.enable_heuristics = true});
    CheckConfigHeuristicsTestSetup(*proxy);
    EXPECT_EQ(proxy->ApplyConfig(config_stamp).status_value(), ZX_ERR_BAD_STATE);

    // Drain all calls before destroying mock/proxy.
    PingMockDisplayCoordinator(proxy.get());
  }
}

}  // namespace display::test
