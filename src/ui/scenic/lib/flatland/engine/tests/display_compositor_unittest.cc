// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/engine/display_compositor.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <fidl/fuchsia.hardware.display/cpp/fidl.h>
#include <fidl/fuchsia.math/cpp/fidl.h>
#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fidl/fuchsia.ui.views/cpp/fidl.h>
#include <lib/async-loop/cpp/loop.h>
#include <lib/async/cpp/executor.h>
#include <lib/async/cpp/task.h>
#include <lib/async/default.h>
#include <lib/fpromise/bridge.h>
#include <lib/fpromise/single_threaded_executor.h>
#include <lib/inspect/cpp/hierarchy.h>
#include <lib/inspect/cpp/inspect.h>
#include <lib/inspect/cpp/reader.h>
#include <lib/sync/cpp/completion.h>
#include <lib/zx/time.h>

#include <array>
#include <cstdint>
#include <memory>
#include <span>
#include <thread>
#include <vector>

#include <gmock/gmock.h>

#include "src/lib/testing/loop_fixture/real_loop_fixture.h"
#include "src/ui/scenic/lib/allocation/buffer_collection_importer.h"
#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/display/util.h"
#include "src/ui/scenic/lib/flatland/engine/engine.h"
#include "src/ui/scenic/lib/flatland/engine/tests/mock_display_coordinator.h"
#include "src/ui/scenic/lib/flatland/flatland_display.h"
#include "src/ui/scenic/lib/flatland/flatland_presenter_impl.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/link_system.h"
#include "src/ui/scenic/lib/flatland/renderer/mock_renderer.h"
#include "src/ui/scenic/lib/flatland/uber_struct_system.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/helpers.h"
#include "src/ui/scenic/tests/utils/promise.h"

#include <glm/gtx/matrix_transform_2d.hpp>

using ::testing::_;
using ::testing::Eq;
using ::testing::Return;

using allocation::BufferCollectionUsage;
using allocation::ImageMetadata;
using flatland::MockDisplayCoordinator;
using flatland::SrcToDest;

using fuchsia_ui_composition::ImageFlip;
using fuchsia_ui_composition::Orientation;
using integration_tests::ReturnPromise;

namespace fuchsia_hardware_display::wire {

bool operator==(const BufferCollectionId& a, const BufferCollectionId& b) {
  return a.value == b.value;
}

bool operator==(const ImageId& a, const ImageId& b) { return a.value == b.value; }

bool operator==(const LayerId& a, const LayerId& b) { return a.value == b.value; }

}  // namespace fuchsia_hardware_display::wire

namespace fuchsia_hardware_display_types::wire {

bool operator==(const DisplayId& a, const DisplayId& b) { return a.value == b.value; }

}  // namespace fuchsia_hardware_display_types::wire

namespace fuchsia_math::wire {

bool operator==(const RectU& a, const RectU& b) {
  return a.x == b.x && a.y == b.y && a.width == b.width && a.height == b.height;
}

}  // namespace fuchsia_math::wire

namespace flatland::test {

namespace {

constexpr uint32_t kMaxDisplayLayersCount = 2;

// `AddDisplay()` creates the empty-scene layer before the pool layers, and layer ids
// start at 1.
constexpr display::WireLayerId kEmptySceneLayer{.value = 1};

// Returns a matcher matching the `field` from [`fuchsia.hardware.display/Coordinator.FunctionName`]
// FIDL request.
#define MatchRequestField(FunctionName, field, matcher)                                          \
  testing::Field(&fidl::WireRequest<fuchsia_hardware_display::Coordinator::FunctionName>::field, \
                 (matcher))

fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> DuplicateToken(
    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>& token) {
  fidl::Arena arena;
  uint32_t mask = ZX_RIGHT_SAME_RIGHTS;
  auto result = fidl::WireCall(token)->DuplicateSync(
      fuchsia_sysmem2::wire::BufferCollectionTokenDuplicateSyncRequest::Builder(arena)
          .rights_attenuation_masks(fidl::VectorView<uint32_t>::FromExternal(&mask, 1))
          .Build());
  FX_CHECK(result.ok());
  FX_CHECK(result->tokens().size() == 1u);
  return std::move(result->tokens()[0]);
}

void SetConstraintsAndClose(fidl::WireClient<fuchsia_sysmem2::Allocator>& sysmem_allocator,
                            fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token,
                            fuchsia_sysmem2::BufferCollectionConstraints constraints) {
  auto [client_end, server_end] = fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(std::move(token))
          .buffer_collection_request(std::move(server_end))
          .Build());
  ASSERT_TRUE(result.ok());

  fidl::WireSyncClient<fuchsia_sysmem2::BufferCollection> collection(std::move(client_end));
  auto set_result = collection->SetConstraints(
      fuchsia_sysmem2::wire::BufferCollectionSetConstraintsRequest::Builder(arena)
          .constraints(fidl::ToWire(arena, std::move(constraints)))
          .Build());
  ASSERT_TRUE(set_result.ok());
  auto release_result = collection->Release();
  ASSERT_TRUE(release_result.ok());
}

bool RunWithTimeoutOrUntil(fit::function<bool()> condition, zx::duration timeout,
                           zx::duration step) {
  zx::duration wait_time = zx::msec(0);
  while (wait_time <= timeout) {
    if (condition())
      return true;
    zx::nanosleep(zx::deadline_after(step));
    wait_time += step;
  }

  return condition();
}

class FakeFrameScheduler : public scheduling::FrameScheduler {
 public:
  void SetRenderContinuously(bool render_continuously) override {}
  void ScheduleUpdateForSession(zx::time requested_presentation_time,
                                scheduling::SchedulingIdPair id_pair, bool squashable,
                                bool schedule_asap) override {}
  std::vector<scheduling::FuturePresentationInfo> GetFuturePresentationInfos(
      zx::duration requested_prediction_span) override {
    return {};
  }
  void RemoveSession(scheduling::SessionId session_id) override {}
};

}  // namespace

class DisplayCompositorTest : public gtest::RealLoopFixture {
 public:
  void SetUp() override {
    gtest::RealLoopFixture::SetUp();
    async_set_default_dispatcher(dispatcher());
    dispatcher_setter_.emplace(dispatcher(), dispatcher());

    sysmem_allocator_ = utils::CreateSysmemAllocatorClient(dispatcher(), "DisplayCompositorTest");

    renderer_ = std::make_shared<flatland::MockRenderer>();

    zx::result endpoints_result = fidl::CreateEndpoints<fuchsia_hardware_display::Coordinator>();
    FX_CHECK(endpoints_result.is_ok())
        << "Failed to create FIDL endpoints for the display coordinator: "
        << endpoints_result.status_string();
    auto [coordinator_client, coordinator_server] = std::move(endpoints_result).value();

    mock_display_coordinator_ =
        std::make_unique<testing::StrictMock<flatland::MockDisplayCoordinator>>();
    // The `fidl::Server` requires the binding and teardown to occur on the
    // same thread where the FIDL server runs.
    libsync::Completion completion;
    async::PostTask(
        display_coordinator_loop_.dispatcher(),
        [this, &completion, coordinator_server = std::move(coordinator_server)]() mutable {
          mock_display_coordinator_->Bind(std::move(coordinator_server),
                                          display_coordinator_loop_.dispatcher());
          completion.Signal();
        });
    display_coordinator_loop_.StartThread("display-coordinator-loop");
    completion.Wait();

    auto coordinator_proxy =
        std::make_shared<display::CoordinatorProxy>(std::move(coordinator_client), dispatcher());

    display_compositor_ = std::make_shared<flatland::DisplayCompositor>(
        dispatcher(), std::move(coordinator_proxy), renderer_,
        utils::CreateSysmemAllocatorClient(dispatcher(), "display_compositor_unittest"),
        flatland::DisplayCompositorConfig{}, inspector_.GetRoot().CreateChild("DisplayCompositor"));
  }

  void TearDown() override {
    renderer_.reset();
    display_compositor_.reset();

    // This is to make sure that the display coordinator loop has finished
    // handling all its pending tasks.
    RunLoopUntilIdle();
    libsync::Completion completion;
    async::PostTask(display_coordinator_loop_.dispatcher(), [this, &completion]() mutable {
      display_coordinator_loop_.RunUntilIdle();
      mock_display_coordinator_.reset();
      completion.Signal();
    });
    completion.Wait();
    display_coordinator_loop_.Quit();
    display_coordinator_loop_.JoinThreads();

    dispatcher_setter_.reset();
    gtest::RealLoopFixture::TearDown();
  }

  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> CreateToken() {
    auto [client_end, server_end] =
        fidl::Endpoints<fuchsia_sysmem2::BufferCollectionToken>::Create();
    fidl::Arena arena;
    fidl::OneWayStatus result = sysmem_allocator_->AllocateSharedCollection(
        fuchsia_sysmem2::wire::AllocatorAllocateSharedCollectionRequest::Builder(arena)
            .token_request(std::move(server_end))
            .Build());
    FX_DCHECK(result.ok());
    auto sync_result = fidl::WireCall(client_end)->Sync();
    FX_DCHECK(sync_result.ok());
    return std::move(client_end);
  }

  using DisplayBufferCollectionMetadata = DisplayCompositor::DisplayBufferCollectionMetadata;

  void SetDisplaySupported(allocation::GlobalBufferCollectionId id, bool is_supported,
                           std::optional<DisplayBufferCollectionMetadata> metadata = std::nullopt) {
    std::scoped_lock lock(display_compositor_->lock_);
    display_compositor_->buffer_collection_supports_display_[id] = is_supported;
    display_compositor_->buffer_collection_tiling_type_map_[id] =
        fuchsia_hardware_display_types::kImageTilingTypeLinear;
    if (metadata.has_value()) {
      display_compositor_->buffer_collection_metadata_map_[id] = *metadata;
    } else {
      display_compositor_->buffer_collection_metadata_map_.erase(id);
    }
  }

  void ForceRendererOnlyMode(bool force_renderer_only) {
    const_cast<bool&>(display_compositor_->config_.enable_direct_to_display) = !force_renderer_only;
  }

  void SendOnVsyncEvent(display::WireConfigStamp stamp) {
    display_compositor_->OnVsync(display::DisplayId(1), zx::time_monotonic(), stamp);
  }

  std::deque<DisplayCompositor::ApplyConfigInfo> GetPendingApplyConfigs() {
    return display_compositor_->pending_apply_configs_;
  }

  bool BufferCollectionSupportsDisplay(allocation::GlobalBufferCollectionId id) {
    std::scoped_lock lock(display_compositor_->lock_);
    return display_compositor_->IsDisplaySupported(id);
  }

  bool HasDisplaySupportEntry(allocation::GlobalBufferCollectionId id) {
    std::scoped_lock lock(display_compositor_->lock_);
    return display_compositor_->buffer_collection_supports_display_.contains(id);
  }

  bool TryDirectToDisplay(std::span<const RenderData> render_data_list) {
    std::scoped_lock lock(display_compositor_->lock_);
    return display_compositor_->TryDirectToDisplay(render_data_list, /* frame_number= */ 1,
                                                   /* trace_flow_id= */ 1);
  }

  // Populates `display_engine_data_map_[display_id]` with a synthetic render target and wait
  // event so `PerformGpuComposition()` can run without going through `AddDisplay()` sysmem
  // allocation.
  void AddSyntheticRenderTarget(display::DisplayId display_id, const ImageMetadata& render_target) {
    std::scoped_lock lock(display_compositor_->lock_);
    auto& display_engine_data = display_compositor_->display_engine_data_map_.at(display_id);
    display_engine_data.vmo_count = 1;
    display_engine_data.render_targets.push_back(render_target);
    display_compositor_->image_import_map_[render_target.identifier] =
        DisplayCompositor::ImageImportData{
            .metadata = render_target,
            .imported_to_display = true,
            .tiling_type = fuchsia_hardware_display_types::kImageTilingTypeLinear,
    };
    zx::event wait_event;
    FX_CHECK(zx::event::create(0, &wait_event) == ZX_OK);
    display_engine_data.frame_event_datas.push_back({
        .wait_id = display::EventId(1),
        .wait_event = std::move(wait_event),
    });
  }

  // Clears the synthetic render target added by `AddSyntheticRenderTarget()`. Call before test
  // teardown so cleanup does not treat the synthetic render target as a real imported image.
  void ClearSyntheticRenderTargets(display::DisplayId display_id) {
    std::scoped_lock lock(display_compositor_->lock_);
    auto& display_engine_data = display_compositor_->display_engine_data_map_.at(display_id);
    for (const auto& target : display_engine_data.render_targets) {
      display_compositor_->image_import_map_.erase(target.identifier);
    }
    display_engine_data.frame_event_datas.clear();
    display_engine_data.render_targets.clear();
    display_engine_data.vmo_count = 0;
  }

  // Imports `collection_id` as a display-supported client buffer collection and returns the
  // renderer token to keep it alive for the duration of the test.
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> ImportDisplaySupportedClientCollection(
      allocation::GlobalBufferCollectionId collection_id,
      std::optional<DisplayCompositor::DisplayBufferCollectionMetadata> metadata = std::nullopt) {
    const display::WireBufferCollectionId display_collection_id =
        display::ToDisplayFidlBufferCollectionId(collection_id);
    EXPECT_CALL(
        *mock_display_coordinator_,
        ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                 Eq(display_collection_id)),
                               _))
        .Times(1)
        .WillOnce(testing::Invoke(
            [](fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest*,
               MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
              completer.Reply(fit::ok());
            }));
    EXPECT_CALL(*mock_display_coordinator_,
                SetBufferCollectionConstraints(
                    MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                      Eq(display_collection_id)),
                    _))
        .Times(1)
        .WillOnce(testing::Invoke(
            [](fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
               MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
              completer.Reply(fit::ok());
            }));

    fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
    EXPECT_CALL(*renderer_, ImportBufferCollection(collection_id, _, _, _, _))
        .WillOnce([&token_ref](auto id, auto& client, auto token, auto usage, auto size) {
          token_ref = std::move(token);
          return fpromise::make_ok_promise();
        });
    EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
        collection_id, sysmem_allocator_, CreateToken(), BufferCollectionUsage::kClientImage,
        std::nullopt)));
    SetDisplaySupported(collection_id, true, metadata);
    return token_ref;
  }

  // Builds a full-image `ResolvedLayer` for `metadata`.
  static ResolvedLayer MakeImageLayer(const ImageMetadata& metadata) {
    return ResolvedLayer{
        .geometry = SrcToDest(types::RectangleF({.x = 0,
                                                 .y = 0,
                                                 .width = static_cast<float>(metadata.width),
                                                 .height = static_cast<float>(metadata.height)}),
                              types::RectangleF({.x = 0,
                                                 .y = 0,
                                                 .width = static_cast<float>(metadata.width),
                                                 .height = static_cast<float>(metadata.height)}),
                              types::RotateFlip::kIdentity()),
        .multiply_color = {1.f, 1.f, 1.f, 1.f},
        .blend_mode = BlendMode::kReplace(),
        .content =
            ResolvedLayer::ImageContent{
                .image_id = metadata.identifier,
                .width = metadata.width,
                .height = metadata.height,
            },
    };
  }

  // `AddDisplay()` with the mock expectations it needs. Layer ids are 1 (the
  // empty-scene layer) through `kMaxDisplayLayersCount + 1`.
  void AddDisplayWithExpectations(display::Display* display, const DisplayInfo& display_info) {
    next_layer_id_ = 1;
    EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
        .Times(kMaxDisplayLayersCount + 1)
        .WillRepeatedly(testing::Invoke(
            [this](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                       request,
                   MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
              EXPECT_EQ(request->layer_id.value, next_layer_id_++);
              completer.Reply(fit::ok());
            }));
    EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_));
    display_compositor_->AddDisplay(display, display_info, /*num_vmos*/ 0,
                                    /*out_buffer_collection*/ nullptr);
  }

  // Expectations for the `DisplayCompositor` destructor after `AddDisplayWithExpectations()`.
  void ExpectDisplayCleanup() {
    for (uint64_t i = 1; i <= kMaxDisplayLayersCount + 1; ++i) {
      EXPECT_CALL(
          *mock_display_coordinator_,
          DestroyLayer(
              MatchRequestField(DestroyLayer, layer_id, Eq(display::WireLayerId{.value = i})), _))
          .Times(1)
          .WillOnce(Return());
    }
    EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
  }

  struct ClientImageUsageCounts {
    uint64_t total_imported = 0;
    uint64_t active = 0;
    uint64_t direct_to_display_only = 0;
    uint64_t gpu_composition_total = 0;
    uint64_t gpu_composition_only = 0;
    uint64_t both_d2d_and_gpu = 0;
    uint64_t unused = 0;

    bool operator==(const ClientImageUsageCounts&) const = default;
  };

  ClientImageUsageCounts ReadClientImageUsageCounts() {
    auto result = fpromise::run_single_threaded(inspect::ReadFromInspector(inspector_));
    FX_CHECK(result.is_ok());
    const auto hierarchy = result.take_value();
    const auto* compositor_node = hierarchy.GetByPath({"DisplayCompositor"});
    FX_CHECK(compositor_node);
    const auto* usage_node = compositor_node->GetByPath({"Client image usage counts"});
    FX_CHECK(usage_node);

    auto get_uint = [&](const std::string& name) -> uint64_t {
      const auto* prop = usage_node->node().get_property<inspect::UintPropertyValue>(name);
      FX_CHECK(prop) << "Missing property: " << name;
      return prop->value();
    };

    return ClientImageUsageCounts{
        .total_imported = get_uint("Total imported"),
        .active = get_uint("Active"),
        .direct_to_display_only = get_uint("Direct to display only"),
        .gpu_composition_total = get_uint("GPU composition (total)"),
        .gpu_composition_only = get_uint("GPU composition only"),
        .both_d2d_and_gpu = get_uint("Both direct to display and GPU"),
        .unused = get_uint("Unused"),
    };
  }

  // One `SetDisplayPowerMode()` call on the mock, replying `reply`. Set this up right
  // before each `DisplayCompositor::SetDisplayPowerMode()` call.
  void ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode expected_mode,
                                 zx_status_t reply) {
    EXPECT_CALL(*mock_display_coordinator_, SetDisplayPowerMode(_, _))
        .Times(1)
        .WillOnce(testing::Invoke(
            [expected_mode, reply](
                fuchsia_hardware_display::wire::CoordinatorSetDisplayPowerModeRequest* request,
                MockDisplayCoordinator::SetDisplayPowerModeCompleter::Sync& completer) {
              EXPECT_EQ(request->power_mode, expected_mode);
              if (reply == ZX_OK) {
                completer.Reply(fit::ok());
              } else {
                completer.Reply(fit::error(reply));
              }
            }));
  }

 protected:
  uint64_t next_layer_id_ = 1;

  bool RunPromise(fpromise::promise<> promise) {
    return integration_tests::RunPromise(
        dispatcher(), [this] { RunLoopUntilIdle(); }, std::move(promise));
  }

  static constexpr fuchsia_images2::PixelFormat kPixelFormat =
      fuchsia_images2::PixelFormat::kB8G8R8A8;

  async::Loop display_coordinator_loop_{&kAsyncLoopConfigNeverAttachToThread};
  std::unique_ptr<flatland::MockDisplayCoordinator> mock_display_coordinator_;
  std::shared_ptr<flatland::MockRenderer> renderer_;
  inspect::Inspector inspector_;
  std::shared_ptr<flatland::DisplayCompositor> display_compositor_;

  // Only for use on the main thread. Establish a new connection when on the
  // `MockDisplayCoordinator` thread.
  fidl::WireClient<fuchsia_sysmem2::Allocator> sysmem_allocator_;

  std::optional<utils::ScopedThreadDispatcherSetter> dispatcher_setter_;
  FakeFrameScheduler fake_frame_scheduler_;

  void HardwareFrameCorrectnessWithRotationTester(
      Orientation orientation, ImageFlip image_flip, fuchsia_math::wire::RectU expected_dst,
      display::WireCoordinateTransformation expected_transform);
};

