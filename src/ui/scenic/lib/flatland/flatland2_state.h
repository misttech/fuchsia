// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#ifndef SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND2_STATE_H_
#define SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND2_STATE_H_

#include <functional>
#include <ostream>
#include <variant>
#include <vector>

#include "src/ui/scenic/lib/allocation/id.h"
#include "src/ui/scenic/lib/scheduling/id.h"
#include "src/ui/scenic/lib/types/blend_mode.h"
#include "src/ui/scenic/lib/types/rectangle.h"
#include "src/ui/scenic/lib/types/rectangle_f.h"
#include "src/ui/scenic/lib/types/rotate_flip.h"

namespace flatland {

// A globally scoped layer handle which is used to refer to `LayerObject` within the Flatland
// session implementation, as well as `UberStructLayer` within `UberStruct`; the latter is used to
// create the flattened list of `ResolvedLayer` that is displayed each frame.
// TODO(https://fxbug.dev/523371761): update docs when LayerObject and UberStructLayer have been
// unified in step 135.
// TODO(https://fxbug.dev/523371761): TransformHandle/LayerHandle should maybe both be structs, not
// classes.  And what's up with UberStruct being a struct not a class?
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

// TODO(https://fxbug.dev/523371761): undocumented for now; this will be "merged" with
// UberStructLayer in step 135, and fully documented then.
struct LayerObject {
  struct ImageContent {
    types::RectangleF sample_rect;
    types::RotateFlip transform = types::RotateFlip::kIdentity();
    types::Rectangle display_rect;
    float opacity = 1.f;
    types::BlendMode blend_mode = types::BlendMode::kReplace();
    allocation::GlobalImageId bound_image = allocation::kInvalidImageId;
  };

  struct SolidColorContent {
    std::array<float, 4> color = {1.f, 1.f, 1.f, 1.f};
    types::Rectangle display_rect;
    float opacity = 1.f;
  };

  uint64_t epoch = 0;
  int32_t ref_count = 0;
  std::variant<std::monostate, ImageContent, SolidColorContent> content;
};

struct LayerStackData {
  std::vector<LayerHandle> layers;
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

#endif  // SRC_UI_SCENIC_LIB_FLATLAND_FLATLAND2_STATE_H_
