// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_TYPES_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_TYPES_H_

#include <fidl/fuchsia.ui.composition/cpp/fidl.h>
#include <fuchsia/math/cpp/fidl.h>
#include <fuchsia/ui/composition/cpp/fidl.h>
#include <zircon/types.h>

#include <array>
#include <optional>
#include <variant>

#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/scheduling/id.h"
#include "src/ui/scenic/lib/types/blend_mode.h"
#include "src/ui/scenic/lib/types/id_type.h"
#include "src/ui/scenic/lib/types/rectangle.h"
#include "src/ui/scenic/lib/types/rectangle_f.h"
#include "src/ui/scenic/lib/types/rotate_flip.h"
#include "src/ui/scenic/lib/types/view_ref.h"

#include <glm/glm.hpp>

namespace flatland {

class LayerHandle {
 public:
  using InstanceId = scheduling::SessionId;

  LayerHandle() = default;
  LayerHandle(InstanceId instance_id, uint64_t layer_id)
      : instance_id_(instance_id), layer_id_(layer_id) {}

  // Allow copy and move ctors.
  LayerHandle(const LayerHandle& other) = default;
  LayerHandle& operator=(const LayerHandle& other) = default;
  LayerHandle(LayerHandle&& other) = default;
  LayerHandle& operator=(LayerHandle&& other) = default;

  // Default "Spaceship operator" generates all six comparison operators (==, !=, <, <=, >, >=)
  // by comparing each field in the order declared.
  auto operator<=>(const LayerHandle&) const = default;

  InstanceId GetInstanceId() const { return instance_id_; }
  uint64_t GetLayerId() const { return layer_id_; }

 private:
  friend struct std::hash<flatland::LayerHandle>;
  friend std::ostream& operator<<(std::ostream& out, const flatland::LayerHandle& h);

  InstanceId instance_id_ = 0;
  uint64_t layer_id_ = 0;
};

std::ostream& operator<<(std::ostream& out, const LayerHandle& h);

// The sample region to use for an image when texturing a rectangle.
using ImageSampleRegion = types::RectangleF;

// The clip region for a transform to bound its children.
using TransformClipRegion = types::Rectangle;

// Alpha blending mode.
using BlendMode = types::BlendMode;

using ViewRef = types::ViewRef;

// Represents an image rectangle, parameterized by an origin point, an extent representing the width
// and height. The texel UV coordinates specify, in clockwise order, the unnormalized clockwise
// texel coordinates beginning at the top-left coordinate (in texture-space). The orientation
// specifies the rotation applied to the rect. Note that origin and extent are specified in the
// new global coordinate-space (i.e. after all transforms have been applied).
//
// TODO(https://fxbug.dev/446975761): consider replacing `orientation` with a `types::RotateFlip`
// and `origin`/`extent` with a `types::RectangleF`.  This is not completely trivial.
struct ImageRect {
  ImageRect(const glm::vec2& origin, const glm::vec2& extent, const std::array<glm::ivec2, 4> uvs,
            fuchsia::ui::composition::Orientation orientation)
      : origin(origin), extent(extent), texel_uvs(uvs), orientation(orientation) {}

  // Creates an ImageRect with the specified width and height. |texel_uvs| are initialized using the
  // specified |extent| of the rectangle. Note that this may not be equal to the image you are
  // sampling from.
  ImageRect(const glm::vec2& origin, const glm::vec2& extent)
      : origin(origin),
        extent(extent),
        orientation(fuchsia::ui::composition::Orientation::CCW_0_DEGREES) {
    texel_uvs = {glm::vec2(0, 0), glm::vec2(extent.x, 0), glm::vec2(extent.x, extent.y),
                 glm::vec2(0, extent.y)};
  }

  ImageRect() = default;

  glm::vec2 origin = glm::vec2(0, 0);
  glm::vec2 extent = glm::vec2(1, 1);
  std::array<glm::ivec2, 4> texel_uvs = {glm::ivec2(0, 0), glm::ivec2(1, 0), glm::ivec2(1, 1),
                                         glm::ivec2(0, 1)};
  fuchsia::ui::composition::Orientation orientation;

  // Two `ImageRect` are identical if all of the following are true:
  // - orientations are identical
  // - texel_uvs are identical
  // - origins within epsilon-distance
  // - extents within epsilon-distance
  bool operator==(const ImageRect& other) const;
};

std::ostream& operator<<(std::ostream& str, const flatland::ImageRect& r);

// A flexible representation of a flatland hit region.
class HitRegion {
 public:
  // Finite hit region with default interaction.
  explicit HitRegion(const types::RectangleF& region,
                     const fuchsia::ui::composition::HitTestInteraction& interaction =
                         fuchsia::ui::composition::HitTestInteraction::DEFAULT);
  explicit HitRegion(types::RectangleF::ConstructorArgs region,
                     fuchsia::ui::composition::HitTestInteraction interaction =
                         fuchsia::ui::composition::HitTestInteraction::DEFAULT);

  // Infinite hit region with default interaction.
  static HitRegion Infinite(fuchsia::ui::composition::HitTestInteraction interaction =
                                fuchsia::ui::composition::HitTestInteraction::DEFAULT);

  // Return true if region has finite extent.
  bool is_finite() const;

  // Finite region accessor. Caller ensures is_finite() is true.
  const types::RectangleF& region() const;

  // Hit test interaction accessor.
  fuchsia::ui::composition::HitTestInteraction interaction() const;

 private:
  // Helper for Infinite().
  explicit HitRegion(fuchsia::ui::composition::HitTestInteraction interaction);

  // Presence indicates a finite hit region.
  // Absence indicates an infinite hit region.
  std::optional<types::RectangleF> region_;

  fuchsia::ui::composition::HitTestInteraction interaction_ =
      fuchsia::ui::composition::HitTestInteraction::DEFAULT;
};

// Layer instance resolved from the global Flatland scene graph.
struct ResolvedLayer {
  // Reference to a sysmem image bound to a layer.
  struct ImageContent {
    allocation::GlobalImageId image_id = allocation::kInvalidImageId;
    uint32_t width = 0;
    uint32_t height = 0;
    bool operator==(const ImageContent&) const = default;
  };

  // A solid-color fill.  Replaces the kInvalidImageId sentinel encoding.
  struct SolidColorContent {
    std::array<float, 4> color = {1.f, 1.f, 1.f, 1.f};
    bool operator==(const SolidColorContent&) const = default;
  };

  ImageRect rect;
  std::array<float, 4> multiply_color = {1.f, 1.f, 1.f, 1.f};  // multiply color
  types::BlendMode blend_mode = types::BlendMode::kReplace();
  fuchsia_ui_composition::ImageFlip flip = fuchsia_ui_composition::ImageFlip::kNone;
  std::variant<ImageContent, SolidColorContent> content;

  // Sentinel value representing an unset or invalid topology index (primarily for unit tests).
  static constexpr int32_t kInvalidTopologyIndex = -1;

  // Index of the Transform node in the global topology vector that produced this layer.
  // Used for debug dumps and (eventually, maybe) cross-frame layer identity tracking.
  int32_t topology_index = kInvalidTopologyIndex;

  bool operator==(const ResolvedLayer&) const = default;
};

}  // namespace flatland

namespace std {

template <>
struct hash<flatland::LayerHandle> {
  size_t operator()(const flatland::LayerHandle& h) const noexcept {
    return hash<flatland::LayerHandle::InstanceId>{}(h.instance_id_) ^
           hash<uint64_t>{}(h.layer_id_);
  }
};

}  // namespace std

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND_TYPES_H_
