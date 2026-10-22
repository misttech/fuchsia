// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_GLOBAL_RESOLVED_LAYERS_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_GLOBAL_RESOLVED_LAYERS_H_

#include <cstdint>
#include <memory_resource>
#include <span>
#include <vector>

#include "src/ui/scenic/lib/allocation/image_metadata.h"
#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/global_matrix_data.h"
#include "src/ui/scenic/lib/flatland/global_topology_data.h"
#include "src/ui/scenic/lib/flatland/uber_struct.h"

#include <glm/mat3x3.hpp>

namespace flatland {

// The list of global opacity values for a particular global topology.  Each entry is the
// global opacity value (i.e. relative to the root TransformHandle) of the transform in the
// corresponding position of the `topology_vector` supplied to `ComputeGlobalOpacityValues()`.
using GlobalOpacityVector = std::vector<float>;

// Computes a list of global opacity values for the global topology.
GlobalOpacityVector ComputeGlobalOpacityValues(
    const GlobalTopologyData::TopologyVector& global_topology,
    const GlobalTopologyData::ParentIndexVector& parent_indices,
    const UberStruct::InstanceMap& uber_structs);
void ComputeGlobalOpacityValues(GlobalOpacityVector& output,
                                const GlobalTopologyData::TopologyVector& global_topology,
                                const GlobalTopologyData::ParentIndexVector& parent_indices,
                                const UberStruct::InstanceMap& uber_structs);

// Captures all global transform state that feeds into resolving the layers of a single
// stack-hosting node in the global topology.
//
// This struct definition is the contract between the transform stage and the layer stage:
// `ComputeGlobalResolvedLayers()` depends on the global scene graph solely through a slice of
// `ResolvedLayerStack` entries plus the fresh `UberStruct::InstanceMap` snapshot.
struct ResolvedLayerStack {
  // The transform handle hosting this layer stack; used to look up the session's `UberStruct`
  // and `layer_stacks` entry in the fresh snapshot.
  TransformHandle handle;

  // Index of `handle` in `GlobalTopologyData::topology_vector`, stamped into each emitted
  // `ResolvedLayer`.
  int32_t topology_index = ResolvedLayer::kInvalidTopologyIndex;

  // The pure-rotation `types::RotateFlip` decoded from `global_matrix`.
  //
  // Why rotation-only (no flip is cached here): a transform node's global matrix can never
  // contain a reflection. Transform matrices are built exclusively from `SetTranslation`,
  // `SetOrientation` (quarter-turn rotations only), and `SetScale` (which rejects non-positive
  // scale at the FIDL boundary), and viewport link scales are likewise positive. Reflections
  // exist only per-layer (`SetImageFlip` in Flatland1, `LayerProperties::transform` in
  // Flatland2) and ride in the layer's own `types::RotateFlip`. Consequently, a pure-rotation
  // `types::RotateFlip` captures the node's entire non-translation/scale orientation, and
  // satisfies `RotateFlip::RotatedBy()`'s pure-rotation precondition by construction.
  types::RotateFlip node_rotation = types::RotateFlip::kIdentity();

  // Copy of the hosting node's global transform matrix.
  glm::mat3 global_matrix{1.f};

  // Copy of the hosting node's global clip region.
  TransformClipRegion clip_region = kUnclippedRegion;

  // Copy of the hosting node's accumulated inherited opacity from `GlobalOpacityVector`.
  float opacity = 1.f;

