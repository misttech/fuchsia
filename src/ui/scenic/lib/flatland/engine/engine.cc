// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#include "src/ui/scenic/lib/flatland/engine/engine.h"

#include <fidl/fuchsia.hardware.display.types/cpp/fidl.h>
#include <lib/async/cpp/time.h>
#include <lib/syslog/cpp/macros.h>

#include <array>
#include <cstddef>
#include <sstream>
#include <string>

#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_resolved_layers.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"
#include "src/ui/scenic/lib/flatland/scene_dumper.h"
#include "src/ui/scenic/lib/scheduling/frame_scheduler.h"
#include "src/ui/scenic/lib/utils/check_is_on_thread.h"
#include "src/ui/scenic/lib/utils/helpers.h"
#include "src/ui/scenic/lib/utils/logging.h"

// Hardcoded double buffering.
// TODO(https://fxbug.dev/42156567): make this configurable.  Even fancier: is it worth considering
// sharing a pool of framebuffers between multiple displays?  (assuming that their dimensions are
// similar, etc.)
static constexpr uint32_t kNumDisplayFramebuffers = 2;

namespace flatland {

Engine::Engine(std::shared_ptr<DisplayCompositor> flatland_compositor,
               std::shared_ptr<FlatlandPresenterImpl> flatland_presenter,
               std::shared_ptr<UberStructSystem> uber_struct_system,
               std::shared_ptr<LinkSystem> link_system, inspect::Node inspect_node,
               GetRootTransformFunc get_root_transform)
    : flatland_compositor_(std::move(flatland_compositor)),
      flatland_presenter_(std::move(flatland_presenter)),
      uber_struct_system_(std::move(uber_struct_system)),
      link_system_(std::move(link_system)),
      inspect_node_(std::move(inspect_node)),
      get_root_transform_(std::move(get_root_transform)),
      executor_(async_get_default_dispatcher()) {
  utils::CheckIsOnMainThread();
  FX_DCHECK(flatland_compositor_);
  FX_DCHECK(flatland_presenter_);
  FX_DCHECK(uber_struct_system_);
  FX_DCHECK(link_system_);
  InitializeInspectObjects();
}

constexpr char kSceneDump[] = "scene_dump";

void Engine::InitializeInspectObjects() {
  inspect_scene_dump_ = inspect_node_.CreateLazyValues(kSceneDump, [this] {
    utils::CheckIsOnMainThread();
    inspect::Inspector inspector;
    const auto root_transform = get_root_transform_();
    if (!root_transform) {
      inspector.GetRoot().CreateString(kSceneDump, "(No Root Transform)", &inspector);
      return fpromise::make_ok_promise(std::move(inspector));
    }

    SceneState scene_state(&link_map_pool_);
    GlobalTopologyData::LinkTopologyMap links(&link_map_pool_);
    const uint64_t link_topology_generation = link_system_->GetResolvedTopologyLinks(links);
    PrepareSceneState(scene_state, uber_struct_system_->Snapshot(), std::move(links),
                      link_topology_generation, link_system_->GetInstanceId(), *root_transform,
                      /*needs_full_rebuild=*/true);
    auto resolved_layers =
        ComputeGlobalResolvedLayers(scene_state.resolved_layer_stacks, scene_state.snapshot.map);
    std::ostringstream output;
    DumpScene(scene_state.snapshot.map, scene_state.topology_data, resolved_layers, output);
    inspector.GetRoot().CreateString(kSceneDump, output.str(), &inspector);
    return fpromise::make_ok_promise(std::move(inspector));
  });

  inspect_frame_results_ = inspect_node_.CreateChild("Frame result counts");
  inspect_direct_display_frame_count_ = inspect_frame_results_.CreateUint("Direct to display", 0);
  inspect_gpu_composition_frame_count_ = inspect_frame_results_.CreateUint("GPU composition", 0);
  inspect_failed_frame_count_ = inspect_frame_results_.CreateUint("Failed", 0);
}

void Engine::RenderScheduledFrame(uint64_t frame_number, zx::time presentation_time,
                                  const FlatlandDisplay* display,
                                  scheduling::FramePresentedCallback callback) {
  utils::CheckIsOnMainThread();

  if (display == nullptr) {
    FX_LOGS(INFO) << "No FlatlandDisplay; skipping render scheduled frame.";
    // In production, `App` returns an empty ViewTree snapshot directly when there is no
    // FlatlandDisplay without calling `GenerateViewTreeSnapshot()`. Clear `scene_state_` so
    // `UpdateLinkWatchersAfterViewTreePublished()` observes an empty scene and the next frame
    // with a display performs a full rebuild.
    scene_state_.Clear();
    SkipRender(frame_number, std::move(callback));
    return;
  }

  // Emit a counter called "ScenicRender" for visualization in the Trace Viewer.
  //
  // This counter is flipped between 0 and 1 and back on each frame, and is
  // used to visually delineate successive frames in the sometimes busy trace
  // view.
  static bool render_edge_flag = false;
  TRACE_COUNTER("gfx", "ScenicRender", 0, "", TA_UINT32(render_edge_flag = !render_edge_flag));
  // The "RenderFrame" duration, its "frame_number" argument, and the "scenic_frame" flow step are
  // read by the trace-processing metrics named below. A frame with no FlatlandDisplay returns
  // before this point so that it is not counted as a rendered frame.
  // LINT.IfChange
  TRACE_DURATION("gfx", "RenderFrame", "frame_number", frame_number, "time",
                 presentation_time.get());
  TRACE_FLOW_STEP("gfx", "scenic_frame", frame_number);
  // LINT.ThenChange(//src/performance/lib/trace_processing/metrics/fps.py,//src/performance/lib/trace_processing/metrics/scenic.py)

  // Copy the links only when their generation moved; otherwise `scene_state_.links` already holds
  // them. A cleared state has `link_topology_generation == 0`, so it always gets a copy.
  const uint64_t known_link_topology_generation = scene_state_.link_topology_generation;
  GlobalTopologyData::LinkTopologyMap links(&link_map_pool_);
  const uint64_t link_topology_generation =
      link_system_->GetResolvedTopologyLinksIfChanged(known_link_topology_generation, links);
  std::optional<GlobalTopologyData::LinkTopologyMap> new_links;
  if (link_topology_generation != known_link_topology_generation) {
    new_links = std::move(links);
  } else {
#ifndef NDEBUG
    // Verify that `LinkSystem` did not mutate its resolved links without advancing its generation.
    // A session thread may resolve or invalidate a link between the two `LinkSystem` calls, so
    // only compare when the second snapshot is still at `link_topology_generation`.
    TRACE_DURATION("gfx", "ValidateLinkTopologyChanges");
    if (link_system_->GetResolvedTopologyLinks(links) == link_topology_generation) {
      FX_DCHECK(links == scene_state_.links);
    }
#endif
  }
  const bool links_changed = link_topology_generation != scene_state_.link_topology_generation;
  const bool uber_structs_dirty = uber_struct_system_->MustRecomputeSceneState();
  // Rebuild when the cache describes no frame, when a session published a transform-graph change,
  // or when the link topology changed; otherwise reuse the cached global transform state.
  const bool needs_full_rebuild = scene_state_.cleared || uber_structs_dirty || links_changed;
  // Records which cause triggered each rebuild, so a trace shows what keeps a frame off the fast
  // path.
  if (needs_full_rebuild) {
    TRACE_INSTANT("gfx", "flatland::Engine::SceneStateRebuild", TRACE_SCOPE_THREAD, "cleared",
                  scene_state_.cleared, "uber_structs_dirty", uber_structs_dirty, "links_changed",
                  links_changed);
  }
  PrepareSceneState(scene_state_, uber_struct_system_->Snapshot(), std::move(new_links),
                    link_topology_generation, link_system_->GetInstanceId(),
                    display->root_transform(), needs_full_rebuild);

  display::Display* const hw_display = display->display();

  if (auto it = seen_display_ids_.find(hw_display->display_id());
      it == seen_display_ids_.end() || !it->second) {
    FLATLAND_VERBOSE_LOG << "Engine::RenderScheduledFrame() frame_number=" << frame_number
                         << " skipped: display not yet added";
    SkipRender(frame_number, std::move(callback));
    return;
  }

  if (flatland_compositor_->IsDisplayDark(hw_display->display_id())) {
    // While the display is dark nothing is rendered or presented to the DisplayCoordinator;
    // `SkipRender()` signals the frame's fences and invokes its callback so that nothing waits on
    // a vsync. `SceneState` has already been prepared above so that the ViewTree and
    // LinkWatchers are still updated properly.
    FLATLAND_VERBOSE_LOG << "Engine::RenderScheduledFrame() frame_number=" << frame_number
                         << " skipped: display is dark";
    SkipRender(frame_number, std::move(callback));
    return;
  }

  // Stack arena so the frame's `ResolvedLayer` list costs no heap allocation per frame; we
  // pre-reserve `kFrameLayerArenaCapacity` so the arena sees one allocation instead of wasting
  // stack buffer space on geometric growth.  Sized for more layers than a typical frame needs;
  // past that the arena falls back to the heap rather than failing.
  // TODO(https://fxbug.dev/570155917): Avoid `-ftrivial-auto-var-init=pattern` overhead on PMR
  // stack buffers.
  constexpr size_t kFrameLayerArenaCapacity = 128;
  alignas(std::max_align_t) std::array<std::byte, kFrameLayerArenaCapacity * sizeof(ResolvedLayer)>
      frame_layer_arena_buffer;
  std::pmr::monotonic_buffer_resource frame_layer_arena(frame_layer_arena_buffer.data(),
                                                        frame_layer_arena_buffer.size());
  std::pmr::vector<ResolvedLayer> resolved_layers(&frame_layer_arena);
  resolved_layers.reserve(kFrameLayerArenaCapacity);
  ComputeGlobalResolvedLayers(resolved_layers, scene_state_.resolved_layer_stacks,
                              scene_state_.snapshot.map);

#ifdef USE_FLATLAND_VERBOSE_LOGGING
  std::ostringstream str;
  str << "Engine::RenderScheduledFrame() frame_number=" << frame_number;
  // Empty until `FlatlandDisplay::SetContent()` publishes the root's `UberStruct`.
  if (!scene_state_.topology_data.topology_vector.empty()) {
    str << "\nRoot transform of global topology: " << scene_state_.topology_data.topology_vector[0];
  }
  str << "\nTopologically-sorted transforms and their corresponding parent transforms:";
  for (size_t i = 1; i < scene_state_.topology_data.topology_vector.size(); ++i) {
    str << "\n        " << scene_state_.topology_data.topology_vector[i] << " -> "
        << scene_state_.topology_data.topology_vector[scene_state_.topology_data.parent_indices[i]];
  }
  str << "\nFrame display-list contains " << resolved_layers.size()
      << " resolved layers (in increasing Z-order):";
  for (const auto& layer : resolved_layers) {
    str << "\n        layer: " << layer;
  }
  FLATLAND_VERBOSE_LOG << str.str();
#endif

  CullLayersInPlace(&resolved_layers, hw_display->width_in_px(), hw_display->height_in_px());

  // Don't render any initial frames if there is no image that could actually be rendered. We do
  // this to avoid triggering any changes in the display until we have content ready to render. We
  // invoke `callback` to continue the render loop.
  if (!first_frame_with_image_is_rendered_) {
    if (resolved_layers.empty()) {
      SkipRender(frame_number, std::move(callback));
      return;
    }
    first_frame_with_image_is_rendered_ = true;
  }

  RenderData render_data = {
      .display_id = hw_display->display_id(),
      .layers = resolved_layers,
  };

  auto fences = flatland_presenter_->TakeFences();
  auto frame_result = flatland_compositor_->RenderFrame(
      frame_number, presentation_time, std::span<const RenderData>(&render_data, 1),
      std::move(fences.release_fences), std::move(fences.release_counters),
      std::move(fences.present_fences), std::move(callback));
  RecordFrameResult(frame_result);
}

void Engine::RecordFrameResult(DisplayCompositor::RenderFrameResult result) {
  switch (result) {
    case DisplayCompositor::RenderFrameResult::kDirectToDisplay:
      inspect_direct_display_frame_count_.Add(1);
      break;
    case DisplayCompositor::RenderFrameResult::kGpuComposition:
      inspect_gpu_composition_frame_count_.Add(1);
      break;
    case DisplayCompositor::RenderFrameResult::kFailure:
      inspect_failed_frame_count_.Add(1);
      break;
  }
}

void Engine::UpdateLinkWatchersAfterViewTreePublished() {
  TRACE_DURATION("gfx", "flatland::Engine::UpdateLinkWatchersAfterViewTreePublished");
  utils::CheckIsOnMainThread();

  link_system_->UpdateLinkWatchers(scene_state_.topology_data.topology_vector,
                                   scene_state_.global_matrices, scene_state_.snapshot.map);
}

view_tree::GeneratedSubtreeSnapshot Engine::GenerateViewTreeSnapshot(
    const TransformHandle& root_transform) {
  TRACE_DURATION("gfx", "flatland::Engine::GenerateViewTreeSnapshot");
  utils::CheckIsOnMainThread();
  FX_DCHECK(scene_state_.topology_data.topology_vector.empty() ||
            scene_state_.topology_data.topology_vector.front() == root_transform);

  // Compare the link generation that `scene_state_` was built at, not the `LinkSystem`'s current
  // one. A link can change on a session thread after `RenderScheduledFrame()`; recording that
  // newer generation here would make the next frame, which rebuilds `scene_state_` with the
  // change, answer "no diff".
  const uint64_t link_topology_generation = scene_state_.link_topology_generation;
  if (!uber_struct_system_->MustRecomputeViewTree() &&
      view_tree_link_topology_generation_ == link_topology_generation) {
    return view_tree::SubtreeSnapshotNoDiff();
  }
  view_tree_link_topology_generation_ = link_topology_generation;

  // Built only when regenerating, which most frames do not.
  GlobalTopologyData::ChildToParentTransformMap link_child_to_parent_transform_map(&link_map_pool_);
  link_system_->GetLinkChildToParentTransformMap(link_child_to_parent_transform_map);

  const auto& uber_struct_snapshot = scene_state_.snapshot;
  const auto& topology_data = scene_state_.topology_data;
  const auto& global_matrices = scene_state_.global_matrices;
  const auto& global_clip_regions = scene_state_.clip_regions;

  auto hit_regions =
      ComputeGlobalHitRegions(topology_data.topology_vector, topology_data.parent_indices,
                              global_matrices, uber_struct_snapshot.map);

  return flatland::GlobalTopologyData::GenerateViewTreeSnapshot(
      topology_data, uber_struct_snapshot.map, std::move(hit_regions), global_clip_regions,
      global_matrices, link_child_to_parent_transform_map);
}

// TODO(https://fxbug.dev/42162342) If we put Screenshot on its own thread, we should make this
// call thread safe.
Renderables Engine::GetRenderables(const FlatlandDisplay& display) {
  utils::CheckIsOnMainThread();

  TransformHandle root = display.root_transform();

  SceneState scene_state(&link_map_pool_);
  GlobalTopologyData::LinkTopologyMap links(&link_map_pool_);
  const uint64_t link_topology_generation = link_system_->GetResolvedTopologyLinks(links);
  PrepareSceneState(scene_state, uber_struct_system_->Snapshot(), std::move(links),
                    link_topology_generation, link_system_->GetInstanceId(), root,
                    /*needs_full_rebuild=*/true);
  const auto hw_display = display.display();

  auto resolved_layers =
      ComputeGlobalResolvedLayers(scene_state.resolved_layer_stacks, scene_state.snapshot.map);

  CullLayersInPlace(&resolved_layers, hw_display->width_in_px(), hw_display->height_in_px());

  flatland_compositor_->EnsureGpuImagesImported(resolved_layers);

  return resolved_layers;
}

void Engine::PrepareSceneState(SceneState& scene_state, UberStructSnapshot snapshot,
                               std::optional<GlobalTopologyData::LinkTopologyMap> links,
                               uint64_t link_topology_generation,
                               TransformHandle::InstanceId link_system_id,
                               TransformHandle root_transform, bool needs_full_rebuild) {
  TRACE_DURATION("gfx", "flatland::Engine::PrepareSceneState", "needs_full_rebuild",
                 needs_full_rebuild);
  // Called by the inspect scene dump as well as the frame path;
  // `link_map_pool_` needs the main thread.
  utils::CheckIsOnMainThread();
  FX_CHECK(links || !scene_state.cleared) << "A cleared scene state holds no links to keep";
  FX_CHECK(links || scene_state.link_topology_generation == link_topology_generation)
      << "Cached links cannot be reused across link topology generations";
  if (!needs_full_rebuild) {
    FX_CHECK(!scene_state.cleared)
        << "Memoization check failed: scene state is cleared when needs_full_rebuild is false";
#ifndef NDEBUG
    const std::optional<std::string> stale_input = FindStaleSceneStateInput(
        scene_state, snapshot, links ? *links : scene_state.links, root_transform);
    FX_DCHECK(!stale_input) << "Memoization check failed with needs_full_rebuild false: "
                            << *stale_input;
#endif
  }

  // No `scene_state.Clear()`: each computation below clears its own output, and the cached
  // `links` must survive when `needs_full_rebuild` is triggered by an `UberStruct` change while
  // the link topology generation is unchanged (`links == std::nullopt`).
  scene_state.snapshot = std::move(snapshot);
  // Move rather than copy: the caller's map and `scene_state.links` normally share
  // `link_map_pool_`, so the pmr move assignment steals the nodes; with different resources it
  // falls back to moving element by element.
  if (links) {
    scene_state.links = std::move(*links);
  }
  scene_state.link_topology_generation = link_topology_generation;

  if (!needs_full_rebuild) {
    return;
  }

  GlobalTopologyData::ComputeGlobalTopologyData(/*output=*/scene_state.topology_data,
                                                scene_state.snapshot.map, scene_state.links,
                                                link_system_id, root_transform);

  ComputeGlobalMatrices(/*output=*/scene_state.global_matrices,
                        scene_state.topology_data.topology_vector,
                        scene_state.topology_data.parent_indices, scene_state.snapshot.map);

  ComputeGlobalTransformClipRegions(
      /*output=*/scene_state.clip_regions, scene_state.topology_data.topology_vector,
      scene_state.topology_data.parent_indices, scene_state.global_matrices,
      scene_state.snapshot.map);

  ComputeGlobalOpacityValues(/*output=*/scene_state.opacities,
                             scene_state.topology_data.topology_vector,
                             scene_state.topology_data.parent_indices, scene_state.snapshot.map);

  ComputeGlobalResolvedLayerStacks(/*output=*/scene_state.resolved_layer_stacks,
                                   scene_state.topology_data, scene_state.snapshot.map,
                                   scene_state.global_matrices, scene_state.clip_regions,
                                   scene_state.opacities);

  ++scene_state.rebuild_count;
  scene_state.cleared = false;
}

std::optional<std::string> Engine::FindStaleSceneStateInput(
    const SceneState& scene_state, const UberStructSnapshot& snapshot,
    const GlobalTopologyData::LinkTopologyMap& links, TransformHandle root_transform) {
  TRACE_DURATION("gfx", "flatland::Engine::FindStaleSceneStateInput");
  if (scene_state.cleared) {
    return "scene state is cleared";
  }
  if (scene_state.links != links) {
    return "links map changed";
  }
  // If `topology_vector` is empty, the previous root session had not yet published an
  // `UberStruct`; the topology only becomes non-empty once `snapshot.map` contains
  // `root_transform`'s session.
  if (scene_state.topology_data.topology_vector.empty()
          ? snapshot.map.contains(root_transform.GetInstanceId())
          : scene_state.topology_data.topology_vector.front() != root_transform) {
    return "root transform changed";
  }
  if (scene_state.snapshot.map.size() != snapshot.map.size()) {
    return "session count changed";
  }
  // Returns true if `handle` is absent from both maps, or present in both with equal values.
  const auto entries_match = [](const auto& old_map, const auto& new_map,
                                const TransformHandle& handle) {
    const auto old_entry = old_map.find(handle);
    const auto new_entry = new_map.find(handle);
    if (old_entry == old_map.end() || new_entry == new_map.end()) {
      return old_entry == old_map.end() && new_entry == new_map.end();
    }
    return old_entry->second == new_entry->second;
  };
  for (const auto& [session_id, new_uber] : snapshot.map) {
    const auto old_it = scene_state.snapshot.map.find(session_id);
    if (old_it == scene_state.snapshot.map.end()) {
      std::ostringstream stale;
      stale << "new session " << session_id << " appeared";
      return stale.str();
    }
    const auto& old_uber = old_it->second;
    if (old_uber.get() == new_uber.get()) {
      continue;
    }
    if (old_uber->local_topology != new_uber->local_topology) {
      std::ostringstream stale;
      stale << "local_topology changed for session " << session_id;
      return stale.str();
    }
    // Compare only entries the topology reaches: `Present()` publishes every entry a session
    // holds, but the transform stage reads them only for handles in `local_topology`, so an
    // entry of an unreachable transform appearing or disappearing is not a change of input.
    for (const auto& entry : new_uber->local_topology) {
      const char* changed = nullptr;
      if (!entries_match(old_uber->local_matrices, new_uber->local_matrices, entry.handle)) {
        changed = "local_matrices";
      } else if (!entries_match(old_uber->local_clip_regions, new_uber->local_clip_regions,
                                entry.handle)) {
        changed = "local_clip_regions";
      } else if (!entries_match(old_uber->local_opacity_values, new_uber->local_opacity_values,
                                entry.handle)) {
        changed = "local_opacity_values";
      }
      if (changed) {
        std::ostringstream stale;
        stale << changed << " changed for session " << session_id << " transform " << entry.handle;
        return stale.str();
      }
    }
  }
  // The stack-hosting set is also cached state: `ComputeGlobalResolvedLayerStacks()` reads
  // only `layer_stacks` membership from the snapshot, so recomputing it from the cached
  // transform vectors and the new snapshot catches a transform that gained or lost a stack,
  // which none of the per-session fields above would show.
  std::vector<ResolvedLayerStack> expected_layer_stacks;
  expected_layer_stacks.reserve(scene_state.resolved_layer_stacks.size());
  ComputeGlobalResolvedLayerStacks(expected_layer_stacks, scene_state.topology_data, snapshot.map,
                                   scene_state.global_matrices, scene_state.clip_regions,
                                   scene_state.opacities);
  if (scene_state.resolved_layer_stacks != expected_layer_stacks) {
    return "set of stack-hosting transforms changed";
  }
  return std::nullopt;
}

void Engine::SceneState::Clear() {
  TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear");
  {
    TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear[snapshot]");
    snapshot.map.clear();
  }
  {
    TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear[links]");
    links.clear();
    link_topology_generation = 0;
  }
  {
    TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear[topology_data]");
    topology_data.Clear();
  }
  {
    TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear[global_matrices]");
    global_matrices.clear();
  }
  {
    TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear[clip_regions]");
    clip_regions.clear();
  }
  {
    TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear[opacities]");
    opacities.clear();
  }
  {
    TRACE_DURATION("gfx", "flatland::Engine::SceneState::Clear[resolved_layer_stacks]");
    resolved_layer_stacks.clear();
  }
  cleared = true;
}

void Engine::SkipRender(uint64_t frame_number, scheduling::FramePresentedCallback callback) {
  TRACE_DURATION("gfx", "flatland::Engine::SkipRender");
  utils::CheckIsOnMainThread();

  auto fences = flatland_presenter_->TakeFences();
  flatland_compositor_->SkipFrame(frame_number, std::move(fences.release_fences),
                                  std::move(fences.release_counters),
                                  std::move(fences.present_fences), std::move(callback));
}

void Engine::AddDisplay(display::Display& display, uint32_t num_vmos) {
  utils::CheckIsOnMainThread();

  auto [it, inserted] = seen_display_ids_.emplace(display.display_id(), false);
  if (!inserted) {
    return;
  }

  // This display has _not_ been added to the DisplayCompositor yet.
  DisplayInfo display_info{
      .dimensions = glm::uvec2{display.width_in_px(), display.height_in_px()},
      .formats = display.pixel_formats(),
      .max_layer_count = display.max_layer_count(),
  };
  fpromise::promise<> promise =
      flatland_compositor_->AddDisplay(&display, display_info, num_vmos).and_then([it] {
        it->second = true;
      });
  executor_.schedule_task(std::move(promise));
}

}  // namespace flatland