// TODO(https://fxbug.dev/324688770): Dispatch all `DisplayCompositor` methods
// to the test loop.

TEST_F(DisplayCompositorTest, ImportAndReleaseBufferCollectionTest) {
  constexpr allocation::GlobalBufferCollectionId kGlobalBufferCollectionId = 15;
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest*,
             MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));

  EXPECT_CALL(*mock_display_coordinator_,
              SetBufferCollectionConstraints(
                  MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                    Eq(kDisplayBufferCollectionId)),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
             MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));

  // Save token to avoid early token failure.
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([&token_ref](allocation::GlobalBufferCollectionId,
                             fidl::WireClient<fuchsia_sysmem2::Allocator>&,
                             fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> renderer_token,
                             BufferCollectionUsage, std::optional<fuchsia_math::SizeU>) {
        token_ref = std::move(renderer_token);
        return fpromise::make_ok_promise();
      });
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_,
      fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken>{CreateToken()},
      BufferCollectionUsage::kClientImage, std::nullopt)));

  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(Return());

  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).WillOnce(Return());
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

// This test makes sure the buffer negotiations work as intended.
// There are three participants: the client, the display and the renderer.
// Each participant sets `{min_buffer_count, max_buffer_count}` constraints like so:
// Client: {1, 3}
// Display: {2, 3}
// Renderer: {1, 2}
// Since 2 is the only valid overlap between all of them we expect 2 buffers to be allocated.
TEST_F(DisplayCompositorTest,
       SysmemNegotiationTest_WhenDisplayConstraintsCompatible_TheyShouldBeIncluded) {
  // Create two tokens: one for acting as the "client" and inspecting allocation results with, and
  // one to send to the display compositor.
  auto client_token = CreateToken();
  auto compositor_token = DuplicateToken(client_token);

  // Set "client" constraints.
  auto [client_collection_client_end, client_collection_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator_->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(std::move(client_token))
          .buffer_collection_request(std::move(client_collection_server_end))
          .Build());
  ASSERT_TRUE(result.ok());
  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> client_collection(
      std::move(client_collection_client_end));

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  fuchsia_sysmem2::BufferUsage usage;
  usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
  constraints.usage(std::move(usage));
  constraints.min_buffer_count(1);
  constraints.max_buffer_count(3);
  fuchsia_sysmem2::BufferMemoryConstraints bmc;
  bmc.min_size_bytes(1);
  bmc.max_size_bytes(20);
  constraints.buffer_memory_constraints(std::move(bmc));
  fuchsia_sysmem2::ImageFormatConstraints ifc;
  ifc.pixel_format(fuchsia_images2::PixelFormat::kB8G8R8A8);
  ifc.color_spaces({{fuchsia_images2::ColorSpace::kSrgb}});
  ifc.min_size(fuchsia_math::SizeU(1, 1));
  constraints.image_format_constraints({{std::move(ifc)}});
  set_constraints_request.constraints(std::move(constraints));
  auto set_constraints_result =
      client_collection->SetConstraints(std::move(set_constraints_request));
  ASSERT_TRUE(set_constraints_result.is_ok());

  const auto kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> display_token;
  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_token](
              fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest* request,
              MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
            display_token = std::move(request->buffer_collection_token);
            completer.Reply(fit::ok());
          }));

  // Set display constraints.
  EXPECT_CALL(*mock_display_coordinator_,
              SetBufferCollectionConstraints(
                  MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                    Eq(kDisplayBufferCollectionId)),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [this, &display_token](
              fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
              MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
            fuchsia_sysmem2::BufferCollectionConstraints constraints;
            fuchsia_sysmem2::BufferUsage usage;
            usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
            constraints.usage(std::move(usage));
            constraints.min_buffer_count(2);
            constraints.max_buffer_count(3);

            async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
            auto sysmem_allocator =
                utils::CreateSysmemAllocatorClient(dispatcher(), "MockDisplayCoordinator");
            SetConstraintsAndClose(sysmem_allocator, std::move(display_token),
                                   std::move(constraints));
            loop.RunUntilIdle();
            completer.Reply(fit::ok());
          }));

  EXPECT_CALL(*mock_display_coordinator_,
              ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                           Eq(kDisplayBufferCollectionId)),
                                         MatchRequestField(ImportImage, buffer_index, Eq(0))),
                          _))
      .Times(0);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());

  // Set renderer constraints.
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([this](auto id, auto& client, auto renderer_token, auto usage, auto size) {
        fuchsia_sysmem2::BufferCollectionConstraints constraints;
        fuchsia_sysmem2::BufferUsage buf_usage;
        buf_usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
        constraints.usage(std::move(buf_usage));
        constraints.min_buffer_count(1);
        constraints.max_buffer_count(2);
        SetConstraintsAndClose(sysmem_allocator_, std::move(renderer_token),
                               std::move(constraints));
        return fpromise::make_ok_promise();
      });

  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, std::move(compositor_token),
      BufferCollectionUsage::kClientImage, std::nullopt)));

  {
    auto wait_result = client_collection->WaitForAllBuffersAllocated();
    ASSERT_TRUE(wait_result.is_ok());
    EXPECT_EQ(wait_result->buffer_collection_info().value().buffers().value().size(), 2u);
  }

  // `ImportBufferImage()` to confirm that the allocation was handled correctly.
  // Because display composition is enabled and the collection supports display, the actual
  // display and GPU image imports are deferred until the image is used.
  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).Times(0);
  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      ImageMetadata{.collection_id = kGlobalBufferCollectionId,
                    .identifier = display::ImageId(1),
                    .vmo_index = 0,
                    .width = 1,
                    .height = 1},
      BufferCollectionUsage::kClientImage)));
  EXPECT_TRUE(BufferCollectionSupportsDisplay(kGlobalBufferCollectionId));

  // `ImportBufferImage()` synchronously validates `vmo_index` against the sysmem-allocated buffer
  // count (2 buffers allocated above).
  EXPECT_FALSE(RunPromise(display_compositor_->ImportBufferImage(
      ImageMetadata{.collection_id = kGlobalBufferCollectionId,
                    .identifier = display::ImageId(2),
                    .vmo_index = 2,
                    .width = 1,
                    .height = 1},
      BufferCollectionUsage::kClientImage)));
}

// Verifies that if `ImportBufferImage()` is called before sysmem finishes allocating buffers for a
// collection (`CheckAllBuffersAllocated()` returns `Error::kPending`), the call fails without
// consuming the display token or caching `false` in `buffer_collection_supports_display_`, so a
// subsequent `ImportBufferImage()` after allocation completes still takes the lazy display path.
TEST_F(DisplayCompositorTest,
       SysmemNegotiationTest_ImportImageBeforeAllocationCompleteDoesNotCacheDisplayUnsupported) {
  auto client_token = CreateToken();
  auto compositor_token = DuplicateToken(client_token);

  const auto kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> display_token;
  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_token](
              fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest* request,
              MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
            display_token = std::move(request->buffer_collection_token);
            completer.Reply(fit::ok());
          }));

  EXPECT_CALL(*mock_display_coordinator_,
              SetBufferCollectionConstraints(
                  MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                    Eq(kDisplayBufferCollectionId)),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [this, &display_token](
              fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
              MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
            fuchsia_sysmem2::BufferCollectionConstraints constraints;
            fuchsia_sysmem2::BufferUsage usage;
            usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
            constraints.usage(std::move(usage));
            constraints.min_buffer_count(2);
            constraints.max_buffer_count(3);

            async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
            auto sysmem_allocator =
                utils::CreateSysmemAllocatorClient(dispatcher(), "MockDisplayCoordinator");
            SetConstraintsAndClose(sysmem_allocator, std::move(display_token),
                                   std::move(constraints));
            loop.RunUntilIdle();
            completer.Reply(fit::ok());
          }));

  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([this](auto id, auto& client, auto renderer_token, auto usage, auto size) {
        fuchsia_sysmem2::BufferCollectionConstraints constraints;
        fuchsia_sysmem2::BufferUsage buf_usage;
        buf_usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
        constraints.usage(std::move(buf_usage));
        constraints.min_buffer_count(1);
        constraints.max_buffer_count(2);
        SetConstraintsAndClose(sysmem_allocator_, std::move(renderer_token),
                               std::move(constraints));
        return fpromise::make_ok_promise();
      });

  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, std::move(compositor_token),
      BufferCollectionUsage::kClientImage, std::nullopt)));

  const ImageMetadata image_metadata{
      .collection_id = kGlobalBufferCollectionId,
      .identifier = display::ImageId(1),
      .vmo_index = 0,
      .width = 1,
      .height = 1,
  };

  // Client constraints have not been set yet, so buffers are not allocated (`Error::kPending`).
  // `ImportBufferImage()` must fail without calling `renderer_->ImportBufferImage()` or caching
  // `false` in `buffer_collection_supports_display_`.
  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).Times(0);
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  EXPECT_FALSE(HasDisplaySupportEntry(kGlobalBufferCollectionId));

  // Now bind `client_token`, set compatible client constraints, and wait for allocation.
  auto [client_collection_client_end, client_collection_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  fidl::Arena arena;
  fidl::OneWayStatus bind_result = sysmem_allocator_->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(std::move(client_token))
          .buffer_collection_request(std::move(client_collection_server_end))
          .Build());
  ASSERT_TRUE(bind_result.ok());
  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> client_collection(
      std::move(client_collection_client_end));

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  fuchsia_sysmem2::BufferUsage usage;
  usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
  constraints.usage(std::move(usage));
  constraints.min_buffer_count(1);
  constraints.max_buffer_count(3);
  fuchsia_sysmem2::BufferMemoryConstraints bmc;
  bmc.min_size_bytes(1);
  bmc.max_size_bytes(20);
  constraints.buffer_memory_constraints(std::move(bmc));
  fuchsia_sysmem2::ImageFormatConstraints ifc;
  ifc.pixel_format(fuchsia_images2::PixelFormat::kB8G8R8A8);
  ifc.color_spaces({{fuchsia_images2::ColorSpace::kSrgb}});
  ifc.min_size(fuchsia_math::SizeU(1, 1));
  constraints.image_format_constraints({{std::move(ifc)}});
  set_constraints_request.constraints(std::move(constraints));
  ASSERT_TRUE(client_collection->SetConstraints(std::move(set_constraints_request)).is_ok());
  ASSERT_TRUE(client_collection->WaitForAllBuffersAllocated().is_ok());

  // Retry `ImportBufferImage()` now that allocation has succeeded: it must take the lazy display
  // path without calling `renderer_->ImportBufferImage()`.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).Times(0);
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  EXPECT_TRUE(BufferCollectionSupportsDisplay(kGlobalBufferCollectionId));

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

// This test makes sure the buffer negotiations work as intended.
// There are three participants: the client, the display and the renderer.
// Each participant sets `{min_buffer_count, max_buffer_count}` constraints like so:
// Client: {1, 2}
// Display: {1, 1}
// Renderer: {2, 2}
// Since there is no valid overlap between all participants the display should drop out and we
// expect 2 buffers to be allocated (the only valid overlap between client and renderer).
TEST_F(DisplayCompositorTest,
       SysmemNegotiationTest_WhenDisplayConstraintsIncompatible_TheyShouldBeExcluded) {
  // Create two tokens: one for acting as the "client" and inspecting allocation results with, and
  // one to send to the display compositor.
  auto client_token = CreateToken();
  auto compositor_token = DuplicateToken(client_token);

  // Set "client" constraints.
  auto [client_collection_client_end, client_collection_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator_->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(std::move(client_token))
          .buffer_collection_request(std::move(client_collection_server_end))
          .Build());
  ASSERT_TRUE(result.ok());
  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> client_collection(
      std::move(client_collection_client_end));

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  fuchsia_sysmem2::BufferUsage usage;
  usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
  constraints.usage(std::move(usage));
  constraints.min_buffer_count(1);
  constraints.max_buffer_count(2);
  fuchsia_sysmem2::BufferMemoryConstraints bmc;
  bmc.min_size_bytes(1);
  bmc.max_size_bytes(20);
  constraints.buffer_memory_constraints(std::move(bmc));
  fuchsia_sysmem2::ImageFormatConstraints ifc;
  ifc.pixel_format(fuchsia_images2::PixelFormat::kB8G8R8A8);
  ifc.color_spaces({{fuchsia_images2::ColorSpace::kSrgb}});
  ifc.min_size(fuchsia_math::SizeU(1, 1));
  constraints.image_format_constraints({{std::move(ifc)}});
  set_constraints_request.constraints(std::move(constraints));
  auto set_constraints_result =
      client_collection->SetConstraints(std::move(set_constraints_request));
  ASSERT_TRUE(set_constraints_result.is_ok());

  const auto kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> display_token;
  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_token](
              fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest* request,
              MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
            display_token = std::move(request->buffer_collection_token);
            completer.Reply(fit::ok());
          }));

  // Set display constraints.
  EXPECT_CALL(*mock_display_coordinator_,
              SetBufferCollectionConstraints(
                  MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                    Eq(kDisplayBufferCollectionId)),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [this, &display_token](
              fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
              MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
            fuchsia_sysmem2::BufferCollectionConstraints constraints;
            fuchsia_sysmem2::BufferUsage usage;
            usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
            constraints.usage(std::move(usage));
            constraints.min_buffer_count(1);
            constraints.max_buffer_count(1);

            async::Loop loop(&kAsyncLoopConfigAttachToCurrentThread);
            auto sysmem_allocator =
                utils::CreateSysmemAllocatorClient(dispatcher(), "MockDisplayCoordinator");
            SetConstraintsAndClose(sysmem_allocator, std::move(display_token),
                                   std::move(constraints));
            loop.RunUntilIdle();
            completer.Reply(fit::ok());
          }));

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());

  // Set renderer constraints.
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([this](auto id, auto& client, auto renderer_token, auto usage, auto size) {
        fuchsia_sysmem2::BufferCollectionConstraints constraints;
        fuchsia_sysmem2::BufferUsage buf_usage;
        buf_usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
        constraints.usage(std::move(buf_usage));
        constraints.min_buffer_count(2);
        constraints.max_buffer_count(2);
        SetConstraintsAndClose(sysmem_allocator_, std::move(renderer_token),
                               std::move(constraints));
        return fpromise::make_ok_promise();
      });

  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, std::move(compositor_token),
      BufferCollectionUsage::kClientImage, std::nullopt)));

  {
    auto wait_result = client_collection->WaitForAllBuffersAllocated();
    ASSERT_TRUE(wait_result.is_ok());
    EXPECT_EQ(wait_result->buffer_collection_info().value().buffers().value().size(), 2u);
  }

  // `ImportBufferImage()` to confirm that the allocation was handled correctly.
  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).WillOnce(ReturnPromise(fpromise::ok()));
  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      ImageMetadata{.collection_id = kGlobalBufferCollectionId,
                    .identifier = display::ImageId(1),
                    .vmo_index = 0,
                    .width = 1,
                    .height = 1},
      BufferCollectionUsage::kClientImage)));
  EXPECT_FALSE(BufferCollectionSupportsDisplay(kGlobalBufferCollectionId));
}