  bool operator==(const ResolvedLayerStack&) const = default;
};

// Transform stage: builds the topologically sorted `ResolvedLayerStack` list,
// one entry per stack-hosting node in `topology`, decoding each node's rotation
// into `node_rotation` once.
void ComputeGlobalResolvedLayerStacks(std::vector<ResolvedLayerStack>& output,
                                      const GlobalTopologyData& topology,
                                      const UberStruct::InstanceMap& snapshot,
                                      const GlobalMatrixVector& global_matrices,
                                      const GlobalTransformClipRegionVector& clip_regions,
                                      const GlobalOpacityVector& inherited_opacities);

inline std::vector<ResolvedLayerStack> ComputeGlobalResolvedLayerStacks(
    const GlobalTopologyData& topology, const UberStruct::InstanceMap& snapshot,
    const GlobalMatrixVector& global_matrices, const GlobalTransformClipRegionVector& clip_regions,
    const GlobalOpacityVector& inherited_opacities) {
  std::vector<ResolvedLayerStack> output;
  ComputeGlobalResolvedLayerStacks(output, topology, snapshot, global_matrices, clip_regions,
                                   inherited_opacities);
  return output;
}

// Layer stage: computes the resolved layers list from `layer_stacks` (transform stage output) and
// the fresh `snapshot`. For each entry, looks up the stack's current layers in `snapshot` and
// emits one `ResolvedLayer` per visible stack layer via closed-form composition.
//
// The `std::pmr::vector` overload is used by the per-frame render hot path over a stack arena (the
// caller pre-reserves `output` to avoid geometric-growth waste in a `PmrStackArena`);
// the `std::vector` overload is retained for non-hot-path callers (e.g. `Engine::GetRenderables()`,
// Inspect scene dumps, and unit tests).
void ComputeGlobalResolvedLayers(std::pmr::vector<ResolvedLayer>& output,
                                 std::span<const ResolvedLayerStack> layer_stacks,
                                 const UberStruct::InstanceMap& snapshot);
void ComputeGlobalResolvedLayers(std::vector<ResolvedLayer>& output,
                                 std::span<const ResolvedLayerStack> layer_stacks,
                                 const UberStruct::InstanceMap& snapshot);

inline std::vector<ResolvedLayer> ComputeGlobalResolvedLayers(
    std::span<const ResolvedLayerStack> layer_stacks, const UberStruct::InstanceMap& snapshot) {
  std::vector<ResolvedLayer> output;
  ComputeGlobalResolvedLayers(output, layer_stacks, snapshot);
  return output;
}

// Convenience overload that runs `ComputeGlobalResolvedLayerStacks()` followed by the layer stage
// `ComputeGlobalResolvedLayers()`.
void ComputeGlobalResolvedLayers(std::vector<ResolvedLayer>& output,
                                 const GlobalTopologyData& topology,
                                 const UberStruct::InstanceMap& snapshot,
                                 const GlobalMatrixVector& global_matrices,
                                 const GlobalTransformClipRegionVector& clip_regions,
                                 const GlobalOpacityVector& inherited_opacities);

// Helper which returns a new vector instead of taking the output vector as an argument.
inline std::vector<ResolvedLayer> ComputeGlobalResolvedLayers(
    const GlobalTopologyData& topology, const UberStruct::InstanceMap& snapshot,
    const GlobalMatrixVector& global_matrices, const GlobalTransformClipRegionVector& clip_regions,
    const GlobalOpacityVector& inherited_opacities) {
  std::vector<ResolvedLayer> output;
  ComputeGlobalResolvedLayers(output, topology, snapshot, global_matrices, clip_regions,
                              inherited_opacities);
  return output;
}

// Simple culling algorithm that checks if any of the input rectangles cover the entire display,
// and if so, culls all rectangles that came before them (since rectangles are implicitly sorted
// according to depth, with the first entry being the furthest back, this has the effect of
// eliminating all rectangles behind the full-screen one). Also culls any rectangle that has
// no size (width is zero, or height is zero).
//
// The `std::pmr::vector` overload is used by the per-frame render hot path; the `std::vector`
// overload is retained for non-hot-path callers (e.g. `Engine::GetRenderables()` and unit tests).
void CullLayersInPlace(std::pmr::vector<ResolvedLayer>* layers_in_out, uint64_t display_width,
                       uint64_t display_height);
void CullLayersInPlace(std::vector<ResolvedLayer>* layers_in_out, uint64_t display_width,
                       uint64_t display_height);

// Exposed for testing. Return type for `ResolveBlendAndOpacity()` helper.
struct ResolvedBlend {
  types::BlendMode blend_mode;
  std::array<float, 4> multiply_color;
};

// Exposed for testing. Encapsulates the difference between how Flatland1 and Flatland2 APIs treat
// REPLACE blend mode when `opacity < 1`; `pin_replace` is the selector for this differing behavior.
//
// In Flatland1 *for images only*, RGB is scaled and blend stays REPLACE (fade toward "black").
//
// Flatland2, and Flatland1 for solid color fills, "demote" REPLACE to PREMULTIPLIED_ALPHA,
// so there is no visual discontinuity at `opacity == 0` (where the layer is treated as invisible);
// this allows e.g. a window manager to fade out a child app even if it uses REPLACE.
//
// NOTE: `effective_opacity` combines layer opacity with inherited transform opacity.  It does not
// involve "content opacity", neither the alpha of a solid color fill, nor the alpha channel of
// image pixels.
// TODO(https://fxbug.dev/523371761): ratified DESIGN-blend_mode_and_opacity
// decision must match the behavior implemented here.
ResolvedBlend ResolveBlendAndOpacity(types::BlendMode stored_blend, float effective_opacity,
                                     bool pin_replace);

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_GLOBAL_RESOLVED_LAYERS_H_
