// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_ENGINE_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_ENGINE_H_

#include <fidl/fuchsia.ui.display.color/cpp/fidl.h>
#include <lib/fit/function.h>
#include <lib/inspect/component/cpp/component.h>
#include <lib/zx/eventpair.h>

#include <map>
#include <memory_resource>
#include <optional>
#include <string>
#include <utility>

#include "src/ui/scenic/lib/display/fidl_id_types.h"
#include "src/ui/scenic/lib/flatland/engine/display_compositor.h"
#include "src/ui/scenic/lib/flatland/flatland_display.h"
#include "src/ui/scenic/lib/flatland/flatland_presenter_impl.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"
#include "src/ui/scenic/lib/flatland/link_system.h"
#include "src/ui/scenic/lib/flatland/uber_struct_system.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/view_tree/snapshot_types.h"

namespace flatland {

using GetRootTransformFunc = fit::function<std::optional<TransformHandle>()>;
using Renderables = std::vector<ResolvedLayer>;

// Engine is responsible for building a display list for DisplayCompositor, to insulate it from
// needing to know anything about the Flatland scene graph.
class Engine {
 public:
  // Holds the cached global transform state generated from each Flatland session's `UberStruct`
  // and linked together by the `LinkSystem`. Recomputed only when a transform-level change is made
  // in the global scene graph (`needs_full_rebuild == true`). Public for testing.
  struct SceneState {
    explicit SceneState(std::pmr::memory_resource* resource = std::pmr::get_default_resource())
        : links(resource) {}

    // Empties every field without deallocating memory, and sets `cleared`.
    void Clear();

    UberStructSnapshot snapshot;
    GlobalTopologyData::LinkTopologyMap links;
    // The `LinkSystem` generation that `links` was copied at (`0` on construction and after
    // `Clear()`). `RenderScheduledFrame()` and `GenerateViewTreeSnapshot()` compare against this
    // value to detect a link-topology change.
    uint64_t link_topology_generation = 0;
    flatland::GlobalTopologyData topology_data;
    flatland::GlobalMatrixVector global_matrices;
    flatland::GlobalTransformClipRegionVector clip_regions;
    flatland::GlobalOpacityVector opacities;
    std::vector<ResolvedLayerStack> resolved_layer_stacks;
    // Number of times the state was rebuilt by `PrepareSceneState()`,
    // rather than reusing the existing state.  Used by tests to distinguish
    // a frame that reused the cached state from one that rebuilt it.
    uint64_t rebuild_count = 0;
    // True while this object describes no frame: on construction and after `Clear()`. The rebuild
    // arm of `PrepareSceneState()` resets it. Reusing a cleared state is a caller error.
    bool cleared = true;
  };

  // Maintains `scene_state` for a frame.  Every frame, `snapshot`, `links`, and
  // `link_topology_generation` are moved into `scene_state`; `links` is `std::nullopt` when the
  // cached links are still current.  When `needs_full_rebuild` is true, the global transform state
  // is rebuilt; otherwise it is reused (debug builds use `FindStaleSceneStateInput()` to verify
  // that it's safe to reuse).
  static void PrepareSceneState(SceneState& scene_state, UberStructSnapshot snapshot,
                                std::optional<GlobalTopologyData::LinkTopologyMap> links,
                                uint64_t link_topology_generation,
                                TransformHandle::InstanceId link_system_id,
                                TransformHandle root_transform, bool needs_full_rebuild);

  // Returns a description of the first input of the cached global transform state in
  // `scene_state` that differs in the new frame's `snapshot`, `links`, or `root_transform`, or
  // `std::nullopt` if reusing that state for the new frame is valid.
  //
  // This is the only runtime check that reuse is correct.  A mutator that forgets its change
  // signal leaves the cached state stale, which in production shows up as stale rendering, hit
  // testing, and layout, with no error.  `PrepareSceneState()` runs it only in debug builds,
  // because it scans every session's transform inputs; it is a function so that tests can
  // exercise it in every build type.  When the transform stage gains an input, compare it here.
  static std::optional<std::string> FindStaleSceneStateInput(
      const SceneState& scene_state, const UberStructSnapshot& snapshot,
      const GlobalTopologyData::LinkTopologyMap& links, TransformHandle root_transform);

  Engine(std::shared_ptr<flatland::DisplayCompositor> flatland_compositor,
         std::shared_ptr<flatland::FlatlandPresenterImpl> flatland_presenter,
         std::shared_ptr<flatland::UberStructSystem> uber_struct_system,
         std::shared_ptr<flatland::LinkSystem> link_system, inspect::Node inspect_node,
         GetRootTransformFunc get_root_transform);
  ~Engine() = default;