TEST_F(DisplayCompositorTest, SysmemNegotiationTest_InRendererOnlyMode_DisplayShouldExcludeItself) {
  ForceRendererOnlyMode(true);

  // Create two tokens: one for acting as the "client" and inspecting allocation results with, and
  // one to send to the display compositor.
  auto client_token = CreateToken();
  auto compositor_token = DuplicateToken(client_token);

  // Set "client" constraints.
  auto [client_collection_client_end, client_collection_server_end] =
      fidl::Endpoints<fuchsia_sysmem2::BufferCollection>::Create();
  fidl::Arena arena;
  fidl::OneWayStatus result = sysmem_allocator_->BindSharedCollection(
      fuchsia_sysmem2::wire::AllocatorBindSharedCollectionRequest::Builder(arena)
          .token(std::move(client_token))
          .buffer_collection_request(std::move(client_collection_server_end))
          .Build());
  ASSERT_TRUE(result.ok());
  fidl::SyncClient<fuchsia_sysmem2::BufferCollection> client_collection(
      std::move(client_collection_client_end));

  fuchsia_sysmem2::BufferCollectionSetConstraintsRequest set_constraints_request;
  fuchsia_sysmem2::BufferCollectionConstraints constraints;
  fuchsia_sysmem2::BufferUsage usage;
  usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
  constraints.usage(std::move(usage));
  fuchsia_sysmem2::BufferMemoryConstraints bmc;
  bmc.min_size_bytes(1);
  bmc.max_size_bytes(20);
  constraints.buffer_memory_constraints(std::move(bmc));
  fuchsia_sysmem2::ImageFormatConstraints ifc;
  ifc.pixel_format(fuchsia_images2::PixelFormat::kB8G8R8A8);
  ifc.color_spaces({{fuchsia_images2::ColorSpace::kSrgb}});
  ifc.min_size(fuchsia_math::SizeU(1, 1));
  constraints.image_format_constraints({{std::move(ifc)}});
  set_constraints_request.constraints(std::move(constraints));
  auto set_constraints_result =
      client_collection->SetConstraints(std::move(set_constraints_request));
  ASSERT_TRUE(set_constraints_result.is_ok());

  const auto kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());

  // Set renderer constraints.
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([this](auto id, auto& client, auto renderer_token, auto usage, auto size) {
        fuchsia_sysmem2::BufferCollectionConstraints constraints;
        fuchsia_sysmem2::BufferUsage buf_usage;
        buf_usage.cpu(fuchsia_sysmem2::kCpuUsageWrite);
        constraints.usage(std::move(buf_usage));
        constraints.min_buffer_count(2);
        constraints.max_buffer_count(2);
        SetConstraintsAndClose(sysmem_allocator_, std::move(renderer_token),
                               std::move(constraints));
        return fpromise::make_ok_promise();
      });

  // Import `BufferCollection` and image to trigger constraint setting and handling of allocations.
  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, std::move(compositor_token),
      BufferCollectionUsage::kClientImage, std::nullopt)));

  {
    auto wait_result = client_collection->WaitForAllBuffersAllocated();
    ASSERT_TRUE(wait_result.is_ok());
    EXPECT_EQ(wait_result->buffer_collection_info().value().buffers().value().size(), 2u);
  }

  // `ImportBufferImage()` to confirm that the allocation was handled correctly.
  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).WillOnce(ReturnPromise(fpromise::ok()));
  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      ImageMetadata{.collection_id = kGlobalBufferCollectionId,
                    .identifier = display::ImageId(1),
                    .vmo_index = 0,
                    .width = 1,
                    .height = 1},
      BufferCollectionUsage::kClientImage)));
  EXPECT_FALSE(BufferCollectionSupportsDisplay(kGlobalBufferCollectionId));
}

TEST_F(DisplayCompositorTest, ClientDropSysmemToken) {
  const auto kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> dup_token;
  // Let client drop token.
  {
    auto token = CreateToken();
    fidl::SyncClient<fuchsia_sysmem2::BufferCollectionToken> sync_token(std::move(token));
    fuchsia_sysmem2::BufferCollectionTokenDuplicateSyncRequest dup_request;
    dup_request.rights_attenuation_masks({{ZX_RIGHT_SAME_RIGHTS}});
    auto dup_result = sync_token->DuplicateSync(std::move(dup_request));
    ASSERT_TRUE(dup_result.is_ok());
    ASSERT_TRUE(dup_result->tokens().has_value());
    ASSERT_EQ(dup_result->tokens()->size(), 1u);

    dup_token = std::move(dup_result->tokens()->at(0));
  }

  EXPECT_TRUE(RunWithTimeoutOrUntil(
      [&] {
        zx_status_t status = fidl::WireCall(dup_token)->Sync().status();
        return (status != ZX_OK || status == ZX_ERR_PEER_CLOSED);
      },
      zx::duration::infinite(), zx::msec(50)));

  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _)).Times(0);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());

  EXPECT_CALL(*mock_display_coordinator_, ImportBufferCollection(_, _)).Times(0);

  EXPECT_FALSE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, std::move(dup_token),
      BufferCollectionUsage::kClientImage, std::nullopt)));
}

TEST_F(DisplayCompositorTest, ImageIsValidAfterReleaseBufferCollection) {
  const auto kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  // Import image. With lazy import enabled for display-supported collections, neither
  // `ImportImage()` nor `renderer_->ImportBufferImage()` is called during `ImportBufferImage()`.
  ImageMetadata image_metadata = ImageMetadata{
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  testing::Mock::VerifyAndClearExpectations(mock_display_coordinator_.get());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Release buffer collection. `ReleaseBufferCollection()` must not synchronously import the image
  // or release the collection while lazy imports are still pending for `image_metadata`.
  const display::WireImageId kFidlImageId = image_metadata.identifier.ToFidl();
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);
  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(kFidlImageId)), _))
      .Times(0);
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);
  testing::Mock::VerifyAndClearExpectations(mock_display_coordinator_.get());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // The image remains valid after `ReleaseBufferCollection()`: realizing its GPU import succeeds
  // and immediately releases the collection from the GPU renderer now that no GPU imports remain.
  ResolvedLayer layer = MakeImageLayer(image_metadata);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer, 1));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Releasing the image drops its unexecuted display import and releases the collection from the
  // display coordinator without ever importing the image on the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(kFidlImageId)), _))
      .Times(0);
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, ImportImageErrorCases) {
  const allocation::GlobalBufferCollectionId kGlobalBufferCollectionId =
      allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  const allocation::GlobalImageId kImageId = allocation::GenerateUniqueImageId();
  const display::WireImageId kFidlImageId = kImageId.ToFidl();
  const uint32_t kVmoCount = 2;
  const uint32_t kVmoIdx = 1;
  const uint32_t kMaxWidth = 100;
  const uint32_t kMaxHeight = 200;
  const DisplayBufferCollectionMetadata kCollectionMetadata = {
      .vmo_count = kVmoCount,
      .min_size = {1, 1},
      .max_size = {kMaxWidth, kMaxHeight},
  };
  auto token_ref =
      ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId, kCollectionMetadata);

  ImageMetadata metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = kImageId,
      .vmo_index = kVmoIdx,
      .width = 20,
      .height = 30,
  };

  // With lazy import enabled for display-supported collections, `ImportBufferImage()` defers
  // importing until the image is rendered. Releasing it before rendering should not invoke
  // `ReleaseImage()` or `ReleaseBufferImage()`.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(metadata, _)).Times(0);

  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(metadata, BufferCollectionUsage::kClientImage)));

  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(kFidlImageId)), _))
      .Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(metadata.identifier)).Times(0);

  display_compositor_->ReleaseBufferImage(metadata.identifier);

  // Calling `ReleaseBufferImage()` again on an already-released or unknown image is a no-op.
  display_compositor_->ReleaseBufferImage(metadata.identifier);

  // Out-of-bounds `vmo_index` (`>= kVmoCount`) on a display-supported collection must fail
  // synchronously in `ImportBufferImage()` without calling the display coordinator or renderer.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).Times(0);
  auto copy_metadata = metadata;
  copy_metadata.vmo_index = kVmoCount;
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(copy_metadata, BufferCollectionUsage::kClientImage)));

  // Out-of-bounds dimensions (`> kMaxWidth` or `> kMaxHeight`) on a display-supported collection
  // must fail synchronously in `ImportBufferImage()`.
  copy_metadata = metadata;
  copy_metadata.width = kMaxWidth + 1;
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(copy_metadata, BufferCollectionUsage::kClientImage)));
  copy_metadata = metadata;
  copy_metadata.height = kMaxHeight + 1;
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(copy_metadata, BufferCollectionUsage::kClientImage)));

  // When the collection does not support display, `ImportBufferImage()` imports on the GPU
  // immediately and propagates any GPU import error.
  SetDisplaySupported(kGlobalBufferCollectionId, false);
  EXPECT_CALL(*renderer_, ImportBufferImage(metadata, _))
      .WillOnce(ReturnPromise(fpromise::error()));

  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(metadata, BufferCollectionUsage::kClientImage)));
  SetDisplaySupported(kGlobalBufferCollectionId, true, kCollectionMetadata);

  // Collection ID can't be invalid. This shouldn't reach the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_,
              ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                           Eq(kDisplayBufferCollectionId)),
                                         MatchRequestField(ImportImage, buffer_index, Eq(kVmoIdx))),
                          _))
      .Times(0);
  copy_metadata = metadata;
  copy_metadata.collection_id = allocation::kInvalidId;
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(copy_metadata, BufferCollectionUsage::kClientImage)));

  // Image Id can't be 0. This shouldn't reach the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_,
              ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                           Eq(kDisplayBufferCollectionId)),
                                         MatchRequestField(ImportImage, buffer_index, Eq(kVmoIdx))),
                          _))
      .Times(0);
  copy_metadata = metadata;
  copy_metadata.identifier = display::kInvalidImageId;
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(copy_metadata, BufferCollectionUsage::kClientImage)));

  // Width can't be 0. This shouldn't reach the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_,
              ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                           Eq(kDisplayBufferCollectionId)),
                                         MatchRequestField(ImportImage, buffer_index, Eq(kVmoIdx))),
                          _))
      .Times(0);
  copy_metadata = metadata;
  copy_metadata.width = 0;
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(copy_metadata, BufferCollectionUsage::kClientImage)));

  // Height can't be 0. This shouldn't reach the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_,
              ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                           Eq(kDisplayBufferCollectionId)),
                                         MatchRequestField(ImportImage, buffer_index, Eq(kVmoIdx))),
                          _))
      .Times(0);
  copy_metadata = metadata;
  copy_metadata.height = 0;
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(copy_metadata, BufferCollectionUsage::kClientImage)));

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

// This test checks that `DisplayCompositor` properly processes `ConfigStamp` from Vsync.
TEST_F(DisplayCompositorTest, VsyncConfigStampAreProcessed) {
  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillRepeatedly(
          testing::Invoke([&](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
            completer.Reply(display::WireConfigResult::kOk);
          }));

  static constexpr display::WireConfigStamp kConfigStamp1(1);
  static constexpr display::WireConfigStamp kConfigStamp2(2);
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(2).WillRepeatedly(Return());
  display_compositor_->RenderFrame(1, zx::time_monotonic(1), std::span<const RenderData>(), {}, {},
                                   {}, [](const scheduling::Timestamps&) {});
  display_compositor_->RenderFrame(2, zx::time_monotonic(2), std::span<const RenderData>(), {}, {},
                                   {}, [](const scheduling::Timestamps&) {});

  EXPECT_EQ(2u, GetPendingApplyConfigs().size());

  // Sending another vsync should be skipped.
  static constexpr display::WireConfigStamp kConfigStamp3(3);
  SendOnVsyncEvent({kConfigStamp3});
  EXPECT_EQ(2u, GetPendingApplyConfigs().size());

  // Sending later vsync should signal and remove the earlier one too.
  SendOnVsyncEvent(kConfigStamp2);
  EXPECT_EQ(0u, GetPendingApplyConfigs().size());
}

// When compositing directly to a hardware display layer, the display coordinator
// takes in source and destination `Frame` object types, which mirrors flatland usage.
// The source frames are nonnormalized UV coordinates and the destination frames are
// screenspace coordinates given in pixels. So this test makes sure that the rectangle
// and frame data that is generated by flatland sends along to the display coordinator
// the proper source and destination frame data. Each source and destination frame pair
// should be added to its own layer on the display.
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessTest) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  // Add an image.
  ImageMetadata parent_image_metadata = ImageMetadata{
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };

  // Add an image.
  ImageMetadata child_image_metadata = ImageMetadata{
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 1,
      .width = 512,
      .height = 1024,
  };

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);

  // We will end up with 2 source frames, 2 destination frames, and two layers being sent to the
  // display.
  const fuchsia_math::wire::RectU kExpectedSources[2] = {
      {.x = 0u, .y = 0u, .width = 512, .height = 1024u},
      {.x = 0u, .y = 0u, .width = 128u, .height = 256u},
  };

  const fuchsia_math::wire::RectU kExpectedDestinations[2] = {
      {.x = 5u, .y = 7u, .width = 30, .height = 40u},
      {.x = 9u, .y = 13u, .width = 10u, .height = 20u},
  };

  auto MakeRect = [](const fuchsia_math::wire::RectU& src, const fuchsia_math::wire::RectU& dst) {
    return SrcToDest(types::RectangleF({.x = static_cast<float>(src.x),
                                        .y = static_cast<float>(src.y),
                                        .width = static_cast<float>(src.width),
                                        .height = static_cast<float>(src.height)}),
                     types::RectangleF({.x = static_cast<float>(dst.x),
                                        .y = static_cast<float>(dst.y),
                                        .width = static_cast<float>(dst.width),
                                        .height = static_cast<float>(dst.height)}),
                     types::RotateFlip::kIdentity());
  };

  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest* request,
             MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*mock_display_coordinator_,
              SetBufferCollectionConstraints(
                  MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                    Eq(kDisplayBufferCollectionId)),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
             MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));
  // Save token to avoid early token failure.
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([&token_ref](auto id, auto& client, auto token, auto usage, auto size) {
        token_ref = std::move(token);
        return fpromise::make_ok_promise();
      });
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, CreateToken(),
      BufferCollectionUsage::kClientImage, std::nullopt)));
  SetDisplaySupported(kGlobalBufferCollectionId, true);

  const display::WireImageId fidl_parent_image_id = parent_image_metadata.identifier.ToFidl();
  // `ImportImage()` on the display coordinator fires lazily during `RenderFrame()` below.
  EXPECT_CALL(*mock_display_coordinator_,
              ImportImage(testing::AllOf(
                              MatchRequestField(ImportImage, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              MatchRequestField(ImportImage, buffer_index, Eq(0)),
                              MatchRequestField(ImportImage, image_id, Eq(fidl_parent_image_id))),
                          _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));
  EXPECT_CALL(*renderer_, ImportBufferImage(parent_image_metadata, _)).Times(0);

  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      parent_image_metadata, BufferCollectionUsage::kClientImage)));

  const display::WireImageId fidl_child_image_id = child_image_metadata.identifier.ToFidl();

  // `ImportImage()` on the display coordinator fires lazily during `RenderFrame()` below.
  EXPECT_CALL(
      *mock_display_coordinator_,
      ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                   Eq(kDisplayBufferCollectionId)),
                                 MatchRequestField(ImportImage, buffer_index, Eq(1)),
                                 MatchRequestField(ImportImage, image_id, Eq(fidl_child_image_id))),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));

  EXPECT_CALL(*renderer_, ImportBufferImage(child_image_metadata, _)).Times(0);
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      child_image_metadata, BufferCollectionUsage::kClientImage)));

  EXPECT_CALL(*renderer_, SetColorConversionValues(_, _, _)).WillOnce(Return());
  display_compositor_->SetColorConversionValues({1, 0, 0, 0, 1, 0, 0, 0, 1}, {0.1f, 0.2f, 0.3f},
                                                {-0.3f, -0.2f, -0.1f});

  // Setup the `EXPECT_CALL`s for gmock.
  uint64_t layer_id_value = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(3)
      .WillRepeatedly(testing::Invoke(
          [&](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                  request,
              MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, layer_id_value++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());

  std::vector<display::WireLayerId> layers = {{.value = 2}, {.value = 3}};

  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(
              MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
              MatchRequestField(SetDisplayLayers, layer_ids, testing::ElementsAreArray(layers))),
          _))
      .Times(1)
      .WillOnce(Return());

  // Make sure each layer has all of its components set properly.
  display::WireImageId fidl_image_ids[] = {child_image_metadata.identifier.ToFidl(),
                                           parent_image_metadata.identifier.ToFidl()};
  for (uint32_t i = 0; i < 2; i++) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        SetLayerPrimaryConfig(MatchRequestField(SetLayerPrimaryConfig, layer_id, Eq(layers[i])), _))
        .Times(1)
        .WillOnce(Return());

    EXPECT_CALL(
        *mock_display_coordinator_,
        SetLayerPrimaryPosition(
            testing::AllOf(MatchRequestField(SetLayerPrimaryPosition, layer_id, Eq(layers[i])),
                           MatchRequestField(SetLayerPrimaryPosition, image_source_transformation,
                                             Eq(display::WireCoordinateTransformation::kIdentity))),
            _))
        .Times(1)
        .WillOnce(testing::Invoke(
            [kExpectedSources, kExpectedDestinations, index = i](
                fuchsia_hardware_display::wire::CoordinatorSetLayerPrimaryPositionRequest* request,
                MockDisplayCoordinator::SetLayerPrimaryPositionCompleter::Sync& completer) {
              EXPECT_EQ(request->image_source, kExpectedSources[index]);
              EXPECT_EQ(request->display_destination, kExpectedDestinations[index]);
            }));

    EXPECT_CALL(
        *mock_display_coordinator_,
        SetLayerPrimaryAlpha(MatchRequestField(SetLayerPrimaryAlpha, layer_id, Eq(layers[i])), _))
        .Times(1)
        .WillOnce(Return());

    EXPECT_CALL(
        *mock_display_coordinator_,
        SetLayerImage2(
            testing::AllOf(MatchRequestField(SetLayerImage2, layer_id, Eq(layers[i])),
                           MatchRequestField(SetLayerImage2, image_id, Eq(fidl_image_ids[i]))),
            _))
        .Times(1)
        .WillOnce(Return());
  }

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1);
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayColorConversion(_, _)).Times(1);
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1);

  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([&](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));

  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_));

  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  display_compositor_->AddDisplay(&display, display_info, /*num_vmos*/ 0,
                                  /*out_buffer_collection*/ nullptr);

  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  ResolvedLayer child_layer = {
      .geometry = MakeRect(kExpectedSources[0], kExpectedDestinations[0]),
      .multiply_color = {1.f, 1.f, 1.f, 1.f},
      .blend_mode = BlendMode::kReplace(),
      .content = ResolvedLayer::ImageContent{.image_id = child_image_metadata.identifier,
                                             .width = child_image_metadata.width,
                                             .height = child_image_metadata.height},
  };
  ResolvedLayer parent_layer = {
      .geometry = MakeRect(kExpectedSources[1], kExpectedDestinations[1]),
      .multiply_color = {1.f, 1.f, 1.f, 1.f},
      .blend_mode = BlendMode::kReplace(),
      .content = ResolvedLayer::ImageContent{.image_id = parent_image_metadata.identifier,
                                             .width = parent_image_metadata.width,
                                             .height = parent_image_metadata.height},
  };
  std::array resolved_layers = {child_layer, parent_layer};
  RenderData render_data = {.display_id = kDisplayId, .layers = resolved_layers};
  std::span<const RenderData> render_data_list(&render_data, 1);

  display_compositor_->RenderFrame(1, zx::time_monotonic(1), render_data_list, {}, {}, {},
                                   [](const scheduling::Timestamps&) {});

  // Cleanup: All layers should be destroyed.
  for (uint64_t i = 1; i < layer_id_value; ++i) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        DestroyLayer(
            MatchRequestField(DestroyLayer, layer_id, Eq(display::WireLayerId{.value = i})), _))
        .Times(1)
        .WillOnce(Return());
  }
}

