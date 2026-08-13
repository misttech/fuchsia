// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_SESSION_TYPES_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_SESSION_TYPES_H_

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>

#include <vector>

#include "src/ui/scenic/lib/flatland/flatland_types.h"
#include "src/ui/scenic/lib/flatland/uber_struct.h"
#include "src/ui/scenic/lib/types/id_type.h"

namespace flatland {

namespace internal {
using ContentIdTraits =
    types::DefaultIdTypeTraitsForNaturalFidl<uint64_t, fuchsia_ui_composition::ContentId>;
using TransformIdTraits =
    types::DefaultIdTypeTraitsForNaturalFidl<uint64_t, fuchsia_ui_composition::TransformId>;
}  // namespace internal

using ContentId = types::IdType<::flatland::internal::ContentIdTraits>;
using TransformId = types::IdType<::flatland::internal::TransformIdTraits>;

constexpr ContentId kInvalidContentId = ContentId(0);
constexpr TransformId kInvalidTransformId = TransformId(0);

// The session-side state of a single Flatland2 layer.
//
// Unlike the snapshot type (`UberStructLayer`), which carries only the active composition
// mode's properties, a `LayerObject` holds the properties of every mode simultaneously,
// plus a `mode` discriminant saying which of them is in effect.  This is deliberate:
// layer properties are sticky.  Switching a layer from image mode to solid-color mode and
// back loses nothing; `image_mode` keeps its values while `solid_color_mode` is in
// effect, and vice versa.  The shape mirrors the Flatland2 FIDL API (a single
// `LayerProperties` table holding the union of every mode's fields) and the Composer3
// HAL's `LayerCommand`, whose property bag likewise persists across composition-type
// changes.
//
// Which fields are in effect is determined by `mode` alone:
//   - `common` is always in effect.
//   - `image_mode` is in effect only when `mode == kImage`.
//   - `solid_color_mode` is in effect only when `mode == kSolidColor`.
//   - a `kInvisible` layer has no content: it keeps its place in any layer stack that
//     references it, but contributes nothing to the frame.  (This is a property of the
//     layer itself; a layer in any mode can also contribute nothing via a zero-sized
//     `common.display_rect` or zero opacity.)
//
// At `Present()`, `common` and the active mode's properties are copied into an
// `UberStructLayer`; inactive-mode properties and session-lifetime state (`ref_count`)
// stay behind.
struct LayerObject {
  enum class Mode : uint8_t { kInvisible, kImage, kSolidColor };

  UberStructLayer::CommonProperties common;
  UberStructLayer::ImageModeProperties image_mode;
  UberStructLayer::SolidColorModeProperties solid_color_mode;
  Mode mode = Mode::kInvisible;

  // The number of references to this layer: one for the client's LayerId binding
  // (Flatland2 sessions; the facade holds none) and one per layer-stack membership.
  // Maintained manually by the few ref/unref sites (`CreateLayer()`,
  // `CreateLayerStackData()`, `ReleaseLayerObject()`, stack teardown); the layer is
  // destroyed when it reaches zero.
  int32_t ref_count = 0;
};

struct LayerStackData {
  std::vector<LayerHandle> layers;
};

}  // namespace flatland

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_SESSION_TYPES_H_