  // Orchestrates the generation and submission of a frame to the `DisplayCompositor`.
  //
  // This updates scene topology and link watchers, culls invisible content, and
  // handles first-frame startup logic to avoid driving the display before content
  // is ready.
  //
  // When `display` is null because no FlatlandDisplay exists, clears `scene_state_` and skips the
  // frame so that `LinkSystem::UpdateLinkWatchers()` (and any direct `GenerateViewTreeSnapshot()`
  // call in tests; in production `App` emits an empty ViewTree snapshot directly when there is no
  // display) observes an empty scene instead of the last rendered one.
  void RenderScheduledFrame(uint64_t frame_number, zx::time presentation_time,
                            const FlatlandDisplay* display,
                            scheduling::FramePresentedCallback callback);

  // Dispatches updated layout information (coordinate transforms, view dimensions,
  // device pixel ratio, etc.) to layout observers and link watchers based on the
  // current frame's scene state.
  //
  // CRITICAL: This must be called *after* the new ViewTree snapshot has been fully
  // updated and published (e.g. in `UpdateSnapshot()`). This ensures layout observers
  // do not query or receive layout updates against a stale ViewTree snapshot.
  void UpdateLinkWatchersAfterViewTreePublished();

  // Snapshots the current Flatland content tree from the cached `scene_state_` prepared during
  // `RenderScheduledFrame()`. `root_transform` is set from the root transform of the display
  // returned from `FlatlandManager::GetPrimaryFlatlandDisplayForRendering`, and is checked
  // against the root of `scene_state_`'s global topology when non-empty.
  view_tree::GeneratedSubtreeSnapshot GenerateViewTreeSnapshot(
      const TransformHandle& root_transform);

  // Returns all renderables reachable from the display's root transform. As a side effect, runs
  // any pending lazy GPU imports for image layers in the returned renderables via
  // `DisplayCompositor::EnsureGpuImagesImported()`.
  Renderables GetRenderables(const FlatlandDisplay& display);

  static constexpr uint32_t kNumDisplayFramebuffers = 2;
  void AddDisplay(display::Display& display, uint32_t num_vmos = kNumDisplayFramebuffers);

  const SceneState& scene_state_for_test() const { return scene_state_; }

 private:
  // Initialize all inspect::Nodes, so that the Engine state can be observed.
  void InitializeInspectObjects();

  // Tally the frame result so that it can be displayed via Inspect.
  void RecordFrameResult(DisplayCompositor::RenderFrameResult result);

  // Takes pending fences from `flatland_presenter_` and forwards them with `callback` to
  // `DisplayCompositor::SkipFrame()`.
  void SkipRender(uint64_t frame_number, scheduling::FramePresentedCallback callback);

  std::shared_ptr<flatland::DisplayCompositor> flatland_compositor_;
  std::shared_ptr<flatland::FlatlandPresenterImpl> flatland_presenter_;
  std::shared_ptr<flatland::UberStructSystem> uber_struct_system_;
  std::shared_ptr<flatland::LinkSystem> link_system_;

  // Backs the link maps built each frame.  `Engine` runs only on the main thread,
  // so an unsynchronized pool is safe.  Declared before any member that can hold
  // a container using this pool, so the pool outlives those containers.
  std::pmr::unsynchronized_pool_resource link_map_pool_;

  // Persistent global transform state, maintained across frames by `PrepareSceneState()`.
  SceneState scene_state_{&link_map_pool_};

  bool first_frame_with_image_is_rendered_ = false;

  // The `scene_state_` link topology generation that the last generated view tree reflects.
  // Empty until the first view tree, which is always generated: a subtree generator may not
  // answer "no diff" the first time.
  std::optional<uint64_t> view_tree_link_topology_generation_;

  // Used to skip rendering until the display is added.
  std::map<display::DisplayId, bool> seen_display_ids_;

  inspect::Node inspect_node_;
  inspect::LazyNode inspect_scene_dump_;
  inspect::Node inspect_frame_results_;
  inspect::UintProperty inspect_direct_display_frame_count_;
  inspect::UintProperty inspect_gpu_composition_frame_count_;
  inspect::UintProperty inspect_failed_frame_count_;
  GetRootTransformFunc get_root_transform_;

  async::Executor executor_;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_ENGINE_ENGINE_H_