// The rotation and flip tests below share the following setup:
// the layer draws a 128×256 image (full sample region) into a 10×20 destination rect
// at the origin; each test below varies only the rotation/flip.
void DisplayCompositorTest::HardwareFrameCorrectnessWithRotationTester(
    Orientation orientation, ImageFlip image_flip, const fuchsia_math::wire::RectU expected_dst,
    display::WireCoordinateTransformation expected_transform) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  // Add an image.
  ImageMetadata parent_image_metadata = ImageMetadata{
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);

  // We will end up with 1 source frame, 1 destination frame, and one layer being sent to the
  // display.
  const fuchsia_math::wire::RectU kExpectedSource = {
      .x = 0u, .y = 0u, .width = 128u, .height = 256u};

  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest* request,
             MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*mock_display_coordinator_,
              SetBufferCollectionConstraints(
                  MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                    Eq(kDisplayBufferCollectionId)),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
             MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));
  // Save token to avoid early token failure.
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([&token_ref](auto id, auto& client, auto token, auto usage, auto size) {
        token_ref = std::move(token);
        return fpromise::make_ok_promise();
      });
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, CreateToken(),
      BufferCollectionUsage::kClientImage, std::nullopt)));
  SetDisplaySupported(kGlobalBufferCollectionId, true);

  const display::WireImageId fidl_parent_image_id = parent_image_metadata.identifier.ToFidl();
  // `ImportImage()` on the display coordinator fires lazily during `RenderFrame()` below.
  EXPECT_CALL(*mock_display_coordinator_,
              ImportImage(testing::AllOf(
                              MatchRequestField(ImportImage, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              MatchRequestField(ImportImage, buffer_index, Eq(0)),
                              MatchRequestField(ImportImage, image_id, Eq(fidl_parent_image_id))),
                          _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));

  EXPECT_CALL(*renderer_, ImportBufferImage(parent_image_metadata, _)).Times(0);

  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      parent_image_metadata, BufferCollectionUsage::kClientImage)));

  EXPECT_CALL(*renderer_, SetColorConversionValues(_, _, _)).WillOnce(Return());

  display_compositor_->SetColorConversionValues({1, 0, 0, 0, 1, 0, 0, 0, 1}, {0.1f, 0.2f, 0.3f},
                                                {-0.3f, -0.2f, -0.1f});

  // Setup the `EXPECT_CALL`s for gmock.
  // Note that a couple of layers are created upfront for the display.
  uint64_t layer_id_value = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(3)
      .WillRepeatedly(testing::Invoke(
          [&](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                  request,
              MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, layer_id_value++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());

  // However, we only set one display layer for the image.
  const std::vector<fuchsia_hardware_display::wire::LayerId> layers = {{.value = 2}};
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(
              MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
              MatchRequestField(SetDisplayLayers, layer_ids, testing::ElementsAreArray(layers))),
          _))
      .Times(1)
      .WillOnce(Return());

  const display::WireImageId fidl_collection_image_id = parent_image_metadata.identifier.ToFidl();
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetLayerPrimaryConfig(MatchRequestField(SetLayerPrimaryConfig, layer_id, Eq(layers[0])), _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetLayerPrimaryPosition(
          testing::AllOf(MatchRequestField(SetLayerPrimaryPosition, layer_id, Eq(layers[0])),
                         MatchRequestField(SetLayerPrimaryPosition, image_source_transformation,
                                           Eq(expected_transform))),
          _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [kExpectedSource, expected_dst](
              fuchsia_hardware_display::wire::CoordinatorSetLayerPrimaryPositionRequest* request,
              MockDisplayCoordinator::SetLayerPrimaryPositionCompleter::Sync& completer) {
            EXPECT_EQ(request->image_source, kExpectedSource);
            EXPECT_EQ(request->display_destination, expected_dst);
          }));
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetLayerPrimaryAlpha(MatchRequestField(SetLayerPrimaryAlpha, layer_id, Eq(layers[0])), _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetLayerImage2(
          testing::AllOf(MatchRequestField(SetLayerImage2, layer_id, Eq(layers[0])),
                         MatchRequestField(SetLayerImage2, image_id, Eq(fidl_collection_image_id))),
          _))
      .Times(1)
      .WillOnce(Return());

  EXPECT_CALL(*mock_display_coordinator_, SetDisplayColorConversion(_, _)).Times(1);
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1);

  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([&](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));

  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_));

  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  display_compositor_->AddDisplay(&display, display_info, /*num_vmos*/ 0,
                                  /*out_buffer_collection*/ nullptr);

  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  ResolvedLayer layer = {
      .geometry =
          SrcToDest(types::RectangleF({.x = 0.f, .y = 0.f, .width = 128.f, .height = 256.f}),
                    types::RectangleF({.x = static_cast<float>(expected_dst.x),
                                       .y = static_cast<float>(expected_dst.y),
                                       .width = static_cast<float>(expected_dst.width),
                                       .height = static_cast<float>(expected_dst.height)}),
                    types::RotateFlip::From(expected_transform)),
      .multiply_color = {1.f, 1.f, 1.f, 1.f},
      .blend_mode = BlendMode::kReplace(),
      .content = ResolvedLayer::ImageContent{.image_id = parent_image_metadata.identifier,
                                             .width = parent_image_metadata.width,
                                             .height = parent_image_metadata.height},
  };
  RenderData render_data = {.display_id = kDisplayId,
                            .layers = std::span<const ResolvedLayer>(&layer, 1)};
  std::span<const RenderData> render_data_list(&render_data, 1);

  display_compositor_->RenderFrame(1, zx::time_monotonic(1), render_data_list, {}, {}, {},
                                   [](const scheduling::Timestamps&) {});

  for (uint64_t i = 1; i < layer_id_value; ++i) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        DestroyLayer(
            MatchRequestField(DestroyLayer, layer_id, Eq(display::WireLayerId{.value = i})), _))
        .Times(1)
        .WillOnce(Return());
  }

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1);
}

// With90DegreeRotationTest
//   90° CCW rotation swaps the destination's W/H: 10×20 -> 20×10 at the origin.
//   Display transform: rotate CCW 90°.
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWith90DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 20u, .height = 10u};
  HardwareFrameCorrectnessWithRotationTester(Orientation::kCcw90Degrees, ImageFlip::kNone,
                                             kExpectedDest,
                                             display::WireCoordinateTransformation::kRotateCcw90);
}

// With180DegreeRotationTest
//   180° rotation preserves W/H: destination stays 10×20 at the origin.
//   Display transform: rotate CCW 180°.
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWith180DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 10u, .height = 20u};
  HardwareFrameCorrectnessWithRotationTester(Orientation::kCcw180Degrees, ImageFlip::kNone,
                                             kExpectedDest,
                                             display::WireCoordinateTransformation::kRotateCcw180);
}

// With270DegreeRotationTest
//   270° CCW rotation swaps W/H: 10×20 -> 20×10 at the origin.
//   Display transform: rotate CCW 270°.
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWith270DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 20u, .height = 10u};
  HardwareFrameCorrectnessWithRotationTester(Orientation::kCcw270Degrees, ImageFlip::kNone,
                                             kExpectedDest,
                                             display::WireCoordinateTransformation::kRotateCcw270);
}

// WithLeftRightFlipTest
//   No rotation: destination stays 10×20.  A left-right (horizontal) mirror is a
//   reflection across the Y axis (`kReflectY`); the flip rides `ImageFlip`.
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithLeftRightFlipTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 10u, .height = 20u};
  HardwareFrameCorrectnessWithRotationTester(Orientation::kCcw0Degrees, ImageFlip::kLeftRight,
                                             kExpectedDest,
                                             display::WireCoordinateTransformation::kReflectY);
}

// WithUpDownFlipTest
//   No rotation: destination stays 10×20.  An up-down (vertical) mirror reflects
//   across the X axis (`kReflectX`); the flip rides `ImageFlip`.
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithUpDownFlipTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 10u, .height = 20u};
  HardwareFrameCorrectnessWithRotationTester(Orientation::kCcw0Degrees, ImageFlip::kUpDown,
                                             kExpectedDest,
                                             display::WireCoordinateTransformation::kReflectX);
}

// WithLeftRightFlip90DegreeRotationTest
//   Left-right flip combined with 90° CCW rotation; destination 10×20 -> 20×10.
//   Display transform `ROTATE_CCW_90_REFLECT_X`: "`ROTATE_CCW_90`, followed by
//   `REFLECT_X`" (`coordinator.fidl` / `fuchsia.hardware.display.types.CoordinateTransformation` :
//   the combined enums rotate first, then reflect).  Note the reflection is
//   `REFLECT_X` here, whereas a left-right flip *without* rotation is `REFLECT_Y`
//   (`kReflectY`): the display reflects in the post-rotation frame.
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithLeftRightFlip90DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 20u, .height = 10u};
  HardwareFrameCorrectnessWithRotationTester(
      Orientation::kCcw90Degrees, ImageFlip::kLeftRight, kExpectedDest,
      display::WireCoordinateTransformation::kRotateCcw90ReflectX);
}

// WithUpDownFlip90DegreeRotationTest
//   Up-down flip combined with 90° CCW rotation; destination 10×20 -> 20×10.
//   Display transform `ROTATE_CCW_90_REFLECT_Y`: "`ROTATE_CCW_90`, followed by
//   `REFLECT_Y`" (same rotate-then-reflect order, `coordinator.fidl`).
TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithUpDownFlip90DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 20u, .height = 10u};
  HardwareFrameCorrectnessWithRotationTester(
      Orientation::kCcw90Degrees, ImageFlip::kUpDown, kExpectedDest,
      display::WireCoordinateTransformation::kRotateCcw90ReflectY);
}

TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithLeftRightFlip180DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 10u, .height = 20u};
  HardwareFrameCorrectnessWithRotationTester(Orientation::kCcw180Degrees, ImageFlip::kLeftRight,
                                             kExpectedDest,
                                             display::WireCoordinateTransformation::kReflectX);
}

TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithUpDownFlip180DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 10u, .height = 20u};
  HardwareFrameCorrectnessWithRotationTester(Orientation::kCcw180Degrees, ImageFlip::kUpDown,
                                             kExpectedDest,
                                             display::WireCoordinateTransformation::kReflectY);
}

TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithLeftRightFlip270DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 20u, .height = 10u};
  HardwareFrameCorrectnessWithRotationTester(
      Orientation::kCcw270Degrees, ImageFlip::kLeftRight, kExpectedDest,
      display::WireCoordinateTransformation::kRotateCcw90ReflectY);
}

TEST_F(DisplayCompositorTest, HardwareFrameCorrectnessWithUpDownFlip270DegreeRotationTest) {
  const fuchsia_math::wire::RectU kExpectedDest = {.x = 0u, .y = 0u, .width = 20u, .height = 10u};
  HardwareFrameCorrectnessWithRotationTester(
      Orientation::kCcw270Degrees, ImageFlip::kUpDown, kExpectedDest,
      display::WireCoordinateTransformation::kRotateCcw90ReflectX);
}

// Tests that RenderOnly mode does not attempt to `ImportBufferCollection()` to display.
TEST_F(DisplayCompositorTest, RendererOnly_ImportAndReleaseBufferCollectionTest) {
  ForceRendererOnlyMode(true);

  const allocation::GlobalBufferCollectionId kGlobalBufferCollectionId = 15;
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(0);
  // Save token to avoid early token failure.
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([&token_ref](auto id, auto& client, auto token, auto usage, auto size) {
        token_ref = std::move(token);
        return fpromise::make_ok_promise();
      });
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, CreateToken(),
      BufferCollectionUsage::kClientImage, std::nullopt)));

  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(Return());

  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).WillOnce(Return());
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, SetDisplayLayers_WithNoImages_UsesEmptySceneLayer) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(testing::AnyNumber());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillRepeatedly(
          testing::Invoke([&](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
            completer.Reply(display::WireConfigResult::kOk);
          }));

  // Setup the `EXPECT_CALL`s for gmock.
  // We expect 1 layer for empty scene, and 2 layers for the pool (configured in `SetUp`).
  uint64_t layer_id_value = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(3)
      .WillRepeatedly(testing::Invoke(
          [&](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                  request,
              MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, layer_id_value++);
            completer.Reply(fit::ok());
          }));

  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());

  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  display_compositor_->AddDisplay(&display, display_info, /*num_vmos*/ 0,
                                  /*out_buffer_collection*/ nullptr);

  // We expect `SetDisplayLayers` to be called with the FIRST layer created (empty scene layer).
  std::vector<display::WireLayerId> expected_layers = {{.value = 1}};
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
                         MatchRequestField(SetDisplayLayers, layer_ids,
                                           testing::ElementsAreArray(expected_layers))),
          _))
      .Times(1)
      .WillOnce(Return());

  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  // `RenderFrame` with empty render data list for the display.
  // This triggers `SetRenderDataOnDisplay` with 0 images.
  RenderData render_data = {.display_id = kDisplayId, .layers = {}};
  std::span<const RenderData> render_data_list(&render_data, 1);
  display_compositor_->RenderFrame(1, zx::time_monotonic(1), render_data_list, {}, {}, {},
                                   [](const scheduling::Timestamps&) {});

  // Cleanup: All layers should be destroyed.
  for (uint64_t i = 1; i < layer_id_value; ++i) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        DestroyLayer(
            MatchRequestField(DestroyLayer, layer_id, Eq(display::WireLayerId{.value = i})), _))
        .Times(1)
        .WillOnce(Return());
  }
}

TEST_F(DisplayCompositorTest, TryDirectToDisplayExceedsHardwareLayerLimitFallbackToGpu) {
  static constexpr display::DisplayId kDisplayId(1);
  static constexpr glm::uvec2 resolution(1024, 768);
  static constexpr uint32_t kMaxDisplayLayersCount = 1;
  const DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};

  // Note: Scenic creates `display->max_layer_count()` layers for the pool PLUS
  // one additional layer for the empty scene. Total = `kMaxDisplayLayersCount + 1`.
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(
          testing::Invoke([&](auto, MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));

  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  display_compositor_->AddDisplay(&display, display_info, /* num_vmos= */ 0,
                                  /* out_buffer_collection= */ nullptr);

  auto root_image_id = allocation::GenerateUniqueImageId();
  auto child_image_id = allocation::GenerateUniqueImageId();

  ResolvedLayer layer1 = {
      .content = ResolvedLayer::ImageContent{.image_id = root_image_id},
  };
  ResolvedLayer layer2 = {
      .content = ResolvedLayer::ImageContent{.image_id = child_image_id},
  };
  std::array layers = {layer1, layer2};
  RenderData render_data = {.display_id = kDisplayId, .layers = layers};
  std::span<const RenderData> render_data_list(&render_data, 1);

  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(0);

  ASSERT_EQ(render_data_list[0].layers.size(), 2u);
  bool result = TryDirectToDisplay(render_data_list);
  EXPECT_FALSE(result);

  // Cleanup: All layers (`kMaxDisplayLayersCount` + 1 empty scene layer) should be destroyed.
  for (uint64_t i = 1; i <= kMaxDisplayLayersCount + 1; ++i) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        DestroyLayer(
            MatchRequestField(DestroyLayer, layer_id, Eq(display::WireLayerId{.value = i})), _))
        .Times(1)
        .WillOnce(Return());
  }
  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

struct SolidColorAlphaByteParam {
  BlendMode blend_mode;
  uint8_t expected_alpha;
};

class DisplayCompositorSolidColorAlphaTest
    : public DisplayCompositorTest,
      public testing::WithParamInterface<SolidColorAlphaByteParam> {};

// A `kReplace` fill is sent opaque, even if its color is translucent: the GPU path draws a
// `kReplace` layer opaque, and the fill must not change when Scenic switches between the paths. A
// premultiplied-alpha fill keeps its alpha, so the Coordinator blends it.
TEST_P(DisplayCompositorSolidColorAlphaTest, SolidColorAlphaByteFollowsBlendMode) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  uint64_t layer_id_value = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [&](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                  request,
              MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, layer_id_value++);
            completer.Reply(fit::ok());
          }));

  display_compositor_->AddDisplay(&display, display_info, /*num_vmos*/ 0,
                                  /*out_buffer_collection*/ nullptr);

  // Premultiplied half-opaque green, at layer opacity 1, in the blend mode under test. Red and
  // blue are equal, so the expected bytes do not depend on the channel order.
  ResolvedLayer layer = {
      .geometry = SrcToDest(types::RectangleF({.x = 0,
                                               .y = 0,
                                               .width = static_cast<float>(resolution.x),
                                               .height = static_cast<float>(resolution.y)})),
      .multiply_color = {1.f, 1.f, 1.f, 1.f},
      .blend_mode = GetParam().blend_mode,
      .content = ResolvedLayer::SolidColorContent{.color = {0.f, 0.5f, 0.f, 0.5f}},
  };
  RenderData render_data = {
      .display_id = kDisplayId,
      .layers = std::span<const ResolvedLayer>(&layer, 1),
  };

  // `AddDisplay()` also sends a color config, black, for the empty-scene layer.
  std::vector<std::array<uint8_t, 4>> sent_color_bytes;
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _))
      .Times(2)
      .WillRepeatedly(testing::Invoke(
          [&](fidl::WireServer<
                  fuchsia_hardware_display::Coordinator>::SetLayerColorConfigRequestView request,
              MockDisplayCoordinator::SetLayerColorConfigCompleter::Sync& completer) {
            const fidl::Array<uint8_t, 8>& bytes = request->color.bytes;
            sent_color_bytes.push_back({bytes[0], bytes[1], bytes[2], bytes[3]});
          }));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).Times(0);
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).Times(0);

  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  bool result = TryDirectToDisplay(std::span<const RenderData>(&render_data, 1));
  EXPECT_TRUE(result);

  EXPECT_THAT(sent_color_bytes,
              testing::Contains(testing::ElementsAre(uint8_t{0}, uint8_t{127}, uint8_t{0},
                                                     GetParam().expected_alpha)));

  // Cleanup: All layers should be destroyed.
  for (uint64_t i = 1; i <= kMaxDisplayLayersCount + 1; ++i) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        DestroyLayer(
            MatchRequestField(DestroyLayer, layer_id, Eq(display::WireLayerId{.value = i})), _))
        .Times(1)
        .WillOnce(Return());
  }
  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

INSTANTIATE_TEST_SUITE_P(
    BlendModes, DisplayCompositorSolidColorAlphaTest,
    testing::Values(SolidColorAlphaByteParam{.blend_mode = BlendMode::kReplace(),
                                             .expected_alpha = 255},
                    SolidColorAlphaByteParam{.blend_mode = BlendMode::kPremultipliedAlpha(),
                                             .expected_alpha = 127}));

TEST_F(DisplayCompositorTest, ImageContentTakesImageLayerPath) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  uint64_t layer_id_value = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [&](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                  request,
              MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, layer_id_value++);
            completer.Reply(fit::ok());
          }));

  display_compositor_->AddDisplay(&display, display_info, /*num_vmos*/ 0,
                                  /*out_buffer_collection*/ nullptr);

  // Import buffer collection and image.
  EXPECT_CALL(*mock_display_coordinator_,
              ImportBufferCollection(MatchRequestField(ImportBufferCollection, buffer_collection_id,
                                                       Eq(kDisplayBufferCollectionId)),
                                     _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorImportBufferCollectionRequest* request,
             MockDisplayCoordinator::ImportBufferCollectionCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*mock_display_coordinator_,
              SetBufferCollectionConstraints(
                  MatchRequestField(SetBufferCollectionConstraints, buffer_collection_id,
                                    Eq(kDisplayBufferCollectionId)),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [](fuchsia_hardware_display::wire::CoordinatorSetBufferCollectionConstraintsRequest*,
             MockDisplayCoordinator::SetBufferCollectionConstraintsCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));

  // Save token to avoid early token failure.
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([&token_ref](auto id, auto& client, auto token, auto usage, auto size) {
        token_ref = std::move(token);
        return fpromise::make_ok_promise();
      });
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, CreateToken(),
      BufferCollectionUsage::kClientImage, std::nullopt)));
  SetDisplaySupported(kGlobalBufferCollectionId, true);

  ImageMetadata image_metadata = ImageMetadata{
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };

  const display::WireImageId fidl_image_id = image_metadata.identifier.ToFidl();
  // `ImportImage()` on the display coordinator fires lazily during `TryDirectToDisplay()` below.
  EXPECT_CALL(
      *mock_display_coordinator_,
      ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                   Eq(kDisplayBufferCollectionId)),
                                 MatchRequestField(ImportImage, buffer_index, Eq(0)),
                                 MatchRequestField(ImportImage, image_id, Eq(fidl_image_id))),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);

  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  ResolvedLayer layer = {
      .geometry = SrcToDest(types::RectangleF({.x = 0, .y = 0, .width = 128.f, .height = 256.f}),
                            types::RectangleF({.x = 0, .y = 0, .width = 128.f, .height = 256.f}),
                            types::RotateFlip::kIdentity()),
      .multiply_color = {1.f, 1.f, 1.f, 1.f},
      .blend_mode = BlendMode::kReplace(),
      .content =
          ResolvedLayer::ImageContent{
              .image_id = image_metadata.identifier,
              .width = image_metadata.width,
              .height = image_metadata.height,
          },
  };
  RenderData render_data = {
      .display_id = kDisplayId,
      .layers = std::span<const ResolvedLayer>(&layer, 1),
  };

  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryPosition(_, _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryAlpha(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).Times(1).WillOnce(Return());

  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  bool result = TryDirectToDisplay(std::span<const RenderData>(&render_data, 1));
  EXPECT_TRUE(result);

  // Cleanup: All layers should be destroyed.
  for (uint64_t i = 1; i <= kMaxDisplayLayersCount + 1; ++i) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        DestroyLayer(
            MatchRequestField(DestroyLayer, layer_id, Eq(display::WireLayerId{.value = i})), _))
        .Times(1)
        .WillOnce(Return());
  }
  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, GoingDarkFlushesPendingConfigsAndStagesBlack) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  AddDisplayWithExpectations(&display, display_info);

  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(2)
      .WillRepeatedly(
          testing::Invoke([&](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
            completer.Reply(display::WireConfigResult::kOk);
          }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(3).WillRepeatedly(Return());

  std::vector<display::WireLayerId> expected_layers = {kEmptySceneLayer};
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
                         MatchRequestField(SetDisplayLayers, layer_ids,
                                           testing::ElementsAreArray(expected_layers))),
          _))
      .Times(1)
      .WillOnce(Return());

  bool frame_1_callback_fired = false;
  bool frame_2_callback_fired = false;
  zx::event release_fence = utils::CreateEvent();
  zx::event release_fence_copy;
  ASSERT_EQ(release_fence.duplicate(ZX_RIGHT_SAME_RIGHTS, &release_fence_copy), ZX_OK);

  std::vector<zx::event> frame_2_release_fences;
  frame_2_release_fences.push_back(std::move(release_fence));

  display_compositor_->RenderFrame(
      1, zx::time_monotonic(1), std::span<const RenderData>(), {}, {}, {},
      [&frame_1_callback_fired](const scheduling::Timestamps&) { frame_1_callback_fired = true; });

  display_compositor_->RenderFrame(
      2, zx::time_monotonic(2), std::span<const RenderData>(), std::move(frame_2_release_fences),
      {}, {},
      [&frame_2_callback_fired](const scheduling::Timestamps&) { frame_2_callback_fired = true; });

  EXPECT_EQ(GetPendingApplyConfigs().size(), 2u);

  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_OK);
  EXPECT_TRUE(display_compositor_->IsDisplayDark(kDisplayId));

  EXPECT_TRUE(utils::IsEventSignalled(release_fence_copy, ZX_EVENT_SIGNALED));
  EXPECT_TRUE(frame_1_callback_fired);
  EXPECT_TRUE(frame_2_callback_fired);
  EXPECT_EQ(GetPendingApplyConfigs().size(), 1u);
  EXPECT_EQ(GetPendingApplyConfigs().front().frame_number, 2u);

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, LateVsyncAfterGoingDarkIsIgnored) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  AddDisplayWithExpectations(&display, display_info);

  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(2)
      .WillRepeatedly(
          testing::Invoke([&](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
            completer.Reply(display::WireConfigResult::kOk);
          }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(3).WillRepeatedly(Return());

  std::vector<display::WireLayerId> expected_layers = {kEmptySceneLayer};
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
                         MatchRequestField(SetDisplayLayers, layer_ids,
                                           testing::ElementsAreArray(expected_layers))),
          _))
      .Times(1)
      .WillOnce(Return());

  bool frame_1_callback_fired = false;
  bool frame_2_callback_fired = false;
  zx::event release_fence = utils::CreateEvent();
  zx::event release_fence_copy;
  ASSERT_EQ(release_fence.duplicate(ZX_RIGHT_SAME_RIGHTS, &release_fence_copy), ZX_OK);

  std::vector<zx::event> frame_2_release_fences;
  frame_2_release_fences.push_back(std::move(release_fence));

  display_compositor_->RenderFrame(
      1, zx::time_monotonic(1), std::span<const RenderData>(), {}, {}, {},
      [&frame_1_callback_fired](const scheduling::Timestamps&) { frame_1_callback_fired = true; });

  display_compositor_->RenderFrame(
      2, zx::time_monotonic(2), std::span<const RenderData>(), std::move(frame_2_release_fences),
      {}, {},
      [&frame_2_callback_fired](const scheduling::Timestamps&) { frame_2_callback_fired = true; });

  EXPECT_EQ(GetPendingApplyConfigs().size(), 2u);

  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_OK);
  EXPECT_TRUE(display_compositor_->IsDisplayDark(kDisplayId));

  EXPECT_TRUE(utils::IsEventSignalled(release_fence_copy, ZX_EVENT_SIGNALED));
  EXPECT_TRUE(frame_1_callback_fired);
  EXPECT_TRUE(frame_2_callback_fired);
  EXPECT_EQ(GetPendingApplyConfigs().size(), 1u);
  EXPECT_EQ(GetPendingApplyConfigs().front().frame_number, 2u);

  static constexpr display::WireConfigStamp kConfigStamp1(1);
  SendOnVsyncEvent(kConfigStamp1);
  EXPECT_EQ(GetPendingApplyConfigs().size(), 1u);

  static constexpr display::WireConfigStamp kConfigStamp3(3);
  SendOnVsyncEvent(kConfigStamp3);
  EXPECT_EQ(GetPendingApplyConfigs().size(), 0u);

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, SetDisplayPowerModeWithoutDisplayIsNotFound) {
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                display::DisplayId(1), fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_ERR_NOT_FOUND);
  EXPECT_FALSE(display_compositor_->IsDisplayDark(display::DisplayId(1)));

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, SetDisplayPowerModeReportsCoordinatorError) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  AddDisplayWithExpectations(&display, display_info);

  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_ERR_NOT_SUPPORTED);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_ERR_NOT_SUPPORTED);
  EXPECT_FALSE(display_compositor_->IsDisplayDark(kDisplayId));
  EXPECT_TRUE(GetPendingApplyConfigs().empty());

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, SetDisplayPowerModeMapsCoordinatorErrors) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  AddDisplayWithExpectations(&display, display_info);

  // The coordinator lost the display before this compositor heard about it.
  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_ERR_NOT_FOUND);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_ERR_NOT_FOUND);

  // An error the coordinator does not document.
  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_ERR_BAD_STATE);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_ERR_INTERNAL);
  EXPECT_FALSE(display_compositor_->IsDisplayDark(kDisplayId));

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, DozeModesKeepDisplayingContent) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  AddDisplayWithExpectations(&display, display_info);

  // 1. `kDoze`
  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kDoze, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kDoze),
            ZX_OK);
  EXPECT_FALSE(display_compositor_->IsDisplayDark(kDisplayId));
  EXPECT_TRUE(GetPendingApplyConfigs().empty());

  // 2. `kOff`
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  std::vector<display::WireLayerId> expected_layers = {kEmptySceneLayer};
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
                         MatchRequestField(SetDisplayLayers, layer_ids,
                                           testing::ElementsAreArray(expected_layers))),
          _))
      .Times(1)
      .WillOnce(Return());

  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_OK);
  EXPECT_TRUE(display_compositor_->IsDisplayDark(kDisplayId));
  EXPECT_EQ(GetPendingApplyConfigs().size(), 1u);

  // 3. `kDozeSuspend`
  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kDozeSuspend, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kDozeSuspend),
            ZX_OK);
  EXPECT_FALSE(display_compositor_->IsDisplayDark(kDisplayId));
  EXPECT_EQ(GetPendingApplyConfigs().size(), 1u);

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, SetDisplayPowerModeForwardsToCoordinator) {
  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);

  AddDisplayWithExpectations(&display, display_info);

  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kDoze, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kDoze),
            ZX_OK);
  EXPECT_TRUE(GetPendingApplyConfigs().empty());

  ExpectDisplayCleanup();
}

// The first view tree is always generated; after that, with no session requesting a
// recomputation and no link change, `GenerateViewTreeSnapshot()` answers "no diff".
TEST_F(DisplayCompositorTest, ViewTreeSnapshotIsNoDiffWhenNothingChanged) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  auto first = engine.GenerateViewTreeSnapshot(flatland::TransformHandle(1, 1));
  EXPECT_TRUE(std::holds_alternative<std::unique_ptr<view_tree::SubtreeSnapshot>>(first));

  auto second = engine.GenerateViewTreeSnapshot(flatland::TransformHandle(1, 1));
  EXPECT_TRUE(std::holds_alternative<view_tree::SubtreeSnapshotNoDiff>(second));

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, RenderScheduledFrameWithoutDisplayLeavesViewTreeEmpty) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  bool callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/1, /*presentation_time=*/zx::time(1000), /*display=*/nullptr,
      [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
  EXPECT_TRUE(callback_called);

  auto snapshot_variant = engine.GenerateViewTreeSnapshot(flatland::TransformHandle(1, 1));
  EXPECT_TRUE(
      std::holds_alternative<std::unique_ptr<view_tree::SubtreeSnapshot>>(snapshot_variant));
  auto& snapshot = std::get<std::unique_ptr<view_tree::SubtreeSnapshot>>(snapshot_variant);
  ASSERT_NE(snapshot, nullptr);
  EXPECT_EQ(snapshot->root, ZX_KOID_INVALID);
  EXPECT_TRUE(snapshot->view_tree.empty());

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest,
       RenderScheduledFrameWhenDisplayIsDarkSkipsRenderAndPreservesViewTree) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  auto display =
      std::make_shared<display::Display>(display::WireDisplayId{.value = kDisplayId.value()},
                                         resolution.x, resolution.y, kMaxDisplayLayersCount);

  // Set up mock coordinator expectations for AddDisplay().
  next_layer_id_ = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [this](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                     request,
                 MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, next_layer_id_++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_))
      .WillRepeatedly(Return(kPixelFormat));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).WillRepeatedly(Return());

  engine.AddDisplay(*display, /*num_vmos=*/0);
  RunLoopUntilIdle();

  const auto session_id = scheduling::GetNextSessionId();
  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_ui_composition::FlatlandDisplay>::Create();
  auto uber_struct_queue = uber_struct_system->AllocateQueueForSession(session_id);
  auto flatland_display = FlatlandDisplay::New(
      std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()), std::move(server_end),
      session_id, display,
      /*destroy_display_function=*/[] {}, flatland_presenter, link_system, uber_struct_queue);

  // Create an UberStruct with a ViewRef and a layer with image content.
  auto uber_struct = std::make_unique<flatland::UberStruct>();
  zx::eventpair endpoint1, endpoint2;
  ASSERT_EQ(zx::eventpair::create(0, &endpoint1, &endpoint2), ZX_OK);
  auto view_ref = std::make_shared<const flatland::ViewRef>(std::move(endpoint1));
  const zx_koid_t expected_koid = view_ref->koid();
  uber_struct->view_ref = view_ref;
  uber_struct->local_topology = {{flatland_display->root_transform(), 0}};

  flatland::LayerHandle layer_handle(session_id, 1);
  uber_struct->layer_stacks[flatland_display->root_transform()] =
      std::pmr::vector<flatland::LayerHandle>({layer_handle}, uber_struct->resource());
  flatland::UberStructLayer layer;
  layer.common.display_rect = types::Rectangle::From(types::Point2({.x = 0, .y = 0}),
                                                     types::Extent2({.width = 100, .height = 100}));
  layer.content = flatland::UberStructLayer::ImageModeProperties{
      .sample_rect = types::RectangleF::From(types::Point2F({.x = 0.f, .y = 0.f}),
                                             types::Extent2F({.width = 100.f, .height = 100.f})),
      .image_id = allocation::GlobalImageId(1),
      .image_width = 100,
      .image_height = 100,
  };
  uber_struct->layers[layer_handle] = layer;

  uber_struct_queue->Push(/*present_id=*/1, std::move(uber_struct), /*recompute_view_tree=*/true);
  uber_struct_system->ForceUpdateAllSessions();

  // Transition display to dark mode (kOff).
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  std::vector<display::WireLayerId> expected_layers = {kEmptySceneLayer};
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
                         MatchRequestField(SetDisplayLayers, layer_ids,
                                           testing::ElementsAreArray(expected_layers))),
          _))
      .Times(1)
      .WillOnce(Return());

  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_OK);
  EXPECT_TRUE(display_compositor_->IsDisplayDark(kDisplayId));

  // RenderScheduledFrame() while display is dark.
  // This must NOT invoke RenderFrame() on DisplayCompositor (no unexpected mock calls),
  // but must invoke the frame presented callback and preserve the populated ViewTree.
  bool callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/1, /*presentation_time=*/zx::time(1000), flatland_display.get(),
      [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
  EXPECT_TRUE(callback_called);

  // Verify that the ViewTree snapshot was generated with the expected view.
  auto snapshot_variant = engine.GenerateViewTreeSnapshot(flatland_display->root_transform());
  EXPECT_TRUE(
      std::holds_alternative<std::unique_ptr<view_tree::SubtreeSnapshot>>(snapshot_variant));
  auto& snapshot = std::get<std::unique_ptr<view_tree::SubtreeSnapshot>>(snapshot_variant);
  ASSERT_NE(snapshot, nullptr);
  EXPECT_EQ(snapshot->root, expected_koid);
  EXPECT_TRUE(snapshot->view_tree.contains(expected_koid));

  flatland_display.reset();
  RunLoopUntilIdle();

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, RenderAfterSkippedDarkFrame) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  auto display =
      std::make_shared<display::Display>(display::WireDisplayId{.value = kDisplayId.value()},
                                         resolution.x, resolution.y, kMaxDisplayLayersCount);

  const auto flush_coordinator_loop = [this]() {
    libsync::Completion completion;
    async::PostTask(display_coordinator_loop_.dispatcher(),
                    [&completion]() { completion.Signal(); });
    completion.Wait();
  };

  next_layer_id_ = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [this](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                     request,
                 MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, next_layer_id_++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_))
      .WillRepeatedly(Return(kPixelFormat));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).WillRepeatedly(Return());

  // Register one-way coordinator expectations upfront so background FIDL dispatch on
  // `display_coordinator_loop_` cannot race mid-test `EXPECT_CALL` registration:
  //   - `SetDisplayLayers`: frames 1 and 3 set the direct-scanout layer; power-off sets
  //     `kEmptySceneLayer`.
  //   - `CheckConfig`: frame 1 and power-off check configs; frame 3 hits the config cache.
  //   - `CommitConfig`: frame 1, power-off, and frame 3 each commit once.
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(2).WillRepeatedly(Return());
  const std::vector<display::WireLayerId> expected_empty_layers = {kEmptySceneLayer};
  EXPECT_CALL(
      *mock_display_coordinator_,
      SetDisplayLayers(
          testing::AllOf(MatchRequestField(SetDisplayLayers, display_id, Eq(kDisplayId.ToFidl())),
                         MatchRequestField(SetDisplayLayers, layer_ids,
                                           testing::ElementsAreArray(expected_empty_layers))),
          _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(2)
      .WillRepeatedly(
          testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
            completer.Reply(display::WireConfigResult::kOk);
          }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(3).WillRepeatedly(Return());

  engine.AddDisplay(*display, /*num_vmos=*/0);
  RunLoopUntilIdle();

  const auto session_id = scheduling::GetNextSessionId();
  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_ui_composition::FlatlandDisplay>::Create();
  auto uber_struct_queue = uber_struct_system->AllocateQueueForSession(session_id);
  auto flatland_display = FlatlandDisplay::New(
      std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()), std::move(server_end),
      session_id, display,
      /*destroy_display_function=*/[] {}, flatland_presenter, link_system, uber_struct_queue);

  auto uber_struct = std::make_unique<flatland::UberStruct>();
  uber_struct->local_topology = {{flatland_display->root_transform(), 0}};
  flatland::LayerHandle layer_handle(session_id, 1);
  uber_struct->layer_stacks[flatland_display->root_transform()] =
      std::pmr::vector<flatland::LayerHandle>({layer_handle}, uber_struct->resource());
  flatland::UberStructLayer layer;
  layer.common.display_rect = types::Rectangle::From(types::Point2({.x = 0, .y = 0}),
                                                     types::Extent2({.width = 100, .height = 100}));
  layer.content = flatland::UberStructLayer::SolidColorModeProperties{
      .color = {1.f, 0.f, 0.f, 1.f},
  };
  uber_struct->layers[layer_handle] = layer;
  uber_struct_queue->Push(/*present_id=*/1, std::move(uber_struct), /*recompute_view_tree=*/true);
  uber_struct_system->ForceUpdateAllSessions();

  // Frame 1: direct scanout on an active display (config stamp 1).
  bool frame_1_callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/1, /*presentation_time=*/zx::time(1000), flatland_display.get(),
      [&frame_1_callback_called](const scheduling::Timestamps&) {
        frame_1_callback_called = true;
      });
  EXPECT_FALSE(frame_1_callback_called);

  // Power display off: commits the empty-scene config (config stamp 2) and flushes frame 1.
  flush_coordinator_loop();
  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOff, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOff),
            ZX_OK);
  EXPECT_TRUE(display_compositor_->IsDisplayDark(kDisplayId));
  EXPECT_TRUE(frame_1_callback_called);

  // Frame 2 while dark: skips rendering, forwards fences/callback through `DisplayCompositor`, and
  // signals both immediately because nothing of the client's is on screen.
  auto frame_2_fences = utils::CreateEventArray(1);
  flatland_presenter->ScheduleUpdateForSession(zx::time(0), {session_id, 2}, /*unsquashable=*/false,
                                               utils::CopyZxHandleVector(frame_2_fences), {}, {},
                                               /*schedule_asap=*/false);
  RunLoopUntilIdle();
  flatland_presenter->AccumulateFences({{session_id, 2}});

  bool frame_2_callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/2, /*presentation_time=*/zx::time(2000), flatland_display.get(),
      [&frame_2_callback_called](const scheduling::Timestamps&) {
        frame_2_callback_called = true;
      });
  EXPECT_TRUE(frame_2_callback_called);
  EXPECT_TRUE(utils::IsEventSignalled(frame_2_fences[0], ZX_EVENT_SIGNALED));

  // Power display back on.
  flush_coordinator_loop();
  ExpectSetDisplayPowerMode(fuchsia_hardware_display_types::PowerMode::kOn, ZX_OK);
  EXPECT_EQ(display_compositor_->SetDisplayPowerMode(
                kDisplayId, fuchsia_hardware_display_types::PowerMode::kOn),
            ZX_OK);
  EXPECT_FALSE(display_compositor_->IsDisplayDark(kDisplayId));

  // Frame 3 after power-on (config stamp 3): must succeed cleanly after the skipped dark frame,
  // signaling its release fence immediately and invoking its callback on VSync.
  auto frame_3_fences = utils::CreateEventArray(1);
  flatland_presenter->ScheduleUpdateForSession(zx::time(0), {session_id, 3}, /*unsquashable=*/false,
                                               utils::CopyZxHandleVector(frame_3_fences), {}, {},
                                               /*schedule_asap=*/false);
  RunLoopUntilIdle();
  flatland_presenter->AccumulateFences({{session_id, 3}});

  bool frame_3_callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/3, /*presentation_time=*/zx::time(3000), flatland_display.get(),
      [&frame_3_callback_called](const scheduling::Timestamps&) {
        frame_3_callback_called = true;
      });
  EXPECT_TRUE(utils::IsEventSignalled(frame_3_fences[0], ZX_EVENT_SIGNALED));
  EXPECT_FALSE(frame_3_callback_called);

  SendOnVsyncEvent(display::WireConfigStamp(3));
  EXPECT_TRUE(frame_3_callback_called);

  flatland_display.reset();
  RunLoopUntilIdle();
  flush_coordinator_loop();

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, RenderScheduledFrameWithoutDisplayClearsViewTree) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  auto display =
      std::make_shared<display::Display>(display::WireDisplayId{.value = kDisplayId.value()},
                                         resolution.x, resolution.y, kMaxDisplayLayersCount);

  // Set up mock coordinator expectations for AddDisplay().
  next_layer_id_ = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [this](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                     request,
                 MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, next_layer_id_++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_))
      .WillRepeatedly(Return(kPixelFormat));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).WillRepeatedly(Return());

  engine.AddDisplay(*display, /*num_vmos=*/0);
  RunLoopUntilIdle();

  const auto session_id = scheduling::GetNextSessionId();
  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_ui_composition::FlatlandDisplay>::Create();
  auto uber_struct_queue = uber_struct_system->AllocateQueueForSession(session_id);
  auto flatland_display = FlatlandDisplay::New(
      std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()), std::move(server_end),
      session_id, display,
      /*destroy_display_function=*/[] {}, flatland_presenter, link_system, uber_struct_queue);

  // Create an UberStruct with a ViewRef. It has no image layer, so the first frame prepares the
  // scene state and then skips instead of submitting a frame to the DisplayCompositor.
  auto uber_struct = std::make_unique<flatland::UberStruct>();
  zx::eventpair endpoint1, endpoint2;
  ASSERT_EQ(zx::eventpair::create(0, &endpoint1, &endpoint2), ZX_OK);
  auto view_ref = std::make_shared<const flatland::ViewRef>(std::move(endpoint1));
  const zx_koid_t expected_koid = view_ref->koid();
  uber_struct->view_ref = view_ref;
  uber_struct->local_topology = {{flatland_display->root_transform(), 0}};

  uber_struct_queue->Push(/*present_id=*/1, std::move(uber_struct), /*recompute_view_tree=*/true);
  uber_struct_system->ForceUpdateAllSessions();

  // A frame with the FlatlandDisplay populates the scene state; a following frame without one
  // must clear it.
  bool first_callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/1, /*presentation_time=*/zx::time(1000), flatland_display.get(),
      [&first_callback_called](const scheduling::Timestamps&) { first_callback_called = true; });
  EXPECT_TRUE(first_callback_called);

  // Generate the ViewTree snapshot for Frame 1, consuming `MustRecomputeViewTree()`.
  auto first_snapshot_variant = engine.GenerateViewTreeSnapshot(flatland_display->root_transform());
  ASSERT_TRUE(
      std::holds_alternative<std::unique_ptr<view_tree::SubtreeSnapshot>>(first_snapshot_variant));
  const auto& first_snapshot =
      std::get<std::unique_ptr<view_tree::SubtreeSnapshot>>(first_snapshot_variant);
  ASSERT_NE(first_snapshot, nullptr);
  EXPECT_EQ(first_snapshot->root, expected_koid);
  EXPECT_TRUE(first_snapshot->view_tree.contains(expected_koid));

  bool second_callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/2, /*presentation_time=*/zx::time(2000), /*display=*/nullptr,
      [&second_callback_called](const scheduling::Timestamps&) { second_callback_called = true; });
  EXPECT_TRUE(second_callback_called);

  // Frame 1 consumed `MustRecomputeViewTree()`, but `SceneState::Clear()` reset
  // `link_topology_generation` to 0, so the generator produces a snapshot of the cleared scene
  // state rather than `SubtreeSnapshotNoDiff`.
  auto snapshot_variant = engine.GenerateViewTreeSnapshot(flatland_display->root_transform());
  EXPECT_TRUE(
      std::holds_alternative<std::unique_ptr<view_tree::SubtreeSnapshot>>(snapshot_variant));
  auto& snapshot = std::get<std::unique_ptr<view_tree::SubtreeSnapshot>>(snapshot_variant);
  ASSERT_NE(snapshot, nullptr);
  EXPECT_EQ(snapshot->root, ZX_KOID_INVALID);
  EXPECT_TRUE(snapshot->view_tree.empty());

  flatland_display.reset();
  RunLoopUntilIdle();

  ExpectDisplayCleanup();
}

// `RenderScheduledFrame()` rebuilds the `SceneState` on the first frame, on a frame whose
// session requested it, and on a frame where a link changed, and reuses it on a frame where
// none of those happened. The scene has topology but no layers, so every frame prepares the
// `SceneState` and then skips rendering.
TEST_F(DisplayCompositorTest, LayerOnlyFrameReusesSceneState) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  auto display =
      std::make_shared<display::Display>(display::WireDisplayId{.value = kDisplayId.value()},
                                         resolution.x, resolution.y, kMaxDisplayLayersCount);

  // Set up mock coordinator expectations for AddDisplay().
  next_layer_id_ = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [this](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                     request,
                 MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, next_layer_id_++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_))
      .WillRepeatedly(Return(kPixelFormat));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).WillRepeatedly(Return());

  engine.AddDisplay(*display, /*num_vmos=*/0);
  RunLoopUntilIdle();

  const auto session_id = scheduling::GetNextSessionId();
  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_ui_composition::FlatlandDisplay>::Create();
  auto uber_struct_queue = uber_struct_system->AllocateQueueForSession(session_id);
  auto flatland_display = FlatlandDisplay::New(
      std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()), std::move(server_end),
      session_id, display,
      /*destroy_display_function=*/[] {}, flatland_presenter, link_system, uber_struct_queue);

  const flatland::TransformHandle root = flatland_display->root_transform();
  const flatland::TransformHandle child(root.GetInstanceId(), root.GetTransformId() + 1);

  // Frame 1: the session requests a rebuild, and the cache describes no frame yet.
  {
    auto uber_struct = std::make_unique<flatland::UberStruct>();
    uber_struct->local_topology = {{root, 0}};
    uber_struct_queue->Push(/*present_id=*/1, std::move(uber_struct),
                            /*recompute_view_tree=*/false, /*recompute_scene_state=*/true);
    uber_struct_system->ForceUpdateAllSessions();

    bool callback_called = false;
    engine.RenderScheduledFrame(
        /*frame_number=*/1, /*presentation_time=*/zx::time(1000), flatland_display.get(),
        [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
    EXPECT_TRUE(callback_called);
    EXPECT_EQ(engine.scene_state_for_test().rebuild_count, 1u);
    EXPECT_FALSE(engine.scene_state_for_test().cleared);
  }

  // Frame 2: the same topology without a rebuild request reuses the cached state; the reuse arm
  // still swaps in the new snapshot.
  {
    auto uber_struct = std::make_unique<flatland::UberStruct>();
    uber_struct->local_topology = {{root, 0}};
    const flatland::UberStruct* const frame2_uber_struct = uber_struct.get();
    uber_struct_queue->Push(/*present_id=*/2, std::move(uber_struct),
                            /*recompute_view_tree=*/false, /*recompute_scene_state=*/false);
    uber_struct_system->ForceUpdateAllSessions();

    bool callback_called = false;
    engine.RenderScheduledFrame(
        /*frame_number=*/2, /*presentation_time=*/zx::time(2000), flatland_display.get(),
        [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
    EXPECT_TRUE(callback_called);
    EXPECT_EQ(engine.scene_state_for_test().rebuild_count, 1u);
    const auto& snapshot_map = engine.scene_state_for_test().snapshot.map;
    ASSERT_TRUE(snapshot_map.contains(session_id));
    EXPECT_EQ(snapshot_map.at(session_id).get(), frame2_uber_struct);
  }

  // Frame 3: a transform-graph change with a rebuild request rebuilds the cached state.
  {
    auto uber_struct = std::make_unique<flatland::UberStruct>();
    uber_struct->local_topology = {{root, 1}, {child, 0}};
    uber_struct->local_matrices[child] = glm::translate(glm::mat3(1.f), {10.f, 20.f});
    uber_struct_queue->Push(/*present_id=*/3, std::move(uber_struct),
                            /*recompute_view_tree=*/false, /*recompute_scene_state=*/true);
    uber_struct_system->ForceUpdateAllSessions();

    bool callback_called = false;
    engine.RenderScheduledFrame(
        /*frame_number=*/3, /*presentation_time=*/zx::time(3000), flatland_display.get(),
        [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
    EXPECT_TRUE(callback_called);
    EXPECT_EQ(engine.scene_state_for_test().rebuild_count, 2u);
  }

  // Frame 4: a link resolves in `link_system` without a session publishing a rebuild request
  // (`MustRecomputeSceneState()` is false); `links_changed` triggers a rebuild.
  {
    auto dispatcher_holder = std::make_shared<utils::UnownedDispatcherHolder>(dispatcher());
    zx::channel endpoint1, endpoint2;
    ASSERT_EQ(zx::channel::create(0, &endpoint1, &endpoint2), ZX_OK);
    fuchsia_ui_views::ViewCreationToken child_token({.value = std::move(endpoint1)});
    fuchsia_ui_views::ViewportCreationToken parent_token({.value = std::move(endpoint2)});

    auto [parent_client_end, parent_server_end] =
        fidl::Endpoints<fuchsia_ui_composition::ParentViewportWatcher>::Create();
    fidl::Client<fuchsia_ui_composition::ParentViewportWatcher> parent_viewport_watcher(
        std::move(parent_client_end), dispatcher());
    const flatland::TransformHandle child_transform(uber_struct_system->GetNextInstanceId(), 1);
    auto link_to_parent = link_system->CreateLinkToParent(
        dispatcher_holder, std::move(child_token), /*view_identity=*/std::nullopt,
        std::move(parent_server_end), child_transform,
        [](const std::string& error_log) { GTEST_FAIL() << error_log; });

    auto [child_client_end, child_server_end] =
        fidl::Endpoints<fuchsia_ui_composition::ChildViewWatcher>::Create();
    fidl::Client<fuchsia_ui_composition::ChildViewWatcher> child_view_watcher(
        std::move(child_client_end), dispatcher());
    fuchsia_ui_composition::ViewportProperties properties;
    properties.logical_size(fuchsia_math::SizeU{{.width = 1, .height = 2}});
    properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
    const flatland::TransformHandle parent_transform(uber_struct_system->GetNextInstanceId(), 2);
    auto link_to_child = link_system->CreateLinkToChild(
        dispatcher_holder, std::move(parent_token), std::move(properties),
        std::move(child_server_end), parent_transform,
        [](const std::string& error_log) { GTEST_FAIL() << error_log; });

    EXPECT_FALSE(uber_struct_system->MustRecomputeSceneState());

    bool callback_called = false;
    engine.RenderScheduledFrame(
        /*frame_number=*/4, /*presentation_time=*/zx::time(4000), flatland_display.get(),
        [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
    EXPECT_TRUE(callback_called);
    EXPECT_EQ(engine.scene_state_for_test().rebuild_count, 3u);
  }

  flatland_display.reset();
  RunLoopUntilIdle();

  ExpectDisplayCleanup();
}

// A link change that lands after `RenderScheduledFrame()` but before `GenerateViewTreeSnapshot()`
// still reaches the view tree on the next frame.
TEST_F(DisplayCompositorTest, ViewTreeRegeneratesForLinkChangeAfterRender) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  auto display =
      std::make_shared<display::Display>(display::WireDisplayId{.value = kDisplayId.value()},
                                         resolution.x, resolution.y, kMaxDisplayLayersCount);

  // Set up mock coordinator expectations for AddDisplay().
  next_layer_id_ = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [this](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                     request,
                 MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, next_layer_id_++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_))
      .WillRepeatedly(Return(kPixelFormat));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).WillRepeatedly(Return());

  engine.AddDisplay(*display, /*num_vmos=*/0);
  RunLoopUntilIdle();

  const auto session_id = scheduling::GetNextSessionId();
  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_ui_composition::FlatlandDisplay>::Create();
  auto uber_struct_queue = uber_struct_system->AllocateQueueForSession(session_id);
  auto flatland_display = FlatlandDisplay::New(
      std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()), std::move(server_end),
      session_id, display,
      /*destroy_display_function=*/[] {}, flatland_presenter, link_system, uber_struct_queue);

  const flatland::TransformHandle root = flatland_display->root_transform();

  // Render frame 1 and generate a view tree (full snapshot).
  auto uber_struct = std::make_unique<flatland::UberStruct>();
  uber_struct->local_topology = {{root, 0}};
  uber_struct_queue->Push(/*present_id=*/1, std::move(uber_struct),
                          /*recompute_view_tree=*/false, /*recompute_scene_state=*/true);
  uber_struct_system->ForceUpdateAllSessions();

  bool callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/1, /*presentation_time=*/zx::time(1000), flatland_display.get(),
      [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
  EXPECT_TRUE(callback_called);

  auto first_snapshot = engine.GenerateViewTreeSnapshot(root);
  EXPECT_TRUE(std::holds_alternative<std::unique_ptr<view_tree::SubtreeSnapshot>>(first_snapshot));

  // Resolve a link through the engine's LinkSystem synchronously.
  auto dispatcher_holder = std::make_shared<utils::UnownedDispatcherHolder>(dispatcher());
  zx::channel endpoint1, endpoint2;
  ASSERT_EQ(zx::channel::create(0, &endpoint1, &endpoint2), ZX_OK);
  fuchsia_ui_views::ViewCreationToken child_token({.value = std::move(endpoint1)});
  fuchsia_ui_views::ViewportCreationToken parent_token({.value = std::move(endpoint2)});

  auto [parent_client_end, parent_server_end] =
      fidl::Endpoints<fuchsia_ui_composition::ParentViewportWatcher>::Create();
  fidl::Client<fuchsia_ui_composition::ParentViewportWatcher> parent_viewport_watcher(
      std::move(parent_client_end), dispatcher());
  const flatland::TransformHandle child_transform(uber_struct_system->GetNextInstanceId(), 1);
  auto link_to_parent = link_system->CreateLinkToParent(
      dispatcher_holder, std::move(child_token), /*view_identity=*/std::nullopt,
      std::move(parent_server_end), child_transform,
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  auto [child_client_end, child_server_end] =
      fidl::Endpoints<fuchsia_ui_composition::ChildViewWatcher>::Create();
  fidl::Client<fuchsia_ui_composition::ChildViewWatcher> child_view_watcher(
      std::move(child_client_end), dispatcher());
  fuchsia_ui_composition::ViewportProperties properties;
  properties.logical_size(fuchsia_math::SizeU{{.width = 1, .height = 2}});
  properties.inset(fuchsia_math::Inset{{.top = 0, .right = 0, .bottom = 0, .left = 0}});
  const flatland::TransformHandle parent_transform(uber_struct_system->GetNextInstanceId(), 2);
  auto link_to_child = link_system->CreateLinkToChild(
      dispatcher_holder, std::move(parent_token), std::move(properties),
      std::move(child_server_end), parent_transform,
      [](const std::string& error_log) { GTEST_FAIL() << error_log; });

  // Generate a view tree: expected "no diff", since scene_state_ has not changed.
  auto intermediate_snapshot = engine.GenerateViewTreeSnapshot(root);
  EXPECT_TRUE(std::holds_alternative<view_tree::SubtreeSnapshotNoDiff>(intermediate_snapshot));

  // Render frame 2 and generate a view tree (expected: full snapshot).
  callback_called = false;
  engine.RenderScheduledFrame(
      /*frame_number=*/2, /*presentation_time=*/zx::time(2000), flatland_display.get(),
      [&callback_called](const scheduling::Timestamps&) { callback_called = true; });
  EXPECT_TRUE(callback_called);

  auto second_snapshot = engine.GenerateViewTreeSnapshot(root);
  EXPECT_TRUE(std::holds_alternative<std::unique_ptr<view_tree::SubtreeSnapshot>>(second_snapshot));

  flatland_display.reset();
  RunLoopUntilIdle();

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, LazyImageImport_DirectToDisplayOnlyImportsToDisplay) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  AddDisplayWithExpectations(&display, display_info);

  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  const display::WireImageId fidl_image_id = image_metadata.identifier.ToFidl();

  // `ImportBufferImage()` must not eagerly import to either display or GPU.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  testing::Mock::VerifyAndClearExpectations(mock_display_coordinator_.get());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Rendering via direct-to-display lazily imports the image to the display coordinator once, and
  // never imports it to the GPU renderer.
  EXPECT_CALL(
      *mock_display_coordinator_,
      ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                   Eq(kDisplayBufferCollectionId)),
                                 MatchRequestField(ImportImage, buffer_index, Eq(0)),
                                 MatchRequestField(ImportImage, image_id, Eq(fidl_image_id))),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);

  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryPosition(_, _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryAlpha(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  ResolvedLayer layer = MakeImageLayer(image_metadata);
  RenderData render_data = {
      .display_id = kDisplayId,
      .layers = std::span<const ResolvedLayer>(&layer, 1),
  };

  EXPECT_TRUE(TryDirectToDisplay(std::span<const RenderData>(&render_data, 1)));

  // Releasing the image only releases it from the display coordinator, not the GPU renderer.
  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(fidl_image_id)), _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier)).Times(0);
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, LazyImageImport_ImportedToBothDisplayAndGpuReleasesFromBoth) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  AddDisplayWithExpectations(&display, display_info);

  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  const display::WireImageId fidl_image_id = image_metadata.identifier.ToFidl();

  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  // First frame uses direct-to-display, importing the image to the display coordinator.
  EXPECT_CALL(
      *mock_display_coordinator_,
      ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                   Eq(kDisplayBufferCollectionId)),
                                 MatchRequestField(ImportImage, buffer_index, Eq(0)),
                                 MatchRequestField(ImportImage, image_id, Eq(fidl_image_id))),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryPosition(_, _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryAlpha(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  ResolvedLayer layer = MakeImageLayer(image_metadata);
  RenderData render_data = {
      .display_id = kDisplayId,
      .layers = std::span<const ResolvedLayer>(&layer, 1),
  };
  EXPECT_TRUE(TryDirectToDisplay(std::span<const RenderData>(&render_data, 1)));

  // Next, GPU composition (or `EnsureGpuImagesImported()`) imports the same image to the GPU
  // renderer.
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer, 1));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Releasing the image now releases it from both the display coordinator and the GPU renderer.
  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(fidl_image_id)), _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, LazyImageImport_RendererOnlyModeImportsToGpuImmediately) {
  ForceRendererOnlyMode(true);

  const allocation::GlobalBufferCollectionId kGlobalBufferCollectionId =
      allocation::GenerateUniqueBufferCollectionId();
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([&token_ref](auto id, auto& client, auto token, auto usage, auto size) {
        token_ref = std::move(token);
        return fpromise::make_ok_promise();
      });
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, CreateToken(),
      BufferCollectionUsage::kClientImage, std::nullopt)));

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };

  // When `enable_direct_to_display` is false, `ImportBufferImage()` immediately imports on the GPU
  // without populating `buffer_collection_supports_display_`.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  EXPECT_FALSE(HasDisplaySupportEntry(kGlobalBufferCollectionId));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  EXPECT_CALL(*mock_display_coordinator_, ReleaseImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, RendererOnly_UnregisteredCollectionDoesNotPopulateDisplaySupportMap) {
  ForceRendererOnlyMode(true);

  const allocation::GlobalBufferCollectionId kUnregisteredCollectionId =
      allocation::GenerateUniqueBufferCollectionId();
  const ImageMetadata image_metadata = {
      .collection_id = kUnregisteredCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };

  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::error()));
  EXPECT_FALSE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  EXPECT_FALSE(HasDisplaySupportEntry(kUnregisteredCollectionId));

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, LazyImageImport_EnsureGpuImagesImportedRealizesPendingGpuImport) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };

  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  ResolvedLayer layer = MakeImageLayer(image_metadata);

  // `EnsureGpuImagesImported()` (used by `Engine::GetRenderables()` for screenshots/screen-capture)
  // realizes the pending GPU import without importing to the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer, 1));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Subsequent calls are a no-op since the pending GPU import has already been consumed.
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer, 1));

  // Releasing the image releases it from the GPU renderer and not the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_, ReleaseImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, LazyImageImport_EngineGetRenderablesImportsGpuImages) {
  auto flatland_presenter =
      std::make_shared<flatland::FlatlandPresenterImpl>(dispatcher(), fake_frame_scheduler_);
  auto uber_struct_system = std::make_shared<flatland::UberStructSystem>();
  auto link_system =
      std::make_shared<flatland::LinkSystem>(uber_struct_system->GetNextInstanceId());

  flatland::Engine engine(display_compositor_, flatland_presenter, uber_struct_system, link_system,
                          inspect::Node(),
                          /*get_root_transform=*/[]() -> std::optional<flatland::TransformHandle> {
                            return std::nullopt;
                          });

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  auto display =
      std::make_shared<display::Display>(display::WireDisplayId{.value = kDisplayId.value()},
                                         resolution.x, resolution.y, kMaxDisplayLayersCount);

  next_layer_id_ = 1;
  EXPECT_CALL(*mock_display_coordinator_, CreateLayer(_, _))
      .Times(kMaxDisplayLayersCount + 1)
      .WillRepeatedly(testing::Invoke(
          [this](fidl::WireServer<fuchsia_hardware_display::Coordinator>::CreateLayerRequestView
                     request,
                 MockDisplayCoordinator::CreateLayerCompleter::Sync& completer) {
            EXPECT_EQ(request->layer_id.value, next_layer_id_++);
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*renderer_, ChoosePreferredRenderTargetFormat(_))
      .WillRepeatedly(Return(kPixelFormat));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).WillRepeatedly(Return());

  engine.AddDisplay(*display, /*num_vmos=*/0);
  RunLoopUntilIdle();

  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 100,
      .height = 100,
  };
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  const auto session_id = scheduling::GetNextSessionId();
  auto [client_end, server_end] =
      fidl::Endpoints<fuchsia_ui_composition::FlatlandDisplay>::Create();
  auto uber_struct_queue = uber_struct_system->AllocateQueueForSession(session_id);
  auto flatland_display = FlatlandDisplay::New(
      std::make_shared<utils::UnownedDispatcherHolder>(dispatcher()), std::move(server_end),
      session_id, display,
      /*destroy_display_function=*/[] {}, flatland_presenter, link_system, uber_struct_queue);

  auto uber_struct = std::make_unique<flatland::UberStruct>();
  uber_struct->local_topology = {{flatland_display->root_transform(), 0}};
  flatland::LayerHandle layer_handle(session_id, 1);
  uber_struct->layer_stacks[flatland_display->root_transform()] =
      std::pmr::vector<flatland::LayerHandle>({layer_handle}, uber_struct->resource());
  flatland::UberStructLayer layer;
  layer.common.display_rect = types::Rectangle::From(types::Point2({.x = 0, .y = 0}),
                                                     types::Extent2({.width = 100, .height = 100}));
  layer.content = flatland::UberStructLayer::ImageModeProperties{
      .sample_rect = types::RectangleF::From(types::Point2F({.x = 0.f, .y = 0.f}),
                                             types::Extent2F({.width = 100.f, .height = 100.f})),
      .image_id = image_metadata.identifier,
      .image_width = image_metadata.width,
      .image_height = image_metadata.height,
  };
  uber_struct->layers[layer_handle] = layer;
  uber_struct_queue->Push(/*present_id=*/1, std::move(uber_struct), /*recompute_view_tree=*/false,
                          /*recompute_scene_state=*/true);
  uber_struct_system->ForceUpdateAllSessions();

  // `Engine::GetRenderables()` must realize the pending lazy GPU import for `image_metadata`.
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  Renderables renderables = engine.GetRenderables(*flatland_display);
  EXPECT_EQ(renderables.size(), 1u);
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  flatland_display.reset();
  RunLoopUntilIdle();

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, LazyImageImport_GpuImportFailure) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);
  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  // Defer `ReleaseBufferCollection()` while both display and GPU imports are pending.
  EXPECT_CALL(*mock_display_coordinator_, ReleaseBufferCollection(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);
  testing::Mock::VerifyAndClearExpectations(mock_display_coordinator_.get());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  ResolvedLayer layer = MakeImageLayer(image_metadata);

  // When the lazy GPU import fails during `EnsureGpuImagesImported()`, the pending GPU import count
  // still resolves and triggers the deferred `renderer_->ReleaseBufferCollection()`.
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::error()));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer, 1));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Subsequent `EnsureGpuImagesImported()` calls do not retry the failed import.
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _)).Times(0);
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer, 1));

  // Because GPU import failed, `ReleaseBufferImage()` must not call
  // `renderer_->ReleaseBufferImage()` and must release the collection from the display coordinator.
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier)).Times(0);
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, LazyImageImport_DisplayImportFailureFallsBackToGpu) {
  const allocation::GlobalBufferCollectionId kGlobalBufferCollectionId =
      allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  AddDisplayWithExpectations(&display, display_info);

  // Set up a synthetic render target on the display so `PerformGpuComposition()` can run.
  const ImageMetadata render_target = {
      .collection_id = allocation::GenerateUniqueBufferCollectionId(),
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = resolution.x,
      .height = resolution.y,
  };
  AddSyntheticRenderTarget(kDisplayId, render_target);

  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  const display::WireImageId fidl_image_id = image_metadata.identifier.ToFidl();

  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  // When `RenderFrame()` attempts direct-to-display, `mock_display_coordinator_->ImportImage()`
  // fails. `RenderFrame()` then falls back to `PerformGpuComposition()`, which lazily imports the
  // image on the GPU via `renderer_->ImportBufferImage()` and renders it.
  EXPECT_CALL(
      *mock_display_coordinator_,
      ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                   Eq(kDisplayBufferCollectionId)),
                                 MatchRequestField(ImportImage, buffer_index, Eq(0)),
                                 MatchRequestField(ImportImage, image_id, Eq(fidl_image_id))),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::error(ZX_ERR_INVALID_ARGS));
      }));
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  EXPECT_CALL(*renderer_, RequiresRenderInProtected(_)).WillOnce(Return(false));
  EXPECT_CALL(*renderer_, Render(render_target, _, _)).Times(1).WillOnce(Return());

  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryPosition(_, _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryAlpha(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).Times(1).WillOnce(Return());

  ResolvedLayer layer = MakeImageLayer(image_metadata);
  RenderData render_data = {
      .display_id = kDisplayId,
      .layers = std::span<const ResolvedLayer>(&layer, 1),
  };

  EXPECT_EQ(display_compositor_->RenderFrame(1, zx::time_monotonic(1),
                                             std::span<const RenderData>(&render_data, 1), {}, {},
                                             {}, [](const scheduling::Timestamps&) {}),
            DisplayCompositor::RenderFrameResult::kGpuComposition);

  // Because display import failed and GPU import succeeded, releasing the image only releases it
  // from the GPU renderer, not the display coordinator.
  EXPECT_CALL(*mock_display_coordinator_, ReleaseImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  ClearSyntheticRenderTargets(kDisplayId);
  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest, ImportRenderTargetImage_EagerImportAndRollbackOnDisplayFailure) {
  const allocation::GlobalBufferCollectionId kGlobalBufferCollectionId =
      allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);
  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata render_target_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  const display::WireImageId fidl_image_id = render_target_metadata.identifier.ToFidl();

  // Case 1: Display coordinator `ImportImage()` fails for `kRenderTarget`; `ImportBufferImage()`
  // must roll back the renderer import via `renderer_->ReleaseBufferImage()` and return an error.
  EXPECT_CALL(*renderer_,
              ImportBufferImage(render_target_metadata, BufferCollectionUsage::kRenderTarget))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  EXPECT_CALL(
      *mock_display_coordinator_,
      ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                   Eq(kDisplayBufferCollectionId)),
                                 MatchRequestField(ImportImage, image_id, Eq(fidl_image_id))),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::error(ZX_ERR_INVALID_ARGS));
      }));
  EXPECT_CALL(*renderer_, ReleaseBufferImage(render_target_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  EXPECT_FALSE(RunPromise(display_compositor_->ImportBufferImage(
      render_target_metadata, BufferCollectionUsage::kRenderTarget)));
  testing::Mock::VerifyAndClearExpectations(mock_display_coordinator_.get());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Case 2: Both renderer and display coordinator imports succeed eagerly for `kRenderTarget`;
  // `ReleaseBufferImage()` releases from both.
  EXPECT_CALL(*renderer_,
              ImportBufferImage(render_target_metadata, BufferCollectionUsage::kRenderTarget))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  EXPECT_CALL(
      *mock_display_coordinator_,
      ImportImage(testing::AllOf(MatchRequestField(ImportImage, buffer_collection_id,
                                                   Eq(kDisplayBufferCollectionId)),
                                 MatchRequestField(ImportImage, image_id, Eq(fidl_image_id))),
                  _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      render_target_metadata, BufferCollectionUsage::kRenderTarget)));
  testing::Mock::VerifyAndClearExpectations(mock_display_coordinator_.get());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(fidl_image_id)), _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*renderer_, ReleaseBufferImage(render_target_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(render_target_metadata.identifier);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, ImportBufferImage_EagerGpuImportReleasedBeforePromiseResolves) {
  ForceRendererOnlyMode(true);

  const allocation::GlobalBufferCollectionId kGlobalBufferCollectionId =
      allocation::GenerateUniqueBufferCollectionId();
  fidl::ClientEnd<fuchsia_sysmem2::BufferCollectionToken> token_ref;
  EXPECT_CALL(*renderer_, ImportBufferCollection(kGlobalBufferCollectionId, _, _, _, _))
      .WillOnce([&token_ref](auto id, auto& client, auto token, auto usage, auto size) {
        token_ref = std::move(token);
        return fpromise::make_ok_promise();
      });
  EXPECT_TRUE(RunPromise(display_compositor_->ImportBufferCollection(
      kGlobalBufferCollectionId, sysmem_allocator_, CreateToken(),
      BufferCollectionUsage::kClientImage, std::nullopt)));

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };

  fpromise::bridge<> bridge;
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(testing::Invoke([&bridge](const ImageMetadata&, BufferCollectionUsage) {
        return bridge.consumer.promise();
      }));

  async::Executor executor(dispatcher());
  bool promise_completed = false;
  executor.schedule_task(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)
          .then([&promise_completed](fpromise::result<>&) { promise_completed = true; }));
  RunLoopUntilIdle();
  EXPECT_FALSE(promise_completed);

  // Release the image before `bridge` resolves.
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Resolve the import promise and verify that no stale entry remains in `image_import_map_`.
  bridge.completer.complete_ok();
  RunLoopUntilIdle();
  EXPECT_TRUE(promise_completed);

  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier)).Times(0);
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest,
       ReleaseBufferCollection_AfterDisplayImportRealized_ReleasesDisplayImmediatelyAndDefersGpu) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  AddDisplayWithExpectations(&display, display_info);

  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  const display::WireImageId fidl_image_id = image_metadata.identifier.ToFidl();
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  // Realize display import via `TryDirectToDisplay()`.
  libsync::Completion commit_done;
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryPosition(_, _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryAlpha(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&commit_done](fuchsia_hardware_display::wire::CoordinatorCommitConfigRequest*,
                         MockDisplayCoordinator::CommitConfigCompleter::Sync&) {
            commit_done.Signal();
          }));

  ResolvedLayer layer = MakeImageLayer(image_metadata);
  RenderData render_data = {
      .display_id = kDisplayId,
      .layers = std::span<const ResolvedLayer>(&layer, 1),
  };
  EXPECT_TRUE(TryDirectToDisplay(std::span<const RenderData>(&render_data, 1)));
  commit_done.Wait();

  // Now `ReleaseBufferCollection()` releases the display collection immediately (no pending display
  // imports remain) and defers the GPU collection release (GPU import is still pending).
  libsync::Completion display_collection_released;
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_collection_released](
              fuchsia_hardware_display::wire::CoordinatorReleaseBufferCollectionRequest*,
              MockDisplayCoordinator::ReleaseBufferCollectionCompleter::Sync&) {
            display_collection_released.Signal();
          }));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);
  display_collection_released.Wait();
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // `ReleaseBufferImage()` releases the display image and drops the pending GPU import, triggering
  // the deferred `renderer_->ReleaseBufferCollection()`.
  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(fidl_image_id)), _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier)).Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  ExpectDisplayCleanup();
}

TEST_F(DisplayCompositorTest,
       ReleaseBufferCollection_AfterGpuImportRealized_ReleasesGpuImmediatelyAndDefersDisplay) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);
  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  // Realize GPU import first via `EnsureGpuImagesImported()`.
  ResolvedLayer layer = MakeImageLayer(image_metadata);
  EXPECT_CALL(*renderer_, ImportBufferImage(image_metadata, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer, 1));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // `ReleaseBufferCollection()` releases the GPU collection immediately and defers the display
  // collection release.
  libsync::Completion display_collection_released;
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_collection_released](
              fuchsia_hardware_display::wire::CoordinatorReleaseBufferCollectionRequest*,
              MockDisplayCoordinator::ReleaseBufferCollectionCompleter::Sync&) {
            display_collection_released.Signal();
          }));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);
  EXPECT_FALSE(display_collection_released.signaled());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // `ReleaseBufferImage()` releases the GPU image and triggers the deferred display collection
  // release.
  EXPECT_CALL(*mock_display_coordinator_, ReleaseImage(_, _)).Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image_metadata.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);
  display_collection_released.Wait();

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest,
       ReleaseBufferCollection_WhileBothPending_ThenDirectToDisplayRealizesDisplayRelease) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);

  const display::DisplayId kDisplayId(1);
  glm::uvec2 resolution(1024, 768);
  DisplayInfo display_info = {resolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, resolution.x, resolution.y,
                           kMaxDisplayLayersCount);
  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).Times(1).WillOnce(Return());
  AddDisplayWithExpectations(&display, display_info);

  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  const display::WireImageId fidl_image_id = image_metadata.identifier.ToFidl();
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));

  // Call `ReleaseBufferCollection()` while both display and GPU imports are pending.
  libsync::Completion display_collection_released;
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_collection_released](
              fuchsia_hardware_display::wire::CoordinatorReleaseBufferCollectionRequest*,
              MockDisplayCoordinator::ReleaseBufferCollectionCompleter::Sync&) {
            display_collection_released.Signal();
          }));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);
  EXPECT_FALSE(display_collection_released.signaled());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Direct-to-display render imports the image to the display coordinator and immediately releases
  // the display buffer collection.
  libsync::Completion commit_done;
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _))
      .Times(1)
      .WillOnce(testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                                   MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
        completer.Reply(fit::ok());
      }));
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryPosition(_, _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryAlpha(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).Times(1).WillOnce(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .Times(1)
      .WillOnce(testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
        completer.Reply(display::WireConfigResult::kOk);
      }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&commit_done](fuchsia_hardware_display::wire::CoordinatorCommitConfigRequest*,
                         MockDisplayCoordinator::CommitConfigCompleter::Sync&) {
            commit_done.Signal();
          }));

  ResolvedLayer layer = MakeImageLayer(image_metadata);
  RenderData render_data = {
      .display_id = kDisplayId,
      .layers = std::span<const ResolvedLayer>(&layer, 1),
  };
  EXPECT_TRUE(TryDirectToDisplay(std::span<const RenderData>(&render_data, 1)));
  display_collection_released.Wait();
  commit_done.Wait();

  // Releasing the image releases the display image and triggers the deferred GPU collection
  // release.
  EXPECT_CALL(*mock_display_coordinator_,
              ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(fidl_image_id)), _))
      .Times(1)
      .WillOnce(Return());
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image_metadata.identifier);

  ExpectDisplayCleanup();
}

TEST_F(
    DisplayCompositorTest,
    ReleaseBufferCollection_ReleaseBufferImageFromBackgroundThreadPostsGpuCollectionReleaseToMainDispatcher) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);
  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image_metadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image_metadata, BufferCollectionUsage::kClientImage)));
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);

  // Calling `ReleaseBufferImage()` from a background thread (where `async_get_default_dispatcher()`
  // is not `main_dispatcher_`) releases the display collection under `lock_` and posts
  // `renderer_->ReleaseBufferCollection()` to `main_dispatcher_`.
  libsync::Completion display_collection_released;
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_collection_released](
              fuchsia_hardware_display::wire::CoordinatorReleaseBufferCollectionRequest*,
              MockDisplayCoordinator::ReleaseBufferCollectionCompleter::Sync&) {
            display_collection_released.Signal();
          }));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);

  std::thread background_thread(
      [compositor = display_compositor_, image_id = image_metadata.identifier]() {
        compositor->ReleaseBufferImage(image_id);
      });
  background_thread.join();
  display_collection_released.Wait();
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Running the main loop executes the posted `renderer_->ReleaseBufferCollection()` task on
  // `main_dispatcher_`.
  bool released_on_main_dispatcher = false;
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _))
      .Times(1)
      .WillOnce(testing::Invoke([this, &released_on_main_dispatcher](
                                    allocation::GlobalBufferCollectionId, BufferCollectionUsage) {
        released_on_main_dispatcher = (async_get_default_dispatcher() == dispatcher());
      }));
  RunLoopUntilIdle();
  EXPECT_TRUE(released_on_main_dispatcher);

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest,
       ReleaseBufferCollection_MultiplePendingImages_ReleasesOnlyWhenLastImageResolves) {
  const uint64_t kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  const display::WireBufferCollectionId kDisplayBufferCollectionId =
      display::ToDisplayFidlBufferCollectionId(kGlobalBufferCollectionId);
  auto token_ref = ImportDisplaySupportedClientCollection(kGlobalBufferCollectionId);

  const ImageMetadata image1 = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 256,
  };
  const ImageMetadata image2 = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 1,
      .width = 128,
      .height = 256,
  };
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image1, BufferCollectionUsage::kClientImage)));
  EXPECT_TRUE(RunPromise(
      display_compositor_->ImportBufferImage(image2, BufferCollectionUsage::kClientImage)));

  libsync::Completion display_collection_released;
  EXPECT_CALL(
      *mock_display_coordinator_,
      ReleaseBufferCollection(MatchRequestField(ReleaseBufferCollection, buffer_collection_id,
                                                Eq(kDisplayBufferCollectionId)),
                              _))
      .Times(1)
      .WillOnce(testing::Invoke(
          [&display_collection_released](
              fuchsia_hardware_display::wire::CoordinatorReleaseBufferCollectionRequest*,
              MockDisplayCoordinator::ReleaseBufferCollectionCompleter::Sync&) {
            display_collection_released.Signal();
          }));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);
  display_compositor_->ReleaseBufferCollection(kGlobalBufferCollectionId,
                                               BufferCollectionUsage::kClientImage);
  EXPECT_FALSE(display_collection_released.signaled());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Realize `image1` on the GPU: `image2` still has a pending GPU import, so the GPU collection is
  // not released yet.
  ResolvedLayer layer1 = MakeImageLayer(image1);
  EXPECT_CALL(*renderer_, ImportBufferImage(image1, _))
      .Times(1)
      .WillOnce(ReturnPromise(fpromise::ok()));
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);
  display_compositor_->EnsureGpuImagesImported(std::span<const ResolvedLayer>(&layer1, 1));
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Release `image1`: `image2` still has both display and GPU imports pending, so neither
  // collection release fires yet.
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _)).Times(0);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image1.identifier)).Times(1).WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image1.identifier);
  EXPECT_FALSE(display_collection_released.signaled());
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  // Release `image2`: now the last pending display and GPU imports resolve and both collections are
  // released.
  EXPECT_CALL(*renderer_, ReleaseBufferCollection(kGlobalBufferCollectionId, _))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image2.identifier);
  display_collection_released.Wait();

  EXPECT_CALL(*mock_display_coordinator_, DiscardConfig(_)).Times(1).WillOnce(Return());
}

TEST_F(DisplayCompositorTest, ClientImageUsageInspectCounts) {
  const auto kGlobalBufferCollectionId = allocation::GenerateUniqueBufferCollectionId();
  SetDisplaySupported(kGlobalBufferCollectionId, true);

  const display::DisplayId kDisplayId(1);
  const glm::uvec2 kResolution(1024, 768);
  const DisplayInfo kDisplayInfo = {kResolution, {kPixelFormat}, kMaxDisplayLayersCount};
  display::Display display({kDisplayId.ToFidl()}, kResolution.x, kResolution.y,
                           kMaxDisplayLayersCount);

  EXPECT_CALL(*mock_display_coordinator_, SetLayerColorConfig(_, _)).WillRepeatedly(Return());
  AddDisplayWithExpectations(&display, kDisplayInfo);

  const ImageMetadata kRenderTargetMetadata = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = kResolution.x,
      .height = kResolution.y,
  };
  AddSyntheticRenderTarget(kDisplayId, kRenderTargetMetadata);

  // Importing and releasing a `kRenderTarget` image must not be counted as a client image.
  EXPECT_CALL(*mock_display_coordinator_, ImportImage(_, _))
      .WillRepeatedly(
          testing::Invoke([](fuchsia_hardware_display::wire::CoordinatorImportImageRequest*,
                             MockDisplayCoordinator::ImportImageCompleter::Sync& completer) {
            completer.Reply(fit::ok());
          }));
  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).WillRepeatedly(ReturnPromise(fpromise::ok()));

  const ImageMetadata render_target_image = {
      .collection_id = kGlobalBufferCollectionId,
      .identifier = allocation::GenerateUniqueImageId(),
      .vmo_index = 0,
      .width = 128,
      .height = 128,
  };
  ASSERT_TRUE(RunPromise(display_compositor_->ImportBufferImage(
      render_target_image, BufferCollectionUsage::kRenderTarget)));

  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{}));

  auto expect_display_release_image = [&](const ImageMetadata& image,
                                          libsync::Completion& released) {
    EXPECT_CALL(
        *mock_display_coordinator_,
        ReleaseImage(MatchRequestField(ReleaseImage, image_id, Eq(image.identifier.ToFidl())), _))
        .Times(1)
        .WillOnce(testing::Invoke(
            [&released](fuchsia_hardware_display::wire::CoordinatorReleaseImageRequest*,
                        MockDisplayCoordinator::ReleaseImageCompleter::Sync&) {
              released.Signal();
            }));
  };

  libsync::Completion render_target_released;
  expect_display_release_image(render_target_image, render_target_released);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(render_target_image.identifier))
      .Times(1)
      .WillOnce(Return());
  display_compositor_->ReleaseBufferImage(render_target_image.identifier);
  render_target_released.Wait();
  testing::Mock::VerifyAndClearExpectations(renderer_.get());

  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{}));

  EXPECT_CALL(*renderer_, ImportBufferImage(_, _)).WillRepeatedly(ReturnPromise(fpromise::ok()));

  auto make_client_image = [&](allocation::GlobalBufferCollectionId collection_id) {
    ImageMetadata metadata = {
        .collection_id = collection_id,
        .identifier = allocation::GenerateUniqueImageId(),
        .vmo_index = 0,
        .width = 128,
        .height = 256,
    };
    EXPECT_TRUE(RunPromise(
        display_compositor_->ImportBufferImage(metadata, BufferCollectionUsage::kClientImage)));
    return metadata;
  };

  // Import 4 display-supported client images:
  // - image1: D2D only
  // - image2: D2D first, then GPU (becomes Both direct to display and GPU)
  // - image3: GPU first, then D2D (becomes Both direct to display and GPU)
  // - image4: Unused
  const ImageMetadata image1 = make_client_image(kGlobalBufferCollectionId);
  const ImageMetadata image2 = make_client_image(kGlobalBufferCollectionId);
  const ImageMetadata image3 = make_client_image(kGlobalBufferCollectionId);
  const ImageMetadata image4 = make_client_image(kGlobalBufferCollectionId);

  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                              .total_imported = 4u,
                                              .active = 4u,
                                              .direct_to_display_only = 0u,
                                              .gpu_composition_total = 0u,
                                              .gpu_composition_only = 0u,
                                              .both_d2d_and_gpu = 0u,
                                              .unused = 4u,
                                          }));

  // Import a client image on a non-display-supported collection (`!IsDisplaySupported()`), which is
  // imported eagerly to the GPU renderer and counted in the GPU-only bucket.
  const auto kGpuOnlyCollectionId = allocation::GenerateUniqueBufferCollectionId();
  SetDisplaySupported(kGpuOnlyCollectionId, false);
  const ImageMetadata image5 = make_client_image(kGpuOnlyCollectionId);

  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                              .total_imported = 5u,
                                              .active = 5u,
                                              .direct_to_display_only = 0u,
                                              .gpu_composition_total = 1u,
                                              .gpu_composition_only = 1u,
                                              .both_d2d_and_gpu = 0u,
                                              .unused = 4u,
                                          }));

  EXPECT_CALL(*mock_display_coordinator_, SetDisplayLayers(_, _)).WillRepeatedly(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryConfig(_, _)).WillRepeatedly(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryPosition(_, _)).WillRepeatedly(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerPrimaryAlpha(_, _)).WillRepeatedly(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetLayerImage2(_, _)).WillRepeatedly(Return());
  EXPECT_CALL(*mock_display_coordinator_, SetDisplayMode(_, _)).WillRepeatedly(Return());
  EXPECT_CALL(*mock_display_coordinator_, CheckConfig(_))
      .WillRepeatedly(
          testing::Invoke([](MockDisplayCoordinator::CheckConfigCompleter::Sync& completer) {
            completer.Reply(display::WireConfigResult::kOk);
          }));
  EXPECT_CALL(*mock_display_coordinator_, CommitConfig(_, _)).WillRepeatedly(Return());

  // Frame 1: Direct-to-display with image1 and image2.
  {
    std::array layers = {MakeImageLayer(image1), MakeImageLayer(image2)};
    RenderData render_data = {.display_id = kDisplayId, .layers = layers};
    auto result = display_compositor_->RenderFrame(1, zx::time_monotonic(1),
                                                   std::span<const RenderData>(&render_data, 1), {},
                                                   {}, {}, [](const scheduling::Timestamps&) {});
    EXPECT_EQ(result, DisplayCompositor::RenderFrameResult::kDirectToDisplay);

    EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                                .total_imported = 5u,
                                                .active = 5u,
                                                .direct_to_display_only = 2u,
                                                .gpu_composition_total = 1u,
                                                .gpu_composition_only = 1u,
                                                .both_d2d_and_gpu = 0u,
                                                .unused = 2u,
                                            }));
  }

  // Frame 2: GPU composition with image2 and image3.
  EXPECT_CALL(*renderer_, RequiresRenderInProtected(_)).WillRepeatedly(Return(false));
  EXPECT_CALL(*renderer_, Render(_, _, _))
      .WillRepeatedly(testing::Invoke([](const ImageMetadata&, std::span<const ResolvedLayer>,
                                         const Renderer::RenderArgs& render_args) {
        for (const auto& fence : render_args.release_fences) {
          fence.signal(0, ZX_EVENT_SIGNALED);
        }
      }));
  {
    std::array layers = {MakeImageLayer(image2), MakeImageLayer(image3)};
    RenderData render_data = {.display_id = kDisplayId, .layers = layers};
    auto result = display_compositor_->RenderFrame(
        2, zx::time_monotonic(2), std::span<const RenderData>(&render_data, 1), {}, {}, {},
        [](const scheduling::Timestamps&) {},
        DisplayCompositor::RenderFrameTestArgs{.force_gpu_composition = true});
    EXPECT_EQ(result, DisplayCompositor::RenderFrameResult::kGpuComposition);

    EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                                .total_imported = 5u,
                                                .active = 5u,
                                                .direct_to_display_only = 1u,
                                                .gpu_composition_total = 3u,
                                                .gpu_composition_only = 2u,
                                                .both_d2d_and_gpu = 1u,
                                                .unused = 1u,
                                            }));
  }

  // Frame 3: Direct-to-display with image1 (already D2D-only -> idempotent) and image3
  // (previously GPU-only -> now Both direct to display and GPU).
  {
    std::array layers = {MakeImageLayer(image1), MakeImageLayer(image3)};
    RenderData render_data = {.display_id = kDisplayId, .layers = layers};
    auto result = display_compositor_->RenderFrame(3, zx::time_monotonic(3),
                                                   std::span<const RenderData>(&render_data, 1), {},
                                                   {}, {}, [](const scheduling::Timestamps&) {});
    EXPECT_EQ(result, DisplayCompositor::RenderFrameResult::kDirectToDisplay);

    EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                                .total_imported = 5u,
                                                .active = 5u,
                                                .direct_to_display_only = 1u,
                                                .gpu_composition_total = 3u,
                                                .gpu_composition_only = 1u,
                                                .both_d2d_and_gpu = 2u,
                                                .unused = 1u,
                                            }));
  }

  // Release image1 (D2D-only) first, then image4 (Unused), image5 (GPU-only), and image2 (Both)
  // one by one, verifying after each release that `Active` decrements by 1 and all four lifetime
  // bucket counts remain intact.
  libsync::Completion image1_released;
  expect_display_release_image(image1, image1_released);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(_)).Times(0);
  display_compositor_->ReleaseBufferImage(image1.identifier);
  image1_released.Wait();
  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                              .total_imported = 5u,
                                              .active = 4u,
                                              .direct_to_display_only = 1u,
                                              .gpu_composition_total = 3u,
                                              .gpu_composition_only = 1u,
                                              .both_d2d_and_gpu = 2u,
                                              .unused = 1u,
                                          }));

  display_compositor_->ReleaseBufferImage(image4.identifier);
  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                              .total_imported = 5u,
                                              .active = 3u,
                                              .direct_to_display_only = 1u,
                                              .gpu_composition_total = 3u,
                                              .gpu_composition_only = 1u,
                                              .both_d2d_and_gpu = 2u,
                                              .unused = 1u,
                                          }));

  EXPECT_CALL(*renderer_, ReleaseBufferImage(image5.identifier)).Times(1).WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image5.identifier);
  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                              .total_imported = 5u,
                                              .active = 2u,
                                              .direct_to_display_only = 1u,
                                              .gpu_composition_total = 3u,
                                              .gpu_composition_only = 1u,
                                              .both_d2d_and_gpu = 2u,
                                              .unused = 1u,
                                          }));

  libsync::Completion image2_released;
  expect_display_release_image(image2, image2_released);
  EXPECT_CALL(*renderer_, ReleaseBufferImage(image2.identifier)).Times(1).WillOnce(Return());
  display_compositor_->ReleaseBufferImage(image2.identifier);
  image2_released.Wait();
  EXPECT_EQ(ReadClientImageUsageCounts(), (ClientImageUsageCounts{
                                              .total_imported = 5u,
                                              .active = 1u,
                                              .direct_to_display_only = 1u,
                                              .gpu_composition_total = 3u,
                                              .gpu_composition_only = 1u,
                                              .both_d2d_and_gpu = 2u,
                                              .unused = 1u,
                                          }));

  ClearSyntheticRenderTargets(kDisplayId);
  ExpectDisplayCleanup();
}

}  // namespace flatland::test
